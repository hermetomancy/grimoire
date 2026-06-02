use anyhow::{Context, Result, bail};
use std::{
    fs::{self, File},
    io::Cursor,
    path::{Path, PathBuf},
};

use crate::{
    archive,
    nu::{
        nuon_io,
        runtime::{EmbeddedNuRuntime, RuneRuntime},
    },
    paths,
    progress::{status, success},
};

pub fn pack_built_rune(
    rune: &Path,
    package_dir: &Path,
    output: &Path,
    quiet: bool,
) -> Result<PathBuf> {
    status(
        quiet,
        &format!("reading rune metadata ({})", rune.display()),
    );
    let runtime = EmbeddedNuRuntime;
    let metadata = runtime.package_metadata(rune)?;
    let target = paths::target_triple();
    let archive_name = format!("{}-{}-{target}.tar.zst", metadata.name, metadata.version);
    let archive_path = output.join(archive_name);
    fs::create_dir_all(output)?;

    status(
        quiet,
        &format!(
            "staging package metadata for {} {}",
            metadata.name, metadata.version
        ),
    );
    let package_nuon = nuon_io::to_nuon_string(&metadata.archive_value(&target))?;
    let rune_source =
        fs::read(rune).with_context(|| format!("read rune source {}", rune.display()))?;

    status(
        quiet,
        &format!(
            "compressing archive ({})",
            archive_path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("package.tar.zst")
        ),
    );

    let file = File::create(&archive_path)?;
    let encoder = zstd::stream::write::Encoder::new(file, 0)?;
    let mut tar = tar::Builder::new(encoder);
    append_package_dir(&mut tar, package_dir)?;
    append_bytes(&mut tar, ".grimoire/package.nuon", package_nuon.as_bytes())?;
    append_bytes(&mut tar, ".grimoire/rune.rn", &rune_source)?;
    let encoder = tar.into_inner()?;
    encoder.finish()?;

    success(quiet, &format!("wrote {}", archive_path.display()));
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
            tar.append_dir(relative, path)?;
        } else if metadata.is_file() {
            tar.append_path_with_name(path, relative)?;
        } else {
            bail!(
                "package output contains unsupported file {}",
                path.display()
            );
        }
    }
    Ok(())
}

fn append_bytes<W: std::io::Write>(
    tar: &mut tar::Builder<W>,
    path: &str,
    bytes: &[u8],
) -> Result<()> {
    let mut header = tar::Header::new_gnu();
    header.set_size(bytes.len() as u64);
    header.set_mode(0o644);
    header.set_cksum();
    tar.append_data(&mut header, path, Cursor::new(bytes))?;
    Ok(())
}
