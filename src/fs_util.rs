//! Small filesystem utilities shared across modules.

use anyhow::{Context, Result, bail};
use std::{fs, path::Path};

/// Recursively copies `source` into `destination`, preserving directory structure.
/// Symlinks are rejected with a message that includes `label` (e.g. "addendum" or "tome").
pub fn copy_dir_all(source: &Path, destination: &Path, label: &str) -> Result<()> {
    fs::create_dir_all(destination)?;
    for entry in walkdir::WalkDir::new(source).sort_by_file_name() {
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
