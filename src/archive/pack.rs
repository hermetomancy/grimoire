//! Packing a built package directory into a `.tar.zst` archive with embedded `.grimoire`
//! metadata. This is the output format of a source build and is byte-for-byte the same shape a
//! prebuilt download has, so the install path does not care how a package was produced.

use anyhow::{Context, Result, bail};
use std::{
    fs::{self, File},
    io::{Cursor, Read},
    path::{Path, PathBuf},
};

#[cfg(unix)]
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
    output: &Path,
) -> Result<PathBuf> {
    let target = paths::target_triple();
    let archive_name = format!("{}-{}-{target}.tar.zst", metadata.name, metadata.version);
    let archive_path = output.join(archive_name);
    fs::create_dir_all(output)?;

    status(&format!(
        "staging package metadata for {} {}",
        metadata.name, metadata.version
    ));
    let package_nuon = nuon_io::to_nuon_string(&metadata.archive_value(&target))?;
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
    append_package_dir(&mut tar, package_dir)?;
    append_bytes(&mut tar, ".grimoire/package.nuon", package_nuon.as_bytes())?;
    append_bytes(&mut tar, ".grimoire/rune.rn", &rune_source)?;
    let encoder = tar.into_inner()?;
    encoder.finish()?;

    success(&format!("wrote {}", archive_path.display()));
    Ok(archive_path)
}

fn append_package_dir<W: std::io::Write>(
    tar: &mut tar::Builder<W>,
    package_dir: &Path,
) -> Result<()> {
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
            bail!("package output contains symlink {}", path.display());
        }

        if metadata.is_dir() {
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

#[cfg(unix)]
fn file_mode(metadata: &fs::Metadata) -> u32 {
    metadata.permissions().mode() & 0o777
}

#[cfg(not(unix))]
fn file_mode(_metadata: &fs::Metadata) -> u32 {
    0o755
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

fn set_deterministic_metadata(header: &mut tar::Header) {
    header.set_mtime(0);
    header.set_uid(0);
    header.set_gid(0);
    header.set_username("").ok();
    header.set_groupname("").ok();
    header.set_cksum();
}
