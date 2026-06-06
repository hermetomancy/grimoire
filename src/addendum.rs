//! Addenda: data-only overlays that patch package metadata read from tomes.
//!
//! An addendum is a git repository (or local directory for testing) with an inert
//! `addendum.nuon` manifest at its root. Grimoire clones/copies it natively, stores its state as
//! NUON, and applies matching patches to rune metadata after reading the rune's exported
//! `package` record. Addenda never execute code; they only replace or merge structured data.

use anyhow::{Context, Result, bail};
use std::{
    fs,
    path::{Path, PathBuf},
};

use crate::{
    cli::{TomeAddArgs, TomeRemoveArgs, TomeUpdateArgs},
    model::{
        AddendumManifest, AddendumState, PackageMetadata, validate_tome_name, validate_tome_ref,
        validate_tome_url,
    },
    nu::nuon_io,
    paths,
    progress::{report, status},
    signing, tome,
};

const MANIFEST: &str = "addendum.nuon";

pub fn add(args: TomeAddArgs) -> Result<()> {
    validate_tome_url(&args.git_url)?;
    validate_tome_ref(&args.ref_name)?;
    validate_local_addendum_source(&args.git_url)?;

    let name = read_source_addendum_name(&args.git_url, &args.ref_name)?;
    validate_tome_name(&name)?;
    validate_local_addendum_if_available(&name, &args.git_url)?;

    let state_dir = addendum_state_dir()?;
    let state_path = state_dir.join(format!("{name}.nuon"));
    if state_path.exists() {
        bail!("addendum `{name}` already exists");
    }

    fs::create_dir_all(&state_dir)?;
    let state = AddendumState {
        name: name.clone(),
        url: resolve_addendum_source_url(&args.git_url)?,
        ref_name: args.ref_name,
        checked_ref: None,
        checked_commit: None,
        addendum: None,
        signer_pubkeys: Vec::new(),
    };
    nuon_io::write_nuon(&state_path, &state.to_value())?;
    report(&format!("added addendum {name}"));
    Ok(())
}

pub fn remove(args: TomeRemoveArgs) -> Result<()> {
    validate_tome_name(&args.name)?;
    let state_path = addendum_state_dir()?.join(format!("{}.nuon", args.name));
    if !state_path.exists() {
        bail!("addendum `{}` is not configured", args.name);
    }
    fs::remove_file(state_path)?;
    let cache_path = addendum_cache_path(&args.name)?;
    if cache_path.exists() {
        fs::remove_dir_all(cache_path)?;
    }
    report(&format!("removed addendum {}", args.name));
    Ok(())
}

pub fn list() -> Result<()> {
    for state in load_addendums()? {
        println!("{}\t{}\t{}", state.name, state.url, state.ref_name);
    }
    Ok(())
}

pub fn update(args: TomeUpdateArgs) -> Result<()> {
    let addenda = match args.name {
        Some(name) => {
            validate_tome_name(&name)?;
            vec![load_addendum(&name)?]
        }
        None => load_addendums()?,
    };

    if addenda.is_empty() {
        report("no addenda configured");
        return Ok(());
    }

    let mut any_failed = false;
    for state in addenda {
        match sync_addendum_cache(&state) {
            Ok(()) => report(&format!("updated addendum {}", state.name)),
            Err(e) => {
                report(&format!("failed to update addendum {}: {e}", state.name));
                any_failed = true;
            }
        }
    }

    if any_failed {
        bail!("one or more addenda failed to update");
    }
    Ok(())
}

/// Applies configured addendum patches to `metadata`. `tome_name` scopes tome-specific patches;
/// unscoped patches apply to any package with the matching name. Later addenda win because states
/// are loaded in name order and applied in that order.
pub fn apply_patches(
    metadata: &mut PackageMetadata,
    tome_name: Option<&str>,
    rune: &Path,
) -> Result<()> {
    for state in load_addendums()? {
        let cache = ensure_addendum_cache(&state)?;
        verify_addendum(&cache, &state)
            .with_context(|| format!("verify addendum `{}`", state.name))?;
        let manifest =
            read_manifest(&cache).with_context(|| format!("read addendum `{}`", state.name))?;
        for patch in &manifest.patches {
            if patch.package != metadata.name {
                continue;
            }
            if patch
                .tome
                .as_deref()
                .is_some_and(|patch_tome| Some(patch_tome) != tome_name)
            {
                continue;
            }
            status(&format!(
                "applying addendum {} to {} ({})",
                state.name,
                metadata.name,
                rune.display()
            ));
            metadata.apply_addendum_patch(patch);
        }
    }
    Ok(())
}

pub fn patched_package_metadata(
    metadata: &mut PackageMetadata,
    tome_name: Option<&str>,
    rune: &Path,
) -> Result<()> {
    apply_patches(metadata, tome_name, rune)
}

pub fn load_addendums() -> Result<Vec<AddendumState>> {
    let state_dir = addendum_state_dir()?;
    if !state_dir.exists() {
        return Ok(Vec::new());
    }

    let mut states = Vec::new();
    for entry in fs::read_dir(&state_dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("nuon") {
            continue;
        }
        states.push(
            AddendumState::from_value(nuon_io::read_nuon(&path)?)
                .with_context(|| format!("read addendum state {}", path.display()))?,
        );
    }
    states.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(states)
}

fn ensure_addendum_cache(state: &AddendumState) -> Result<PathBuf> {
    let cache_path = addendum_cache_path(&state.name)?;
    if !cache_path.join(MANIFEST).exists() {
        sync_addendum_cache(state)?;
    }
    Ok(cache_path)
}

pub(crate) fn sync_addendum_cache(state: &AddendumState) -> Result<()> {
    let cache_dir = addendum_cache_dir()?;
    let cache_path = addendum_cache_path(&state.name)?;
    fs::create_dir_all(&cache_dir)?;
    let temp = tempfile::Builder::new()
        .prefix("grimoire-addendum-")
        .tempdir_in(&cache_dir)?;
    let staged = temp.path().join("cache");

    if is_local_addendum_source(&state.url) {
        let source = PathBuf::from(&state.url)
            .canonicalize()
            .with_context(|| format!("resolve addendum source {}", state.url))?;
        status(&format!("copying local addendum ({})", state.name));
        copy_dir_all(&source, &staged)?;
    } else {
        tome::git::clone(&state.url, &state.ref_name, &staged)?;
    }

    validate_staged_addendum_cache(state, &staged)?;
    promote_addendum_cache(state, &staged, &cache_path)
}

fn validate_staged_addendum_cache(state: &AddendumState, cache_path: &Path) -> Result<()> {
    let manifest = read_manifest(cache_path)?;
    validate_addendum_cache(state, &manifest)
}

fn validate_addendum_cache(state: &AddendumState, manifest: &AddendumManifest) -> Result<()> {
    if manifest.name != state.name {
        bail!(
            "addendum manifest name `{}` does not match configured addendum `{}`",
            manifest.name,
            state.name
        );
    }
    Ok(())
}

fn promote_addendum_cache(state: &AddendumState, staged: &Path, cache_path: &Path) -> Result<()> {
    let backup = if cache_path.exists() {
        let backup = addendum_cache_backup_path(cache_path)?;
        remove_path_if_exists(&backup)?;
        fs::rename(cache_path, &backup)?;
        Some(backup)
    } else {
        None
    };

    if let Err(err) = fs::rename(staged, cache_path) {
        if let Some(backup) = &backup {
            let _ = fs::rename(backup, cache_path);
        }
        return Err(err)
            .with_context(|| format!("promote addendum cache {}", cache_path.display()));
    }

    if let Some(backup) = backup {
        let _ = fs::remove_dir_all(backup);
    }
    record_addendum_sync_state(state, cache_path)
}

fn record_addendum_sync_state(state: &AddendumState, cache_path: &Path) -> Result<()> {
    let manifest = read_manifest(cache_path)?;
    validate_addendum_cache(state, &manifest)?;
    let commit = tome::git::head_commit(cache_path)?;
    let mut state = load_addendum(&state.name).unwrap_or_else(|_| state.clone());
    let signer_pubkeys =
        capture_addendum_signer(&state, cache_path, &manifest, &state.signer_pubkeys)?;
    state.checked_ref = Some(state.ref_name.clone());
    state.checked_commit = commit;
    state.addendum = Some(manifest);
    state.signer_pubkeys = signer_pubkeys;
    let state_path = addendum_state_dir()?.join(format!("{}.nuon", state.name));
    nuon_io::write_nuon(&state_path, &state.to_value())
}

/// Trust-on-first-use for addendum signing keys. Mirrors `tome::capture_signer` — the first sync
/// pins whatever `signers` the manifest advertises after verifying a detached signature; later
/// syncs must present the exact same set, or a signature from the currently pinned keys to rotate.
fn capture_addendum_signer(
    state: &AddendumState,
    cache: &Path,
    manifest: &AddendumManifest,
    existing: &[String],
) -> Result<Vec<String>> {
    let advertised = manifest.signers.clone();

    if existing.is_empty() && advertised.is_empty() {
        return Ok(Vec::new());
    }

    if !existing.is_empty() && advertised.is_empty() {
        bail!(
            "addendum `{}` previously advertised signers but no longer does; refusing. \
             Remove and re-add the addendum to trust an unsigned manifest.",
            state.name
        );
    }

    if existing.is_empty() && !advertised.is_empty() {
        let manifest_path = cache.join(MANIFEST);
        if let Err(err) = signing::verify_detached(&manifest_path, &advertised) {
            bail!(
                "addendum `{}` manifest signature does not verify: {err}",
                state.name
            );
        }
        report(&format!(
            "pinned {} signer(s) for addendum `{}` (trust on first use)",
            advertised.len(),
            state.name
        ));
        return Ok(advertised);
    }

    let sets_match =
        existing.len() == advertised.len() && existing.iter().all(|k| advertised.contains(k));
    if !sets_match {
        let manifest_path = cache.join(MANIFEST);
        if signing::verify_detached(&manifest_path, existing).is_ok() {
            report(&format!(
                "addendum `{}` rotated signing keys ({} -> {})",
                state.name,
                existing.len(),
                advertised.len()
            ));
            return Ok(advertised);
        }
        bail!(
            "addendum `{}` now advertises a different set of signing keys than the one pinned on \
             first use; refusing. Remove and re-add the addendum to trust the new keys.",
            state.name
        );
    }

    Ok(existing.to_vec())
}

/// Verifies an addendum's detached signature (`addendum.nuon.minisig`) against the addendum's
/// pinned signers. Returns `Ok` when the addendum has no pinned signers or when the signature
/// verifies.
pub fn verify_addendum(cache_path: &Path, state: &AddendumState) -> Result<()> {
    if state.signer_pubkeys.is_empty() {
        return Ok(());
    }
    let manifest_path = cache_path.join(MANIFEST);
    signing::verify_detached(&manifest_path, &state.signer_pubkeys)
        .with_context(|| format!("verify addendum signature for {}", state.name))
}

fn load_addendum(name: &str) -> Result<AddendumState> {
    let state_path = addendum_state_dir()?.join(format!("{name}.nuon"));
    if !state_path.exists() {
        bail!("addendum `{name}` is not configured");
    }
    AddendumState::from_value(nuon_io::read_nuon(&state_path)?)
        .with_context(|| format!("read addendum state {}", state_path.display()))
}

fn read_source_addendum_name(url: &str, ref_name: &str) -> Result<String> {
    if is_local_addendum_source(url) {
        return Ok(read_manifest(Path::new(url))?.name);
    }

    let temp =
        tempfile::tempdir().context("create temporary directory to read addendum manifest")?;
    tome::git::clone(url, ref_name, temp.path())
        .with_context(|| format!("clone addendum `{url}` to read its manifest"))?;
    Ok(read_manifest(temp.path())?.name)
}

fn read_manifest(root: &Path) -> Result<AddendumManifest> {
    let path = root.join(MANIFEST);
    AddendumManifest::from_value(
        nuon_io::read_nuon(&path)
            .with_context(|| format!("read addendum manifest {}", path.display()))?,
    )
}

fn is_local_addendum_source(url: &str) -> bool {
    let path = Path::new(url);
    path.is_dir() && path.join(MANIFEST).exists()
}

fn resolve_addendum_source_url(url: &str) -> Result<String> {
    if !is_local_addendum_source(url) {
        return Ok(url.to_owned());
    }
    let absolute = Path::new(url)
        .canonicalize()
        .with_context(|| format!("resolve local addendum source {url}"))?;
    Ok(absolute.to_string_lossy().into_owned())
}

fn validate_local_addendum_source(url: &str) -> Result<()> {
    let path = Path::new(url);
    if path.exists() && path.is_dir() && !path.join(MANIFEST).exists() {
        bail!(
            "local addendum source is missing root {MANIFEST}: {}",
            path.display()
        );
    }
    Ok(())
}

fn validate_local_addendum_if_available(name: &str, url: &str) -> Result<()> {
    if !is_local_addendum_source(url) {
        return Ok(());
    }
    let manifest = read_manifest(Path::new(url))?;
    validate_addendum_cache(
        &AddendumState {
            name: name.to_owned(),
            url: url.to_owned(),
            ref_name: "local-validation".to_owned(),
            checked_ref: None,
            checked_commit: None,
            addendum: None,
            signer_pubkeys: Vec::new(),
        },
        &manifest,
    )
}

fn copy_dir_all(source: &Path, destination: &Path) -> Result<()> {
    crate::fs_util::copy_dir_all(source, destination, "addendum")
}

fn remove_path_if_exists(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() => fs::remove_dir_all(path)?,
        Ok(_) => fs::remove_file(path)?,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => return Err(err).with_context(|| format!("read {}", path.display())),
    }
    Ok(())
}

fn addendum_state_dir() -> Result<PathBuf> {
    Ok(paths::install_root()?.join("state").join("addendums"))
}

fn addendum_cache_dir() -> Result<PathBuf> {
    Ok(paths::install_root()?.join("cache").join("addendums"))
}

fn addendum_cache_path(name: &str) -> Result<PathBuf> {
    Ok(addendum_cache_dir()?.join(name))
}

fn addendum_cache_backup_path(cache_path: &Path) -> Result<PathBuf> {
    let name = cache_path
        .file_name()
        .and_then(|name| name.to_str())
        .context("addendum cache path should have a name")?;
    Ok(cache_path.with_file_name(format!("{name}.grimoire-old")))
}
