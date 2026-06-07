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
    progress::{report, status},
    signing, sync_common, tome,
};

const MANIFEST: &str = "addendum.nuon";

pub fn add(args: TomeAddArgs) -> Result<()> {
    validate_tome_url(&args.git_url)?;
    validate_tome_ref(&args.ref_name)?;
    sync_common::validate_local_source(&args.git_url, MANIFEST)?;

    let name = read_source_addendum_name(&args.git_url, &args.ref_name)?;
    validate_tome_name(&name)?;
    validate_local_addendum_if_available(&name, &args.git_url)?;

    let state_dir = sync_common::state_dir("addendums")?;
    let state_path = state_dir.join(format!("{name}.nuon"));
    if state_path.exists() {
        bail!("addendum `{name}` already exists");
    }

    fs::create_dir_all(&state_dir)?;
    let state = AddendumState {
        name: name.clone(),
        url: sync_common::resolve_source_url(&args.git_url, MANIFEST)?,
        ref_name: args.ref_name,
        checked_ref: None,
        checked_commit: None,
        addendum: None,
        signer_pubkeys: args.signer,
    };
    nuon_io::write_nuon(&state_path, &state.to_value())?;
    report(&format!("added addendum {name}"));
    Ok(())
}

pub fn remove(args: TomeRemoveArgs) -> Result<()> {
    validate_tome_name(&args.name)?;
    let state_path = sync_common::state_dir("addendums")?.join(format!("{}.nuon", args.name));
    if !state_path.exists() {
        bail!("addendum `{}` is not configured", args.name);
    }
    fs::remove_file(state_path)?;
    let cache_path = sync_common::cache_path("addendums", &args.name)?;
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

        if let Ok(head) = tome::git::head_commit(&cache) {
            if head.as_ref() != state.checked_commit.as_ref() {
                report(&format!(
                    "warning: addendum `{}` is stale; run `grm addendum update {}`",
                    state.name, state.name
                ));
            }
        }

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
    let state_dir = sync_common::state_dir("addendums")?;
    let mut states = Vec::new();
    for path in sync_common::list_state_files(&state_dir)? {
        states.push(
            AddendumState::from_value(nuon_io::read_nuon(&path)?)
                .with_context(|| format!("read addendum state {}", path.display()))?,
        );
    }
    Ok(states)
}

fn ensure_addendum_cache(state: &AddendumState) -> Result<PathBuf> {
    let cache_path = sync_common::cache_path("addendums", &state.name)?;
    if !cache_path.join(MANIFEST).exists() {
        sync_addendum_cache(state)?;
    }
    Ok(cache_path)
}

pub(crate) fn sync_addendum_cache(state: &AddendumState) -> Result<()> {
    let cache_dir = sync_common::cache_dir("addendums")?;
    let cache_path = sync_common::cache_path("addendums", &state.name)?;
    fs::create_dir_all(&cache_dir)?;
    let temp = tempfile::Builder::new()
        .prefix("grimoire-addendum-")
        .tempdir_in(&cache_dir)?;
    let staged = temp.path().join("cache");

    if !sync_common::copy_local_source(&state.name, &state.url, &staged, MANIFEST)? {
        status(&format!(
            "cloning addendum ({}) ref ({})",
            state.name, state.ref_name
        ));
        tome::git::clone(&state.url, &state.ref_name, &staged)
            .with_context(|| format!("could not sync addendum `{}`", state.name))?;
    }

    validate_staged_addendum_cache(state, &staged)?;
    sync_common::promote_cache(&staged, &cache_path, || {
        record_addendum_sync_state(state, &cache_path)
    })
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

fn record_addendum_sync_state(state: &AddendumState, cache_path: &Path) -> Result<()> {
    let manifest = read_manifest(cache_path)?;
    validate_addendum_cache(state, &manifest)?;
    let commit = tome::git::head_commit(cache_path)?;
    let mut state = load_addendum(&state.name).unwrap_or_else(|_| state.clone());
    let signer_pubkeys = sync_common::capture_signer(
        "addendum",
        &state.name,
        &cache_path.join(MANIFEST),
        &manifest.signers,
        &state.signer_pubkeys,
    )?;
    state.checked_ref = Some(state.ref_name.clone());
    state.checked_commit = commit;
    state.addendum = Some(manifest);
    state.signer_pubkeys = signer_pubkeys;
    let state_path = sync_common::state_dir("addendums")?.join(format!("{}.nuon", state.name));
    nuon_io::write_nuon(&state_path, &state.to_value())
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
    let state_path = sync_common::state_dir("addendums")?.join(format!("{name}.nuon"));
    if !state_path.exists() {
        bail!("addendum `{name}` is not configured");
    }
    AddendumState::from_value(nuon_io::read_nuon(&state_path)?)
        .with_context(|| format!("read addendum state {}", state_path.display()))
}

fn read_source_addendum_name(url: &str, ref_name: &str) -> Result<String> {
    if sync_common::is_local_source(url, MANIFEST) {
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

fn validate_local_addendum_if_available(name: &str, url: &str) -> Result<()> {
    if !sync_common::is_local_source(url, MANIFEST) {
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
