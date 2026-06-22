//! Generation bookkeeping: ids, `gen.nuon` metadata, and the `state/generations.nuon`
//! registry with its on-disk resync.

use anyhow::{Context, Result, bail};
use std::{
    fs,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use crate::{nu::nuon_io, util::paths, util::progress::warn};

use super::*;

/// Lists all retained generations, newest first.
///
/// Reads the canonical `state/generations.nuon` registry. Entries whose directories no longer
/// exist are pruned, and any generation directories on disk that are missing from the registry
/// are discovered and added. The registry is rewritten when it diverges from disk.
pub fn list_generations() -> Result<Vec<Generation>> {
    let mut generations = read_registry().unwrap_or_default();
    let mut changed = false;

    // Drop registry entries whose directories no longer exist.
    let before = generations.len();
    generations.retain(|g| generation_dir(g.id).map(|d| d.exists()).unwrap_or(false));
    if generations.len() != before {
        changed = true;
    }

    // Scan for generation directories on disk that are not yet in the registry.
    let dir = profiles_dir()?;
    if dir.exists() {
        for entry in fs::read_dir(&dir)? {
            let entry = entry?;
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            if name == "current" || !name.starts_with("gen-") {
                continue;
            }
            let gen_path = entry.path();
            if !gen_path.join("gen.nuon").exists() {
                continue;
            }
            let Ok(id) = parse_generation_id(name) else {
                continue;
            };
            if generations.iter().any(|g| g.id == id) {
                continue;
            }
            match read_generation_metadata(&gen_path) {
                Ok(g) => {
                    generations.push(g);
                    changed = true;
                }
                Err(e) => warn(&format!(
                    "could not read generation metadata {}: {e:#}",
                    gen_path.display()
                )),
            }
        }
    }

    generations.sort_by_key(|b| std::cmp::Reverse(b.id));

    if changed && let Err(e) = write_registry(&generations) {
        warn(&format!("could not write generations registry: {e:#}"));
    }

    Ok(generations)
}

pub(crate) fn next_generation_id() -> Result<u64> {
    let mut max = 0u64;
    let dir = profiles_dir()?;
    if !dir.exists() {
        return Ok(1);
    }
    for entry in fs::read_dir(&dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if name == "current" {
            continue;
        }
        if let Ok(id) = parse_generation_id(name) {
            max = max.max(id);
        }
    }
    Ok(max + 1)
}

pub(crate) fn parse_generation_id(name: &str) -> Result<u64> {
    let id_str = name
        .strip_prefix("gen-")
        .with_context(|| format!("generation name `{name}` is not `gen-<id>`"))?;
    id_str
        .parse::<u64>()
        .with_context(|| format!("generation id `{id_str}` is not a number"))
}

pub(crate) fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

pub(crate) fn write_generation_metadata(gen_dir: &Path, generation: &Generation) -> Result<()> {
    let mut record = nu_protocol::Record::new();
    record.push(
        "format",
        nu_protocol::Value::int(1, nu_protocol::Span::unknown()),
    );
    record.push(
        "id",
        nu_protocol::Value::int(generation.id as i64, nu_protocol::Span::unknown()),
    );
    record.push(
        "created",
        nu_protocol::Value::int(generation.created as i64, nu_protocol::Span::unknown()),
    );
    record.push(
        "packages",
        crate::model::string_list_value(&generation.packages),
    );
    record.push(
        "store_paths",
        crate::model::string_list_value(&generation.store_paths),
    );
    let value = nu_protocol::Value::record(record, nu_protocol::Span::unknown());
    let path = gen_dir.join("gen.nuon");
    nuon_io::write_nuon(&path, &value)
}

pub(crate) fn read_generation_metadata(gen_dir: &Path) -> Result<Generation> {
    let path = gen_dir.join("gen.nuon");
    let value = nuon_io::read_nuon(&path)?;
    let record = crate::model::expect_record(value, "generation metadata")?;
    let id = crate::model::required_field_i64(&record, "generation metadata", "id")? as u64;
    let created = crate::model::optional_i64(&record, "created")?.unwrap_or(0) as u64;
    let packages = crate::model::optional_string_list(&record, "packages")?;
    let store_paths = crate::model::optional_string_list(&record, "store_paths")?;
    Ok(Generation {
        id,
        created,
        packages,
        store_paths,
    })
}

pub(crate) fn generations_registry_path() -> Result<PathBuf> {
    Ok(paths::install_root()?
        .join("state")
        .join("generations.nuon"))
}

pub(crate) fn read_registry() -> Result<Vec<Generation>> {
    let path = generations_registry_path()?;
    if !path.exists() {
        return Ok(Vec::new());
    }
    let value = nuon_io::read_nuon(&path)?;
    let record = crate::model::expect_record(value, "generations registry")?;
    let Some(nu_protocol::Value::List { vals, .. }) = record.get("generations") else {
        bail!("generations registry is missing a `generations` list");
    };
    let vals = vals.clone();
    let mut generations = Vec::with_capacity(vals.len());
    for val in vals {
        let record = crate::model::expect_record(val, "generation registry entry")?;
        let id =
            crate::model::required_field_i64(&record, "generation registry entry", "id")? as u64;
        let created = crate::model::optional_i64(&record, "created")?.unwrap_or(0) as u64;
        let packages = crate::model::optional_string_list(&record, "packages")?;
        let store_paths = crate::model::optional_string_list(&record, "store_paths")?;
        generations.push(Generation {
            id,
            created,
            packages,
            store_paths,
        });
    }
    Ok(generations)
}

pub(crate) fn write_registry(generations: &[Generation]) -> Result<()> {
    let path = generations_registry_path()?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let span = nu_protocol::Span::unknown();
    let items: Vec<nu_protocol::Value> = generations
        .iter()
        .map(|g| {
            let mut record = nu_protocol::Record::new();
            record.push("id", nu_protocol::Value::int(g.id as i64, span));
            record.push("created", nu_protocol::Value::int(g.created as i64, span));
            record.push("packages", crate::model::string_list_value(&g.packages));
            record.push(
                "store_paths",
                crate::model::string_list_value(&g.store_paths),
            );
            nu_protocol::Value::record(record, span)
        })
        .collect();
    let mut root = nu_protocol::Record::new();
    root.push("format", nu_protocol::Value::int(1, span));
    root.push("generations", nu_protocol::Value::list(items, span));
    nuon_io::write_nuon(&path, &nu_protocol::Value::record(root, span))
}

/// Writes the full installed-package state into the generation directory as `state.nuon`.
/// This snapshot is what lets activation restore state: `gen.nuon` records only names and
/// store paths, which cannot reconstruct bins, deps, flags, or requested/held intent.
pub(super) fn write_state_snapshot(gen_dir: &Path, states: &[PackageState]) -> Result<()> {
    let span = nu_protocol::Span::unknown();
    let values: Vec<nu_protocol::Value> = states.iter().map(|s| s.to_value()).collect();
    let mut root = nu_protocol::Record::new();
    root.push("format", nu_protocol::Value::int(1, span));
    root.push("packages", nu_protocol::Value::list(values, span));
    nuon_io::write_nuon(
        &gen_dir.join("state.nuon"),
        &nu_protocol::Value::record(root, span),
    )
    .with_context(|| format!("write state snapshot for {}", gen_dir.display()))
}

/// Reads a generation's state snapshot; `None` when the generation predates snapshots.
pub(crate) fn read_state_snapshot(gen_dir: &Path) -> Result<Option<Vec<PackageState>>> {
    let path = gen_dir.join("state.nuon");
    if !path.exists() {
        return Ok(None);
    }
    let value = nuon_io::read_nuon(&path)?;
    let record = crate::model::expect_record(value, "state snapshot")?;
    let Some(nu_protocol::Value::List { vals, .. }) = record.get("packages") else {
        bail!(
            "state snapshot {} is missing a `packages` list",
            path.display()
        );
    };
    let vals = vals.clone();
    let states = vals
        .into_iter()
        .map(PackageState::from_value)
        .collect::<Result<Vec<_>>>()
        .with_context(|| format!("parse state snapshot {}", path.display()))?;
    Ok(Some(states))
}

/// Restores `state/packages/` and the lockfile from a generation's snapshot. Returns `false`
/// (and restores nothing) when the generation has no snapshot.
///
/// The replacement is staged into a sibling directory and committed with two renames: the
/// old state moves aside, the staged state moves in, then the backup is dropped. A crash
/// mid-swap leaves either the old or the new state directory in place — never a blend —
/// with the `.packages-old` backup detectable by `grm doctor`.
pub(super) fn restore_state_snapshot(gen_dir: &Path) -> Result<bool> {
    let Some(states) = read_state_snapshot(gen_dir)? else {
        return Ok(false);
    };

    let state_root = paths::install_root()?.join("state");
    let packages_dir = state_root.join("packages");
    let staging = state_root.join(".packages-staging");
    let backup = state_root.join(".packages-old");
    if staging.exists() {
        fs::remove_dir_all(&staging)?;
    }
    if backup.exists() {
        fs::remove_dir_all(&backup)?;
    }
    fs::create_dir_all(&staging)?;
    for state in &states {
        nuon_io::write_nuon(
            &staging.join(format!("{}.nuon", state.name)),
            &state.to_value(),
        )?;
    }

    // Snapshots describe the linked environment only; live *store-only* records (cached
    // build deps) are cache, orthogonal to the semantic state, and survive activation —
    // dropping them would orphan their store dirs (unrecorded dirs are never re-trusted).
    // Only genuine cache carries over: anything in the abandoned environment's linked
    // closure is exactly what a semantic rollback must drop, and a record whose store
    // path is gone is not worth keeping either.
    let snapshot_names: std::collections::HashSet<&str> =
        states.iter().map(|s| s.name.as_str()).collect();
    let live_world = crate::install::InstalledWorld::load_default().unwrap_or_default();
    let live_linked = live_world.linked_immut();
    for live in live_world.iter() {
        if snapshot_names.contains(live.name.as_str()) || live_linked.contains(&live.name) {
            continue;
        }
        if !Path::new(&live.store_path).exists() {
            continue;
        }
        nuon_io::write_nuon(
            &staging.join(format!("{}.nuon", live.name)),
            &live.to_value(),
        )?;
    }

    if packages_dir.exists() {
        fs::rename(&packages_dir, &backup)
            .with_context(|| format!("move aside {}", packages_dir.display()))?;
    }
    fs::rename(&staging, &packages_dir)
        .with_context(|| format!("promote restored state to {}", packages_dir.display()))?;
    if let Some(parent) = packages_dir.parent() {
        crate::util::fs_util::fsync_dir(parent)?;
    }
    let _ = fs::remove_dir_all(&backup);

    Ok(true)
}
