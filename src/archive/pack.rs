//! Packing a built package directory into a `.tar.zst` archive with embedded `.grimoire`
//! metadata. This is the output format of a source build and is byte-for-byte the same shape a
//! prebuilt download has, so the install path does not care how a package was produced.

use anyhow::{Context, Result, bail};
use std::{
    fs::{self, File},
    io::{Cursor, Read},
    path::{Path, PathBuf},
};

use std::os::unix::fs::PermissionsExt;

use crate::{
    archive,
    model::PackageMetadata,
    nu::nuon_io,
    paths,
    progress::{status, success},
};

pub fn pack_built_rune(
    rune: &Path,
    metadata: &PackageMetadata,
    package_dir: &Path,
    final_prefix: &Path,
    store_hash: &str,
    output: &Path,
    target: &str,
) -> Result<PathBuf> {
    let archive_name = format!("{}-{}-{target}.tar.zst", metadata.name, metadata.version);
    let archive_path = output.join(archive_name);
    fs::create_dir_all(output)?;

    status(&format!(
        "staging package metadata for {} {}",
        metadata.name, metadata.version
    ));
    let store_relative = paths::store_relative_dir(store_hash, &metadata.name, &metadata.version);
    let package_nuon =
        nuon_io::to_nuon_string(&metadata.archive_value(target, Some(&store_relative)))?;
    let rune_source =
        fs::read(rune).with_context(|| format!("read rune source {}", rune.display()))?;

    status(&format!(
        "compressing archive ({})",
        archive_path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("package.tar.zst")
    ));

    let file = File::create(&archive_path)?;
    let encoder = zstd::stream::write::Encoder::new(file, 0)?;
    let mut tar = tar::Builder::new(encoder);
    append_package_dir(&mut tar, package_payload_dir(package_dir, final_prefix))?;
    append_bytes(&mut tar, ".grimoire/package.nuon", package_nuon.as_bytes())?;
    append_bytes(&mut tar, ".grimoire/rune.rn", &rune_source)?;
    let encoder = tar.into_inner()?;
    encoder.finish()?;

    success(&format!("wrote {}", archive_path.display()));
    Ok(archive_path)
}

fn package_payload_dir<'a>(package_dir: &'a Path, final_prefix: &Path) -> PathBufOrBorrowed<'a> {
    let destdir_payload = package_dir.join(relative_destdir_prefix(final_prefix));
    if destdir_payload.exists() {
        PathBufOrBorrowed::Owned(destdir_payload)
    } else {
        PathBufOrBorrowed::Borrowed(package_dir)
    }
}

enum PathBufOrBorrowed<'a> {
    Borrowed(&'a Path),
    Owned(PathBuf),
}

impl AsRef<Path> for PathBufOrBorrowed<'_> {
    fn as_ref(&self) -> &Path {
        match self {
            Self::Borrowed(path) => path,
            Self::Owned(path) => path.as_path(),
        }
    }
}

fn relative_destdir_prefix(prefix: &Path) -> PathBuf {
    let mut relative = PathBuf::new();
    for component in prefix.components() {
        if let std::path::Component::Normal(part) = component {
            relative.push(part);
        }
    }
    relative
}

fn append_package_dir<W: std::io::Write>(
    tar: &mut tar::Builder<W>,
    package_dir: impl AsRef<Path>,
) -> Result<()> {
    let package_dir = package_dir.as_ref();
    for entry in walkdir::WalkDir::new(package_dir).sort_by_file_name() {
        let entry = entry?;
        let path = entry.path();
        if path == package_dir {
            continue;
        }

        let relative = path
            .strip_prefix(package_dir)
            .with_context(|| format!("strip package dir prefix from {}", path.display()))?;
        if !archive::validate_archive_member_path(relative) {
            bail!("package output contains unsafe path {}", relative.display());
        }

        let metadata = fs::symlink_metadata(path)?;
        if metadata.file_type().is_symlink() {
            append_symlink(tar, path, relative)?;
        } else if metadata.is_dir() {
            append_dir(tar, relative)?;
        } else if metadata.is_file() {
            append_file(tar, path, relative, file_mode(&metadata))?;
        } else {
            bail!(
                "package output contains unsupported file {}",
                path.display()
            );
        }
    }
    Ok(())
}

/// Appends a symlink member, preserving it as a symlink rather than dereferencing it. The target
/// must resolve within the package (validated here) so the produced archive is self-contained and
/// safe to extract; an escaping or absolute target is a hard error in the build output.
fn append_symlink<W: std::io::Write>(
    tar: &mut tar::Builder<W>,
    source: &Path,
    relative: &Path,
) -> Result<()> {
    let target =
        fs::read_link(source).with_context(|| format!("read symlink {}", source.display()))?;
    if !archive::validate_symlink_target(relative, &target) {
        bail!(
            "package output contains symlink {} with a target that escapes the package: {}",
            relative.display(),
            target.display()
        );
    }
    let mut header = tar::Header::new_gnu();
    header.set_entry_type(tar::EntryType::Symlink);
    header.set_size(0);
    header.set_mode(0o777);
    set_deterministic_metadata(&mut header);
    tar.append_link(&mut header, relative, target.as_path())
        .with_context(|| format!("append symlink {}", relative.display()))?;
    Ok(())
}

fn append_dir<W: std::io::Write>(tar: &mut tar::Builder<W>, path: &Path) -> Result<()> {
    let mut header = tar::Header::new_gnu();
    header.set_entry_type(tar::EntryType::Directory);
    header.set_size(0);
    header.set_mode(0o755);
    set_deterministic_metadata(&mut header);
    tar.append_data(&mut header, path, Cursor::new([]))?;
    Ok(())
}

fn append_file<W: std::io::Write>(
    tar: &mut tar::Builder<W>,
    source: &Path,
    path: &Path,
    mode: u32,
) -> Result<()> {
    let mut bytes = Vec::new();
    File::open(source)
        .with_context(|| format!("read package output {}", source.display()))?
        .read_to_end(&mut bytes)?;
    let mut header = tar::Header::new_gnu();
    header.set_size(bytes.len() as u64);
    header.set_mode(mode);
    header.set_entry_type(tar::EntryType::Regular);
    set_deterministic_metadata(&mut header);
    tar.append_data(&mut header, path, Cursor::new(bytes))?;
    Ok(())
}

fn file_mode(metadata: &fs::Metadata) -> u32 {
    let mode = metadata.permissions().mode();
    if mode & 0o111 != 0 { 0o755 } else { 0o644 }
}

fn append_bytes<W: std::io::Write>(
    tar: &mut tar::Builder<W>,
    path: &str,
    bytes: &[u8],
) -> Result<()> {
    let mut header = tar::Header::new_gnu();
    header.set_size(bytes.len() as u64);
    header.set_mode(0o644);
    set_deterministic_metadata(&mut header);
    tar.append_data(&mut header, path, Cursor::new(bytes))?;
    Ok(())
}

/// Normalises archive metadata so the same package built twice produces byte-for-byte
/// identical archives. Timestamps, UIDs, and GIDs are fixed because they vary by host
/// and would otherwise change the archive hash even when file contents are unchanged.
/// The timestamp is 2001-04-25 00:00:00 UTC — the release date of Le Fabuleux Destin
/// d'Amélie Poulain, because reproducibility need not be joyless.
fn set_deterministic_metadata(header: &mut tar::Header) {
    header.set_mtime(988153200);
    header.set_uid(0);
    header.set_gid(0);
    header.set_username("").ok();
    header.set_groupname("").ok();
    header.set_cksum();
}
