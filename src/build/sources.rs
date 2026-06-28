//! Fetching and extracting declared sources into the build context, with per-format
//! (`.tar.zst`/`.tar.gz`/`.tar.xz`) readers and the same entry validation installs use.

use anyhow::{Context, Result};
use bzip2::read::MultiBzDecoder;
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
        // Extension first; for extensionless URLs (codeload commit tarballs), sniff the
        // fetched bytes — the URL carries no filename to judge by.
        let kind = source_archive_kind(&source.url).or_else(|| {
            url_is_extensionless(&source.url)
                .then(|| source_archive_kind_sniffed(&source.path))
                .flatten()
        });
        if let Some(kind) = kind {
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
    TarBz2,
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
    if normalized.ends_with(".tar.bz2") || normalized.ends_with(".tbz2") {
        return Some(SourceArchiveKind::TarBz2);
    }
    // codeload / GitHub archive form encodes the container as a path segment before the git ref
    // (`.../tar.gz/<ref>`). A dotted ref — a tag like `0.9.0` — defeats both the suffix check above
    // and the extensionless-sniff fallback in `prepare_sources` (which only fires when the final
    // segment has no `.`), so the archive would never extract. Recognize the segment directly.
    if normalized.contains("/tar.gz/") {
        return Some(SourceArchiveKind::TarGz);
    }
    None
}

/// Whether a URL names no file extension at all (final path segment has no `.`), like
/// codeload.github.com commit tarballs (`.../tar.gz/<sha>`). Only such URLs are eligible
/// for content sniffing — a URL with *any* extension (`.patch.gz`, `.txt`) keeps its
/// extension-derived meaning, so a compressed-but-not-tar artifact never extracts.
pub(super) fn url_is_extensionless(url: &str) -> bool {
    let normalized = url.split(['?', '#']).next().unwrap_or(url);
    normalized
        .rsplit('/')
        .next()
        .is_some_and(|segment| !segment.contains('.'))
}

/// Detects the tarball container by magic bytes for URLs that name no extension. The tar
/// layer is assumed, like everywhere else in source handling; a sniffed container that is
/// not a tarball fails extraction loudly rather than passing through silently.
pub(super) fn source_archive_kind_sniffed(path: &Path) -> Option<SourceArchiveKind> {
    let mut magic = [0u8; 6];
    let mut file = fs::File::open(path).ok()?;
    use std::io::Read;
    file.read_exact(&mut magic).ok()?;
    match magic {
        [0x1f, 0x8b, ..] => Some(SourceArchiveKind::TarGz),
        [0xfd, b'7', b'z', b'X', b'Z', 0x00] => Some(SourceArchiveKind::TarXz),
        [0x28, 0xb5, 0x2f, 0xfd, ..] => Some(SourceArchiveKind::TarZst),
        [b'B', b'Z', b'h', ..] => Some(SourceArchiveKind::TarBz2),
        _ => None,
    }
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
    archive::sanitize_permissions(destination).with_context(|| {
        format!(
            "sanitize source archive permissions in {}",
            destination.display()
        )
    })?;
    Ok(())
}

pub(super) fn source_archive_reader(path: &Path, kind: SourceArchiveKind) -> Result<Box<dyn Read>> {
    let file =
        File::open(path).with_context(|| format!("open source archive {}", path.display()))?;
    match kind {
        SourceArchiveKind::TarBz2 => Ok(Box::new(MultiBzDecoder::new(file))),
        SourceArchiveKind::TarGz => Ok(Box::new(GzDecoder::new(file))),
        SourceArchiveKind::TarXz => Ok(Box::new(XzDecoder::new(file))),
        SourceArchiveKind::TarZst => Ok(Box::new(
            zstd::stream::read::Decoder::new(file)
                .with_context(|| format!("decode zstd source archive {}", path.display()))?,
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extensionless_urls_are_sniff_eligible_and_named_files_are_not() {
        // codeload commit tarballs name no file — the whole reason sniffing exists.
        assert!(url_is_extensionless(
            "https://codeload.github.com/o/r/tar.gz/58b0188a7b8b90251a96b14513355594ac7ec949"
        ));
        assert!(url_is_extensionless("https://example.com/download?id=3"));
        // Any extension keeps its meaning: a gzipped patch must not become a tarball.
        assert!(!url_is_extensionless("https://example.com/fix.patch.gz"));
        assert!(!url_is_extensionless("payload.txt"));
        assert!(!url_is_extensionless(
            "https://ftp.gnu.org/gnu/hello/hello-2.12.3.tar.gz"
        ));
    }

    #[test]
    fn sniffing_recognises_the_three_containers_and_nothing_else() {
        let dir = tempfile::tempdir().unwrap();
        let write = |name: &str, bytes: &[u8]| {
            let path = dir.path().join(name);
            fs::write(&path, bytes).unwrap();
            path
        };
        let gz = write("gz", &[0x1f, 0x8b, 0x08, 0x00, 0x00, 0x00]);
        let xz = write("xz", &[0xfd, b'7', b'z', b'X', b'Z', 0x00]);
        let zst = write("zst", &[0x28, 0xb5, 0x2f, 0xfd, 0x00, 0x00]);
        let bz = write("bz", b"BZh91AY&SY");
        let txt = write("txt", b"verified source payload");
        assert!(matches!(
            source_archive_kind_sniffed(&gz),
            Some(SourceArchiveKind::TarGz)
        ));
        assert!(matches!(
            source_archive_kind_sniffed(&xz),
            Some(SourceArchiveKind::TarXz)
        ));
        assert!(matches!(
            source_archive_kind_sniffed(&zst),
            Some(SourceArchiveKind::TarZst)
        ));
        assert!(matches!(
            source_archive_kind_sniffed(&bz),
            Some(SourceArchiveKind::TarBz2)
        ));
        assert!(source_archive_kind_sniffed(&txt).is_none());
    }

    #[test]
    fn codeload_targz_path_form_detected_for_tag_and_commit_refs() {
        // codeload names the container in the path before the ref. A dotted tag ref (`0.9.0`)
        // defeated the suffix check and the extensionless-sniff path, so the archive never
        // extracted and `$ctx.sources.main.dir` came through as null.
        assert!(matches!(
            source_archive_kind("https://codeload.github.com/uutils/coreutils/tar.gz/0.9.0"),
            Some(SourceArchiveKind::TarGz)
        ));
        assert!(matches!(
            source_archive_kind(
                "https://codeload.github.com/o/r/tar.gz/58b0188a7b8b90251a96b14513355594ac7ec949"
            ),
            Some(SourceArchiveKind::TarGz)
        ));
        // A real suffix still wins; a plain release tarball is unaffected.
        assert!(matches!(
            source_archive_kind("https://ftp.gnu.org/gnu/grep/grep-3.12.tar.xz"),
            Some(SourceArchiveKind::TarXz)
        ));
    }

    #[test]
    fn source_archive_extraction_strips_special_mode_bits() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let archive_path = dir.path().join("src.tar.zst");
        let output = dir.path().join("out");

        let file = File::create(&archive_path)?;
        let encoder = zstd::stream::write::Encoder::new(file, 0)?;
        let mut tar = tar::Builder::new(encoder);
        let mut header = tar::Header::new_gnu();
        header.set_size(4);
        header.set_mode(0o4755);
        header.set_cksum();
        tar.append_data(&mut header, "tool", std::io::Cursor::new(b"tool"))?;
        let encoder = tar.into_inner()?;
        encoder.finish()?;

        extract_source_archive_inner(&archive_path, &output, SourceArchiveKind::TarZst)?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(output.join("tool"))?.permissions().mode();
            assert_eq!(mode & 0o7000, 0, "special mode bits must be stripped");
        }

        Ok(())
    }
}
