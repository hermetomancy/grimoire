//! Trust and index reading: signature verification for runes/archives/manifests, tome
//! cache validation, and the published package index.

use anyhow::{Context, Result, bail};
use std::{
    fs,
    path::{Path, PathBuf},
};

use crate::{
    catalog::signing,
    catalog::sync_common,
    model::{
        Catalog, TomeManifest, TomePackages, TomeState, validate_relative_package_path,
        validate_tome_url,
    },
    nu::{nuon_io, runtime::EmbeddedNuRuntime},
};

use super::*;

/// Finds the tome whose cache directory contains `path`. Returns `None` when the path is not
/// inside any configured tome cache (e.g. a local `.rn` file passed directly on the CLI).
pub fn find_tome_for_path(path: &std::path::Path) -> Result<Option<TomeState>> {
    let canonical = path
        .canonicalize()
        .with_context(|| format!("canonicalize {}", path.display()))?;
    for tome in sync_common::load_catalogs::<TomeState>()? {
        let cache = sync_common::cache_path(TomeState::SUBDIR, &tome.name)?;
        let cache = if cache.exists() {
            cache
                .canonicalize()
                .with_context(|| format!("canonicalize tome cache {}", cache.display()))?
        } else {
            cache
        };
        if canonical.starts_with(&cache) {
            return Ok(Some(tome));
        }
    }
    Ok(None)
}

/// Verifies a rune's detached signature (`package.rn.minisig`) against the tome's pinned signers.
/// Returns `Ok` when the rune is not inside any tome cache, when the tome has no pinned signers,
/// or when the signature verifies against one of the pinned keys.
pub fn verify_rune(rune: &std::path::Path) -> Result<()> {
    let Some(tome) = find_tome_for_path(rune)? else {
        return Ok(());
    };
    if tome.signer_pubkeys.is_empty() {
        return Ok(());
    }
    signing::verify_detached(rune, &tome.signer_pubkeys)
        .with_context(|| format!("verify rune signature for {}", rune.display()))
}

/// Verifies a fetched archive's detached signature against the tome's pinned signers. The
/// `.minisig` is fetched over the *same transport* the archive came from — `local_archive` is the
/// already-downloaded-and-checksummed file, `archive_location`/`base_dir` are where it was fetched
/// from (an `http(s)` URL or a path under the tome's package repo) — so a signed remote binhost
/// verifies, not just a local-path one. Returns `Ok` when the tome has no pinned signers.
pub fn verify_archive(
    local_archive: &Path,
    archive_location: &str,
    base_dir: &Path,
    tome: &TomeState,
) -> Result<()> {
    if tome.signer_pubkeys.is_empty() {
        return Ok(());
    }
    let signature_location = format!("{archive_location}.{}", signing::SIGNATURE_EXTENSION);
    let signature = crate::fetch::fetch_companion_text(&signature_location, base_dir)
        .with_context(|| format!("fetch archive signature {signature_location}"))?;
    let data = fs::read(local_archive)
        .with_context(|| format!("read archive {}", local_archive.display()))?;
    signing::verify_any(&data, &signature, &tome.signer_pubkeys)
        .with_context(|| format!("verify archive signature for {archive_location}"))
}

/// Loads a tome's binary package index along with the package-repository root that its
/// (relative) archive locations resolve against. `None` means the tome declares no package
/// index or has not published one yet, so callers fall back to a source build.
pub fn package_index(tome: &TomeState) -> Result<Option<(PathBuf, crate::model::PackageIndex)>> {
    let cache = ensure_tome_cache(tome)?;
    let manifest = EmbeddedNuRuntime.tome_manifest(&cache.join("tome.rn"))?;
    let Some(packages) = manifest.packages else {
        return Ok(None);
    };

    let Some(raw) = load_raw_index(&cache, &packages)? else {
        return Ok(None);
    };

    parse_resolved_index(&cache, &packages, &raw).map(Some)
}

/// Loads a tome's raw `index.nuon` text. Returns `None` when the tome has no package repo or
/// the index file does not exist yet.
pub(crate) fn load_raw_index(cache: &Path, packages: &TomePackages) -> Result<Option<String>> {
    if is_http_repo(&packages.repo) {
        let base = packages.repo.trim_end_matches('/');
        let index_url = format!("{base}/{}", packages.index);
        // One fetch per URL per process: resolution consults the index several times in
        // a single run, and the answer (document, missing, or unreachable) is the same
        // each time — an unreachable binhost should cost one timeout and one warning,
        // not one per query.
        static INDEX_MEMO: std::sync::OnceLock<
            std::sync::Mutex<std::collections::HashMap<String, Option<String>>>,
        > = std::sync::OnceLock::new();
        let memo = INDEX_MEMO.get_or_init(Default::default);
        if let Some(cached) = memo
            .lock()
            .expect("index memo lock poisoned")
            .get(&index_url)
        {
            return Ok(cached.clone());
        }
        let fetched = match crate::fetch::http_get_index(&index_url)? {
            crate::fetch::IndexFetch::Document(text) => Some(text),
            crate::fetch::IndexFetch::Missing => None,
            // Degrade, loudly: an unreachable binhost means no binary substitutes this
            // run — source builds still work, and refusing outright would let anyone who
            // can *block* (not forge) the index take installs down entirely. The trade:
            // a blocker can steer users from verified binaries to source builds; the
            // warning is the tell.
            crate::fetch::IndexFetch::Unreachable(err) => {
                crate::util::output::warn(&format!(
                    "binhost unreachable ({err:#}); continuing without its binary \
                     packages — affected installs build from source"
                ));
                None
            }
        };
        memo.lock()
            .expect("index memo lock poisoned")
            .insert(index_url, fetched.clone());
        Ok(fetched)
    } else {
        let root = packages_repo_root(cache, packages);
        let index_path = root.join(&packages.index);
        if !index_path.exists() {
            return Ok(None);
        }
        fs::read_to_string(&index_path)
            .with_context(|| format!("read package index {}", index_path.display()))
            .map(Some)
    }
}

/// Parses a verified index document and resolves each entry's archive location. For an `http(s)`
/// repo, relative archive paths are rewritten to absolute `{repo}/{archive}` URLs; for a local
/// repo, the returned root is the directory archive paths resolve against. The index is parsed
/// from the already-verified `text` rather than re-read from disk, so verification and parsing
/// see identical bytes.
pub(crate) fn parse_resolved_index(
    cache: &Path,
    packages: &TomePackages,
    text: &str,
) -> Result<(PathBuf, crate::model::PackageIndex)> {
    let mut index = crate::model::PackageIndex::from_value(nuon_io::parse_nuon(text)?)
        .context("parse package index")?;
    if is_http_repo(&packages.repo) {
        let base = packages.repo.trim_end_matches('/');
        for entry in index.entries.values_mut() {
            if !is_http_repo(&entry.archive) {
                entry.archive = format!("{base}/{}", entry.archive);
            }
        }
        // Archives now carry absolute URLs, so the base directory is unused at fetch time.
        Ok((PathBuf::new(), index))
    } else {
        Ok((packages_repo_root(cache, packages), index))
    }
}

/// Resolves the local directory that holds a tome's package index and (relative) archives — the
/// publish directory an author built and the host serves. An absolute path is used directly; a
/// relative path resolves against the tome cache.
pub(crate) fn packages_repo_root(cache: &Path, packages: &TomePackages) -> PathBuf {
    let path = Path::new(&packages.repo);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        cache.join(path)
    }
}

pub(crate) fn is_http_repo(repo: &str) -> bool {
    repo.starts_with("http://") || repo.starts_with("https://")
}

pub(crate) fn read_tome_manifest(path: &Path) -> Result<TomeManifest> {
    let manifest_path = path.join("tome.rn");
    if !manifest_path.exists() {
        bail!("tome cache is missing root tome.rn: {}", path.display());
    }
    EmbeddedNuRuntime
        .tome_manifest(&manifest_path)
        .with_context(|| format!("read tome manifest {}", manifest_path.display()))
}

pub(crate) fn verify_runes_manifest(cache: &Path, pubkeys: &[String]) -> Result<()> {
    let runes = read_runes_manifest(cache, pubkeys)?;
    let runes_dir = cache.join("runes");
    verify_rune_hashes(&runes, &runes_dir)?;
    check_for_extra_runes(&runes, &runes_dir)?;
    Ok(())
}

pub(crate) fn read_runes_manifest(cache: &Path, pubkeys: &[String]) -> Result<nu_protocol::Record> {
    let manifest_path = cache.join("runes-manifest.nuon");
    if !manifest_path.exists() {
        bail!("runes-manifest.nuon is missing");
    }

    signing::verify_detached(&manifest_path, pubkeys)
        .with_context(|| "verify runes-manifest.nuon signature")?;

    let value = nuon_io::read_nuon(&manifest_path)?;
    let record = crate::model::expect_record(value, "runes manifest")?;

    let format = crate::model::optional_i64(&record, "format")?.unwrap_or(0);
    if format != 1 {
        bail!("unsupported runes manifest format {format}; expected 1");
    }

    match record.get("runes") {
        Some(nu_protocol::Value::Record { val, .. }) => Ok(val.clone().into_owned()),
        Some(_) => bail!("runes manifest field `runes` must be a record"),
        None => bail!("runes manifest is missing required field `runes`"),
    }
}

pub(crate) fn verify_rune_hashes(runes: &nu_protocol::Record, runes_dir: &Path) -> Result<()> {
    for (name, hash_value) in runes.iter() {
        let expected = match hash_value {
            nu_protocol::Value::String { val, .. } => val.as_str(),
            _ => bail!("runes manifest entry `{name}` must be a string"),
        };
        let expected_hex = expected.strip_prefix("sha256:").unwrap_or(expected);
        let rune_path = runes_dir.join(name);
        if !rune_path.exists() {
            bail!("runes manifest lists `{name}` but it does not exist in runes/");
        }
        let actual_hash = crate::archive::archive_hash(&rune_path)
            .with_context(|| format!("hash rune {}", rune_path.display()))?;
        let actual_hex = actual_hash.strip_prefix("sha256:").unwrap_or(&actual_hash);
        if actual_hex != expected_hex {
            bail!(
                "runes manifest hash mismatch for `{name}`: expected {expected}, got {actual_hash}"
            );
        }
    }
    Ok(())
}

pub(crate) fn check_for_extra_runes(runes: &nu_protocol::Record, runes_dir: &Path) -> Result<()> {
    for entry in fs::read_dir(runes_dir)
        .with_context(|| format!("read runes directory {}", runes_dir.display()))?
    {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("rn") {
            continue;
        }
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .with_context(|| format!("rune path has invalid file name: {}", path.display()))?;
        if runes.get(name).is_none() {
            bail!("extra rune file `{name}` found in runes/ but not listed in runes-manifest.nuon");
        }
    }
    Ok(())
}

pub(crate) fn validate_tome_cache(
    tome: &TomeState,
    cache_path: &Path,
    manifest: &TomeManifest,
    runtime: &EmbeddedNuRuntime,
) -> Result<()> {
    sync_common::validate_catalog_identity::<TomeState>(tome, manifest)?;

    let packages = manifest
        .packages
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("tome manifest is missing required field `packages`"))?;
    validate_tome_packages(packages)?;

    // Every package is defined by a rune (even an x-bin package carries a fetch-and-verify rune),
    // so a tome must ship a `runes/` directory with at least one definition, and no two may collide
    // on package name.
    let runes_dir = cache_path.join("runes");
    if !runes_dir.is_dir() {
        bail!(
            "tome cache is missing runes directory: {}",
            runes_dir.display()
        );
    }

    let mut rune_count = 0_usize;
    let mut package_names = std::collections::BTreeSet::new();
    for entry in walkdir::WalkDir::new(&runes_dir).sort_by_file_name() {
        let entry = entry?;
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.into_path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("rn") {
            continue;
        }

        let metadata = runtime
            .package_metadata(&path)
            .with_context(|| format!("validate tome rune {}", path.display()))?;
        if !package_names.insert(metadata.name.clone()) {
            bail!(
                "tome contains duplicate package name `{}` in {}",
                metadata.name,
                path.display()
            );
        }
        rune_count += 1;
    }

    if rune_count == 0 {
        bail!(
            "tome cache contains no rune definitions in {}",
            runes_dir.display()
        );
    }

    if !manifest.signers.is_empty() {
        let pubkeys = if tome.signer_pubkeys.is_empty() {
            &manifest.signers
        } else {
            &tome.signer_pubkeys
        };
        verify_runes_manifest(cache_path, pubkeys)?;
    }

    Ok(())
}

pub(crate) fn validate_tome_packages(packages: &TomePackages) -> Result<()> {
    validate_tome_url(&packages.repo).context("validate tome packages.repo")?;
    let is_http = is_http_repo(&packages.repo);
    match packages.format.as_str() {
        "http" if is_http => {}
        "local" if !is_http => {}
        "http" => bail!("tome packages.format `http` requires an http(s) packages.repo URL"),
        "local" => bail!("tome packages.format `local` requires a filesystem packages.repo path"),
        other => {
            bail!("tome packages.format `{other}` is not supported; expected `http` or `local`")
        }
    }
    validate_relative_package_path(&packages.index, "tome packages.index")?;
    Ok(())
}
