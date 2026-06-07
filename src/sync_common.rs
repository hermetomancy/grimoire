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

use crate::{fs_util, paths, progress, signing};

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
            let _ = fs::remove_dir_all(cache_path);
            if had_previous {
                let _ = fs::rename(&backup, cache_path);
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
