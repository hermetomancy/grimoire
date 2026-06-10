//! Generation bookkeeping: ids, `gen.nuon` metadata, and the `state/generations.nuon`
//! registry with its on-disk resync.

use anyhow::{Context, Result, bail};
use std::{
    fs,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use crate::{nu::nuon_io, util::paths, util::progress::report};

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
                Err(e) => report(&format!(
                    "warning: could not read generation metadata {}: {e}",
                    gen_path.display()
                )),
            }
        }
    }

    generations.sort_by_key(|b| std::cmp::Reverse(b.id));

    if changed {
        if let Err(e) = write_registry(&generations) {
            report(&format!(
                "warning: could not write generations registry: {e}"
            ));
        }
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
    let nu_protocol::Value::List { vals, .. } = value else {
        bail!("generations registry is not a list");
    };
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
    let value = nu_protocol::Value::list(items, span);
    nuon_io::write_nuon(&path, &value)
}
