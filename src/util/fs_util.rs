//! Small filesystem utilities shared across modules.

use anyhow::{Context, Result, bail};
use std::{fs, io::Write, path::Path};

/// Flushes a directory's own contents to disk so that a `rename`, create, or remove of an entry
/// inside it survives a crash. POSIX `rename` is atomic with respect to concurrent readers but is
/// not *durable* until the containing directory is fsync'd: after a power loss the rename may be
/// lost or, worse, the new name may point at a file whose data never reached disk. State
/// promotion pairs this with an fsync of the file data before the rename (AGENTS.md §9).
///
/// The directory fsync is best-effort: some filesystems reject `fsync` on a directory descriptor
/// with `EINVAL`/`ENOTSUP`, which is not a correctness failure here, so only failing to *open* the
/// directory is reported.
pub fn fsync_dir(dir: &Path) -> Result<()> {
    let file = fs::File::open(dir).with_context(|| format!("open {} to fsync", dir.display()))?;
    // WHY ignored: a directory that cannot be fsync'd (unsupported on the filesystem) still leaves
    // the rename visible; we lose only the durability upgrade, never consistency.
    let _ = file.sync_all();
    Ok(())
}

/// Writes bytes through a fsync'd temporary file and atomic rename.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;
    let mut temp = tempfile::Builder::new()
        .prefix(".grimoire-write-")
        .tempfile_in(parent)?;
    temp.write_all(bytes)?;
    temp.flush()?;
    temp.as_file()
        .sync_all()
        .with_context(|| format!("fsync staged file for {}", path.display()))?;
    temp.persist(path)
        .map_err(|err| anyhow::anyhow!(err.to_string()))?;
    fsync_dir(parent)?;
    Ok(())
}

/// Recursively copies `source` into `destination`, preserving directory structure.
/// Symlinks are rejected with a message that includes `label` (e.g. "addendum" or "tome").
pub fn copy_dir_all(source: &Path, destination: &Path, label: &str) -> Result<()> {
    fs::create_dir_all(destination)?;
    // `.git` is excluded: a checked-out submodule's `.git` is a pointer *file* whose
    // relative gitdir breaks when copied, and a cache copy is not a repository anyway —
    // `head_commit` on a local catalog cache must consistently see "no repo".
    for entry in walkdir::WalkDir::new(source)
        .sort_by_file_name()
        .into_iter()
        .filter_entry(|e| e.file_name() != ".git")
    {
        let entry = entry?;
        let path = entry.path();
        if path == source {
            continue;
        }
        let relative = path
            .strip_prefix(source)
            .with_context(|| format!("strip source prefix from {}", path.display()))?;
        let target = destination.join(relative);
        let metadata = fs::symlink_metadata(path)?;
        if metadata.file_type().is_symlink() {
            bail!("{label} source contains symlink {}", path.display());
        }
        if metadata.is_dir() {
            fs::create_dir_all(&target)?;
        } else if metadata.is_file() {
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::copy(path, &target)?;
        }
    }
    Ok(())
}
