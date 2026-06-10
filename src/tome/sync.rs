//! Catalog lifecycle: add/update/remove/list, the cache sync that pins commits, and the
//! news surfacing hook.

use anyhow::{Context, Result, bail};
use std::{
    fs,
    path::{Path, PathBuf},
};

use crate::{
    cli::{TomeAddArgs, TomeRemoveArgs, TomeUpdateArgs},
    model::{Catalog, TomeState, validate_tome_name, validate_tome_ref, validate_tome_url},
    nu::{nuon_io, runtime::EmbeddedNuRuntime},
    progress::report,
    sync_common,
};

use super::*;

pub fn add(args: TomeAddArgs) -> Result<()> {
    validate_tome_url(&args.git_url)?;
    validate_tome_ref(&args.ref_name)?;
    sync_common::validate_local_source(&args.git_url, "tome.rn")?;

    // The tome names itself: read the `name` from its manifest rather than asking the user
    // to repeat it on the command line. Remote sources are cloned to a temp dir to read it.
    let name = sync_common::read_source_catalog_name(
        &args.git_url,
        &args.ref_name,
        "tome.rn",
        |path| {
            let manifest_path = path.join("tome.rn");
            Ok(EmbeddedNuRuntime
                .tome_manifest(&manifest_path)
                .with_context(|| format!("read tome manifest {}", manifest_path.display()))?
                .name)
        },
        git::clone,
    )?;
    validate_tome_name(&name)?;
    validate_local_tome_if_available(&name, &args.git_url)?;

    let state_dir = sync_common::state_dir(TomeState::SUBDIR)?;
    let state_path = state_dir.join(format!("{name}.nuon"));
    if state_path.exists() {
        bail!("tome `{name}` already exists");
    }

    fs::create_dir_all(&state_dir)?;
    let state = TomeState {
        name: name.clone(),
        url: sync_common::resolve_source_url(&args.git_url, "tome.rn")?,
        ref_name: args.ref_name,
        checked_ref: None,
        checked_commit: None,
        tome: None,
        signer_pubkeys: args.signer,
        last_seen_news: None,
    };
    nuon_io::write_nuon(&state_path, &state.to_value())?;
    report(&format!("added tome {name}"));
    Ok(())
}

pub fn list() -> Result<()> {
    sync_common::list_catalogs::<TomeState>()
}

pub fn update(args: TomeUpdateArgs) -> Result<()> {
    let tomes = match args.name {
        Some(name) => {
            validate_tome_name(&name)?;
            vec![sync_common::load_catalog::<TomeState>(&name)?]
        }
        None => sync_common::load_catalogs::<TomeState>()?,
    };

    if tomes.is_empty() {
        bail!("no tomes are configured");
    }

    for tome in tomes {
        let line = sync_tome_cache(&tome)?;
        report(&line);
    }
    Ok(())
}

pub fn update_all_configured() -> Result<()> {
    for tome in sync_common::load_catalogs::<TomeState>()? {
        let line = sync_tome_cache(&tome)?;
        report(&line);
    }
    Ok(())
}

pub fn remove(args: TomeRemoveArgs) -> Result<()> {
    sync_common::remove_catalog::<TomeState>(&args.name, false)
}

pub fn load_tomes() -> Result<Vec<TomeState>> {
    sync_common::load_catalogs::<TomeState>()
}

pub fn ensure_tome_cache(tome: &TomeState) -> Result<PathBuf> {
    let cache_path = sync_common::cache_path(TomeState::SUBDIR, &tome.name)?;
    // Re-sync when the cache is missing *or* invalid: a sync that failed partway (e.g. after a
    // misconfigured URL) can leave a directory with no root `tome.rn`, which would otherwise be
    // trusted forever and fail every later read. Treating that as "not cached" lets it self-heal.
    if !cache_path.join("tome.rn").exists() {
        sync_tome_cache(tome)?;
    }
    Ok(cache_path)
}

pub(crate) fn sync_report_line(
    name: &str,
    ref_name: &str,
    from: Option<&str>,
    to: Option<&str>,
) -> String {
    if from.is_some() && to.is_some() && from == to {
        format!(
            "tome {name} already at latest ({ref_name} {})",
            short_commit(to)
        )
    } else {
        format!(
            "updated tome {name} ({ref_name} {} -> {})",
            short_commit(from),
            short_commit(to)
        )
    }
}

pub(crate) fn short_commit(commit: Option<&str>) -> String {
    commit
        .map(|commit| commit.chars().take(7).collect())
        .unwrap_or_else(|| "unknown".to_owned())
}

pub(crate) fn sync_tome_cache(tome: &TomeState) -> Result<String> {
    let from_commit = tome.checked_commit.clone();
    let to_commit = sync_common::sync_catalog_cache(
        tome,
        "tome.rn",
        git::clone,
        |tome, cache_path| {
            let manifest = read_tome_manifest(cache_path)?;
            validate_tome_cache(tome, cache_path, &manifest, &EmbeddedNuRuntime)
        },
        |tome, cache_path| {
            let manifest = read_tome_manifest(cache_path)?;
            let commit = git::head_commit(cache_path)?;
            sync_common::record_catalog_sync_state::<TomeState>(
                tome,
                cache_path,
                "tome.rn",
                &manifest,
                commit.as_deref(),
            )?;
            Ok(commit)
        },
    )?;
    // The very first sync after `tome add` marks the existing news backlog as seen without
    // printing it; later syncs surface only what arrived since. `checked_ref` is recorded on
    // every successful sync (commits are absent for local tomes), so its absence in the state
    // we were called with means this sync was the first. News problems must not fail the sync
    // itself — the cache and state are already committed.
    let first_sync = tome.checked_ref.is_none();
    let cache_path = sync_common::cache_path(TomeState::SUBDIR, &tome.name)?;
    if let Err(e) = news::surface_after_sync(&tome.name, &cache_path, first_sync) {
        report(&format!("warning: could not surface tome news: {e}"));
    }
    Ok(sync_report_line(
        &tome.name,
        &tome.ref_name,
        from_commit.as_deref(),
        to_commit.as_deref(),
    ))
}

pub(crate) fn validate_local_tome_if_available(name: &str, url: &str) -> Result<()> {
    if !sync_common::is_local_source(url, "tome.rn") {
        return Ok(());
    }

    let source = Path::new(url);
    let manifest = EmbeddedNuRuntime
        .tome_manifest(&source.join("tome.rn"))
        .with_context(|| format!("validate local tome manifest {}", source.display()))?;
    let tome = TomeState {
        name: name.to_owned(),
        url: url.to_owned(),
        ref_name: "local-validation".to_owned(),
        checked_ref: None,
        checked_commit: None,
        tome: None,
        signer_pubkeys: Vec::new(),
        last_seen_news: None,
    };
    validate_tome_cache(&tome, source, &manifest, &EmbeddedNuRuntime)
}
