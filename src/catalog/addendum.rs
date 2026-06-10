//! Addenda: data-only overlays that patch package metadata read from tomes.
//!
//! An addendum is a git repository (or local directory for testing) with an inert
//! `addendum.nuon` manifest at its root. Grimoire clones/copies it natively, stores its state as
//! NUON, and applies matching patches to rune metadata after reading the rune's exported
//! `package` record. Addenda never execute code; they only replace or merge structured data.

use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};

use crate::{
    catalog::signing,
    catalog::sync_common,
    cli::{TomeAddArgs, TomeRemoveArgs, TomeUpdateArgs},
    model::{
        AddendumManifest, AddendumState, Catalog, PackageMetadata, validate_tome_name,
        validate_tome_ref, validate_tome_url,
    },
    nu::nuon_io,
    tome,
    util::progress::{report, status},
};

const MANIFEST: &str = "addendum.nuon";

pub fn add(args: TomeAddArgs) -> Result<()> {
    validate_tome_url(&args.git_url)?;
    validate_tome_ref(&args.ref_name)?;
    sync_common::validate_local_source(&args.git_url, MANIFEST)?;

    let name = sync_common::read_source_catalog_name(
        &args.git_url,
        &args.ref_name,
        MANIFEST,
        |path| Ok(read_manifest(path)?.name),
        tome::git::clone,
    )?;
    validate_tome_name(&name)?;
    validate_local_addendum_if_available(&name, &args.git_url)?;

    let state_dir = sync_common::state_dir(AddendumState::SUBDIR)?;
    let state_path = state_dir.join(format!("{name}.nuon"));
    if state_path.exists() {
        bail!("addendum `{name}` already exists");
    }

    std::fs::create_dir_all(&state_dir)?;
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
    sync_common::remove_catalog::<AddendumState>(&args.name, true)
}

pub fn list() -> Result<()> {
    sync_common::list_catalogs::<AddendumState>()
}

pub fn update(args: TomeUpdateArgs) -> Result<()> {
    let addenda = match args.name {
        Some(name) => {
            validate_tome_name(&name)?;
            vec![sync_common::load_catalog::<AddendumState>(&name)?]
        }
        None => sync_common::load_catalogs::<AddendumState>()?,
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
    for state in sync_common::load_catalogs::<AddendumState>()? {
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

fn ensure_addendum_cache(state: &AddendumState) -> Result<PathBuf> {
    let cache_path = sync_common::cache_path(AddendumState::SUBDIR, &state.name)?;
    if !cache_path.join(MANIFEST).exists() {
        sync_addendum_cache(state)?;
    }
    Ok(cache_path)
}

pub(crate) fn sync_addendum_cache(state: &AddendumState) -> Result<()> {
    sync_common::sync_catalog_cache(
        state,
        MANIFEST,
        tome::git::clone,
        |state, cache_path| {
            let manifest = read_manifest(cache_path)?;
            sync_common::validate_catalog_identity::<AddendumState>(state, &manifest)
        },
        |state, cache_path| {
            let manifest = read_manifest(cache_path)?;
            let commit = tome::git::head_commit(cache_path)?;
            sync_common::record_catalog_sync_state::<AddendumState>(
                state,
                cache_path,
                MANIFEST,
                &manifest,
                commit.as_deref(),
            )
        },
    )
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
    sync_common::validate_catalog_identity::<AddendumState>(
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
