//! Fetching and extracting declared sources into the build context, with per-format
//! (`.tar.zst`/`.tar.gz`/`.tar.xz`) readers and the same entry validation installs use.

use anyhow::{Context, Result};
use flate2::read::GzDecoder;
use std::{collections::BTreeMap, fs, fs::File, io::Read, path::Path};
use xz2::read::XzDecoder;

use crate::{archive, fetch::FetchedSource};

pub(super) fn prepare_sources(
    sources: BTreeMap<String, FetchedSource>,
    work_dir: &Path,
) -> Result<BTreeMap<String, FetchedSource>> {
    let sources_dir = work_dir.join("sources");
    let mut prepared = BTreeMap::new();
    for (name, mut source) in sources {
        if let Some(kind) = source_archive_kind(&source.url) {
            let destination = sources_dir.join(&name);
            fs::create_dir_all(&destination)?;
            extract_source_archive(&source.path, &destination, kind)
                .with_context(|| format!("extract source `{name}`"))?;
            source.extracted_dir = Some(destination);
        }
        prepared.insert(name, source);
    }
    Ok(prepared)
}

#[derive(Debug, Clone, Copy)]
#[allow(clippy::enum_variant_names)] // all source archives are tarballs; the prefix is meaningful
pub(super) enum SourceArchiveKind {
    TarGz,
    TarXz,
    TarZst,
}

pub(super) fn source_archive_kind(url: &str) -> Option<SourceArchiveKind> {
    let normalized = url
        .split(['?', '#'])
        .next()
        .unwrap_or(url)
        .to_ascii_lowercase();
    if normalized.ends_with(".tar.zst") || normalized.ends_with(".tzst") {
        return Some(SourceArchiveKind::TarZst);
    }
    if normalized.ends_with(".tar.gz") || normalized.ends_with(".tgz") {
        return Some(SourceArchiveKind::TarGz);
    }
    if normalized.ends_with(".tar.xz") || normalized.ends_with(".txz") {
        return Some(SourceArchiveKind::TarXz);
    }
    None
}

pub(super) fn extract_source_archive(
    path: &Path,
    destination: &Path,
    kind: SourceArchiveKind,
) -> Result<()> {
    // Copy into the private build directory before reading so a local attacker cannot swap
    // the shared cache file between validation and extraction (AGENTS.md §10).
    let safe = destination.with_extension("grimoire-tmp");
    fs::copy(path, &safe)
        .with_context(|| format!("copy source archive to temp {}", safe.display()))?;
    let result = extract_source_archive_inner(&safe, destination, kind);
    let _ = fs::remove_file(&safe);
    result
}

pub(super) fn extract_source_archive_inner(
    path: &Path,
    destination: &Path,
    kind: SourceArchiveKind,
) -> Result<()> {
    let mut tar = tar::Archive::new(source_archive_reader(path, kind)?);
    archive::validate_tar_entries(&mut tar)
        .with_context(|| format!("validate source archive {}", path.display()))?;

    let mut tar = tar::Archive::new(source_archive_reader(path, kind)?);
    tar.unpack(destination)
        .with_context(|| format!("unpack source archive into {}", destination.display()))?;
    Ok(())
}

pub(super) fn source_archive_reader(path: &Path, kind: SourceArchiveKind) -> Result<Box<dyn Read>> {
    let file =
        File::open(path).with_context(|| format!("open source archive {}", path.display()))?;
    match kind {
        SourceArchiveKind::TarGz => Ok(Box::new(GzDecoder::new(file))),
        SourceArchiveKind::TarXz => Ok(Box::new(XzDecoder::new(file))),
        SourceArchiveKind::TarZst => Ok(Box::new(
            zstd::stream::read::Decoder::new(file)
                .with_context(|| format!("decode zstd source archive {}", path.display()))?,
        )),
    }
}
