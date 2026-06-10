//! Reclaiming space: generation retention, store-path reachability from retained
//! generations, and explicit generation deletion.

use anyhow::{Context, Result, bail};
use std::{collections::BTreeSet, fs, path::Path};

use crate::{util::paths, util::progress::report};

use super::*;

/// Garbage-collects unreferenced store paths and old generations.
///
/// Keeps the `keep` most recent generations (including the current one), deletes older
/// generations, then deletes any store path not referenced by a retained generation.
pub fn gc(keep: usize) -> Result<()> {
    let mut generations = list_generations()?;
    generations.sort_by_key(|b| std::cmp::Reverse(b.id));

    if generations.is_empty() {
        report("no generations to collect");
        return Ok(());
    }

    let current = current_generation_id()?;
    let mut to_retain: BTreeSet<u64> = generations.iter().take(keep).map(|g| g.id).collect();
    if let Some(current_id) = current {
        to_retain.insert(current_id);
        // Always keep the rollback target too: with `--keep 1` the previous generation would
        // otherwise be collected and the next `grm rollback` would have nowhere to go.
        if let Some(previous) = generations
            .iter()
            .map(|g| g.id)
            .filter(|id| *id < current_id)
            .max()
        {
            to_retain.insert(previous);
        }
    }

    let freed_generations = delete_old_generations(&generations, &to_retain, current)?;
    prune_registry()?;
    let referenced = collect_referenced_paths(&to_retain)?;
    let freed_stores = collect_unreferenced_stores(&referenced)?;

    report_gc_result(freed_stores, freed_generations);
    Ok(())
}

pub(crate) fn delete_old_generations(
    generations: &[Generation],
    to_retain: &BTreeSet<u64>,
    current: Option<u64>,
) -> Result<usize> {
    let mut freed = 0;
    for g in generations {
        if to_retain.contains(&g.id) {
            continue;
        }
        // Never delete the current generation unless explicitly told to
        if Some(g.id) == current {
            continue;
        }
        let dir = generation_dir(g.id)?;
        if dir.exists() {
            fs::remove_dir_all(&dir)?;
            freed += 1;
        }
    }
    if freed > 0 {
        report(&format!("removed {freed} old generation(s)"));
    }
    Ok(freed)
}

pub(crate) fn prune_registry() -> Result<()> {
    let mut registry = read_registry().unwrap_or_default();
    let before = registry.len();
    registry.retain(|g| generation_dir(g.id).map(|d| d.exists()).unwrap_or(false));
    if registry.len() != before {
        if let Err(e) = write_registry(&registry) {
            report(&format!(
                "warning: could not write generations registry: {e}"
            ));
        }
    }
    Ok(())
}

pub(crate) fn collect_referenced_paths(to_retain: &BTreeSet<u64>) -> Result<BTreeSet<String>> {
    let mut referenced: BTreeSet<String> = BTreeSet::new();
    for id in to_retain {
        let dir = generation_dir(*id)?;
        let meta = dir.join("gen.nuon");
        if meta.exists() {
            if let Ok(g) = read_generation_metadata(&dir) {
                referenced.extend(g.store_paths);
            }
        }
    }
    Ok(referenced)
}

pub(crate) fn collect_unreferenced_stores(referenced: &BTreeSet<String>) -> Result<usize> {
    let store_root = paths::store_root()?;
    if !store_root.exists() {
        report("store root does not exist, nothing to collect");
        return Ok(0);
    }

    let mut freed = 0;
    for entry in fs::read_dir(&store_root)? {
        let entry = entry?;
        let path = entry.path();
        let path_str = path.display().to_string();
        if referenced.contains(&path_str) {
            continue;
        }
        let size = du(&path)?;
        fs::remove_dir_all(&path)?;
        report(&format!(
            "collected {} ({:.2} MiB)",
            path.file_name().unwrap_or_default().to_string_lossy(),
            size as f64 / (1024.0 * 1024.0)
        ));
        freed += 1;
    }
    Ok(freed)
}

pub(crate) fn report_gc_result(freed_stores: usize, freed_generations: usize) {
    if freed_stores == 0 && freed_generations == 0 {
        report("nothing to collect");
    } else {
        report(&format!(
            "gc complete: removed {freed_stores} store path(s) and {freed_generations} generation(s)"
        ));
    }
}

/// Deletes a specific generation by ID.
///
/// Fails if the generation does not exist or if it is the currently active generation.
/// Removes the generation directory and syncs the registry.
pub fn delete_generation(id: u64) -> Result<()> {
    let gen_dir = generation_dir(id)?;
    if !gen_dir.exists() {
        bail!("generation {id} does not exist");
    }
    if current_generation_id()? == Some(id) {
        bail!(
            "cannot delete generation {id}: it is the currently active generation; \
             switch to another generation first"
        );
    }
    fs::remove_dir_all(&gen_dir)
        .with_context(|| format!("remove generation directory {}", gen_dir.display()))?;

    let mut registry = read_registry().unwrap_or_default();
    let before = registry.len();
    registry.retain(|g| g.id != id);
    if registry.len() != before {
        if let Err(e) = write_registry(&registry) {
            report(&format!(
                "warning: could not write generations registry: {e}"
            ));
        }
    }

    report(&format!("deleted generation {id}"));
    Ok(())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Rough disk usage of a directory in bytes (follows hard links, so it may overcount).
pub(crate) fn du(path: &Path) -> Result<u64> {
    let mut total = 0u64;
    for entry in walkdir::WalkDir::new(path) {
        let entry = entry?;
        if entry.file_type().is_file() {
            total += entry.metadata()?.len();
        }
    }
    Ok(total)
}
