//! Shared utilities for syncing git-backed catalogs (tomes and addenda).
//!
//! Both tomes and addenda follow the same lifecycle: clone/copy, validate, promote,
//! record sync state, and trust-on-first-use signer pinning. This module holds the
//! common machinery so each consumer only wires its type-specific validation.

use anyhow::{Context, Result, bail};
use std::{
    fs,
    path::{Path, PathBuf},
};

use crate::{
    catalog::signing,
    model::{Catalog, CatalogManifest},
    nu::nuon_io,
    util::fs_util,
    util::paths,
    util::progress,
};

/// Returns `true` when `url` is a local directory containing `manifest_name`.
pub fn is_local_source(url: &str, manifest_name: &str) -> bool {
    let path = Path::new(url);
    path.is_dir() && path.join(manifest_name).exists()
}

/// Canonicalizes a local source URL so it stays absolute across working-directory changes.
/// Remote URLs pass through unchanged.
pub fn resolve_source_url(url: &str, manifest_name: &str) -> Result<String> {
    if !is_local_source(url, manifest_name) {
        return Ok(url.to_owned());
    }
    let absolute = Path::new(url)
        .canonicalize()
        .with_context(|| format!("resolve local source {url}"))?;
    Ok(absolute.to_string_lossy().into_owned())
}

/// Validates that a local source directory exists and contains its manifest.
pub fn validate_local_source(url: &str, manifest_name: &str) -> Result<()> {
    let path = Path::new(url);
    if path.exists() && path.is_dir() && !path.join(manifest_name).exists() {
        bail!(
            "local source is missing root {manifest_name}: {}",
            path.display()
        );
    }
    Ok(())
}

/// Returns the state directory for a catalog kind (`"tomes"` or `"addendums"`).
pub fn state_dir(subdir: &str) -> Result<PathBuf> {
    Ok(paths::install_root()?.join("state").join(subdir))
}

/// Returns the cache directory for a catalog kind (`"tomes"` or `"addendums"`).
pub fn cache_dir(subdir: &str) -> Result<PathBuf> {
    Ok(paths::install_root()?.join("cache").join(subdir))
}

/// Returns the cache path for a named catalog entry.
pub fn cache_path(subdir: &str, name: &str) -> Result<PathBuf> {
    Ok(cache_dir(subdir)?.join(name))
}

/// Returns the backup path for an atomic cache promotion.
pub fn cache_backup_path(cache_path: &Path) -> Result<PathBuf> {
    let name = cache_path
        .file_name()
        .and_then(|name| name.to_str())
        .context("cache path should have a name")?;
    Ok(cache_path.with_file_name(format!("{name}.grimoire-old")))
}

/// Lists `.nuon` state files in a directory, sorted.
pub fn list_state_files(state_dir: &Path) -> Result<Vec<PathBuf>> {
    if !state_dir.exists() {
        return Ok(Vec::new());
    }
    let mut files = Vec::new();
    for entry in fs::read_dir(state_dir)
        .with_context(|| format!("read state directory {}", state_dir.display()))?
    {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("nuon") {
            continue;
        }
        files.push(path);
    }
    files.sort();
    Ok(files)
}

/// Copies a local source into `staged` if `url` is local.
///
/// Returns `true` when the copy happened. When `false`, callers should clone the remote.
pub fn copy_local_source(
    name: &str,
    url: &str,
    staged: &Path,
    manifest_name: &str,
) -> Result<bool> {
    if !is_local_source(url, manifest_name) {
        return Ok(false);
    }
    let source = PathBuf::from(url)
        .canonicalize()
        .with_context(|| format!("resolve source {url}"))?;
    progress::status(&format!("copying local ({name})"));
    fs_util::copy_dir_all(&source, staged, name)
        .with_context(|| format!("copy local source for {name}"))?;
    Ok(true)
}

/// Atomically promotes `staged` to `cache_path` with backup/rollback.
///
/// If `cache_path` already exists, it is renamed to a `.grimoire-old` backup.
/// After `record_fn` succeeds, the backup is removed. If `record_fn` fails,
/// the new cache is removed and the backup is restored.
pub fn promote_cache<T>(
    staged: &Path,
    cache_path: &Path,
    record_fn: impl FnOnce() -> Result<T>,
) -> Result<T> {
    let backup = cache_backup_path(cache_path)?;
    let had_previous = cache_path.exists();
    if backup.exists() {
        fs::remove_dir_all(&backup)
            .with_context(|| format!("remove stale cache backup {}", backup.display()))?;
    }
    if had_previous {
        fs::rename(cache_path, &backup)
            .with_context(|| format!("back up cache {}", cache_path.display()))?;
    }

    if let Err(err) = fs::rename(staged, cache_path)
        .with_context(|| format!("promote cache {}", cache_path.display()))
    {
        if had_previous {
            let _ = fs::rename(&backup, cache_path);
        }
        return Err(err);
    }

    let result = match record_fn() {
        Ok(result) => result,
        Err(err) => {
            if had_previous {
                if cache_path.exists() {
                    let trash = cache_path.with_extension("grimoire-trash");
                    let _ = fs::rename(cache_path, &trash);
                    let _ = fs::rename(&backup, cache_path);
                    let _ = fs::remove_dir_all(&trash);
                } else {
                    let _ = fs::rename(&backup, cache_path);
                }
            } else {
                let _ = fs::remove_dir_all(cache_path);
            }
            return Err(err);
        }
    };

    if had_previous {
        let _ = fs::remove_dir_all(&backup);
    }
    Ok(result)
}

/// Trust-on-first-use capture for per-catalog signing.
///
/// Given the keys pinned so far (`existing`) and the manifest's advertised keys,
/// returns the set of keys to record going forward.
pub fn capture_signer(
    entity_kind: &str,
    name: &str,
    manifest_path: &Path,
    advertised: &[String],
    existing: &[String],
) -> Result<Vec<String>> {
    if existing.is_empty() && advertised.is_empty() {
        return Ok(Vec::new());
    }

    if !existing.is_empty() && advertised.is_empty() {
        bail!(
            "{entity_kind} `{name}` previously advertised signers but no longer does; refusing. \
             Remove and re-add the {entity_kind} to trust an unsigned manifest.",
        );
    }

    if existing.is_empty() && !advertised.is_empty() {
        if let Err(err) = signing::verify_detached(manifest_path, advertised) {
            bail!("{entity_kind} `{name}` manifest signature does not verify: {err}",);
        }
        progress::report(&format!(
            "pinned {} signer(s) for {entity_kind} `{name}` (trust on first use)",
            advertised.len(),
        ));
        return Ok(advertised.to_vec());
    }

    let sets_match =
        existing.len() == advertised.len() && existing.iter().all(|k| advertised.contains(k));
    if !sets_match {
        if signing::verify_detached(manifest_path, existing).is_ok() {
            progress::report(&format!(
                "{entity_kind} `{name}` rotated signing keys ({} -> {})",
                existing.len(),
                advertised.len(),
            ));
            return Ok(advertised.to_vec());
        }
        bail!(
            "{entity_kind} `{name}` now advertises a different set of signing keys than the one pinned on \
             first use; refusing. Remove and re-add the {entity_kind} to trust the new keys.",
        );
    }

    if let Err(err) = signing::verify_detached(manifest_path, existing) {
        bail!(
            "{entity_kind} `{name}` manifest signature does not verify against pinned keys: {err}",
        );
    }

    Ok(existing.to_vec())
}

/// Loads every catalog state file of a given kind.
pub fn load_catalogs<C: Catalog>() -> anyhow::Result<Vec<C>> {
    let state_dir = state_dir(C::SUBDIR)?;
    let mut states = Vec::new();
    for path in list_state_files(&state_dir)? {
        states.push(
            C::from_nuon(nuon_io::read_nuon(&path)?)
                .with_context(|| format!("read {} state {}", C::ENTITY_KIND, path.display()))?,
        );
    }
    Ok(states)
}

/// Loads a single catalog state by name.
pub fn load_catalog<C: Catalog>(name: &str) -> anyhow::Result<C> {
    let state_path = state_dir(C::SUBDIR)?.join(format!("{name}.nuon"));
    if !state_path.exists() {
        anyhow::bail!("{} `{name}` is not configured", C::ENTITY_KIND);
    }
    C::from_nuon(nuon_io::read_nuon(&state_path)?)
        .with_context(|| format!("read {} state {}", C::ENTITY_KIND, state_path.display()))
}

/// Removes a catalog's state file and optionally its cache.
pub fn remove_catalog<C: Catalog>(name: &str, remove_cache: bool) -> anyhow::Result<()> {
    crate::model::validate_tome_name(name)?;
    let state_path = state_dir(C::SUBDIR)?.join(format!("{name}.nuon"));
    if !state_path.exists() {
        anyhow::bail!("{} `{name}` is not configured", C::ENTITY_KIND);
    }
    fs::remove_file(&state_path)?;
    if remove_cache {
        let cache_path = cache_path(C::SUBDIR, name)?;
        if cache_path.exists() {
            fs::remove_dir_all(&cache_path)?;
        }
    }
    progress::report(&format!("removed {} {name}", C::ENTITY_KIND));
    Ok(())
}

/// Lists every catalog of a given kind.
pub fn list_catalogs<C: Catalog>() -> anyhow::Result<()> {
    for state in load_catalogs::<C>()? {
        println!("{}\t{}\t{}", state.name(), state.url(), state.ref_name());
    }
    Ok(())
}

/// Reads the self-declared name from a catalog source without permanently caching it.
pub fn read_source_catalog_name(
    url: &str,
    ref_name: &str,
    manifest_name: &str,
    read_name: impl Fn(&Path) -> anyhow::Result<String>,
    clone: impl Fn(&str, &str, &Path) -> anyhow::Result<()>,
) -> anyhow::Result<String> {
    if is_local_source(url, manifest_name) {
        return read_name(Path::new(url));
    }

    let temp =
        tempfile::tempdir().context("create temporary directory to read catalog manifest")?;
    clone(url, ref_name, temp.path())
        .with_context(|| format!("clone `{url}` to read its manifest"))?;
    read_name(temp.path())
}

/// Syncs a catalog cache: clone/copy, validate, promote, and record state.
pub fn sync_catalog_cache<C: Catalog, T>(
    state: &C,
    manifest_name: &str,
    clone: impl Fn(&str, &str, &Path) -> anyhow::Result<()>,
    validate: impl Fn(&C, &Path) -> anyhow::Result<()>,
    record: impl FnOnce(&C, &Path) -> anyhow::Result<T>,
) -> anyhow::Result<T> {
    let cache_dir = cache_dir(C::SUBDIR)?;
    let cache_path = cache_path(C::SUBDIR, state.name())?;
    fs::create_dir_all(&cache_dir)?;
    let temp = tempfile::Builder::new()
        .prefix(&format!("grimoire-{}-", C::SUBDIR))
        .tempdir_in(&cache_dir)?;
    let staged = temp.path().join("cache");

    if !copy_local_source(state.name(), state.url(), &staged, manifest_name)? {
        progress::status(&format!("cloning {} ({})", C::ENTITY_KIND, state.name()));
        clone(state.url(), state.ref_name(), &staged)
            .with_context(|| format!("could not sync {} `{}`", C::ENTITY_KIND, state.name()))?;
    }

    validate(state, &staged)?;
    promote_cache(&staged, &cache_path, || record(state, &cache_path))
}

/// Records a successful sync into the catalog state file, pinning signers trust-on-first-use.
pub fn record_catalog_sync_state<C: Catalog>(
    state: &C,
    cache_path: &Path,
    manifest_name: &str,
    manifest: &C::Manifest,
    commit: Option<&str>,
) -> anyhow::Result<()> {
    let mut new_state = load_catalog::<C>(state.name()).unwrap_or_else(|_| state.clone());
    let signer_pubkeys = capture_signer(
        C::ENTITY_KIND,
        state.name(),
        &cache_path.join(manifest_name),
        manifest.signers(),
        state.signer_pubkeys(),
    )?;
    new_state.set_checked_ref(Some(state.ref_name().to_owned()));
    new_state.set_checked_commit(commit.map(|c| c.to_owned()));
    new_state.set_manifest(Some(manifest.clone()));
    new_state.set_signer_pubkeys(signer_pubkeys);

    let state_path = state_dir(C::SUBDIR)?.join(format!("{}.nuon", state.name()));
    nuon_io::write_nuon(&state_path, &new_state.to_nuon())?;
    Ok(())
}

/// Validates that a catalog manifest declares the same name as its configured state.
pub fn validate_catalog_identity<C: Catalog>(
    state: &C,
    manifest: &C::Manifest,
) -> anyhow::Result<()> {
    if manifest.name() != state.name() {
        anyhow::bail!(
            "{} manifest name `{}` does not match configured {} `{}`",
            C::ENTITY_KIND,
            manifest.name(),
            C::ENTITY_KIND,
            state.name()
        );
    }
    Ok(())
}
