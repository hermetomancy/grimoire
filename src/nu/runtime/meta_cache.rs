//! Content-keyed cache for parsed rune metadata.
//!
//! Parsing a rune (a full Nushell parse plus const extraction) is the hot cost of every
//! resolve: the staleness walk reads every installed package's rune, and the capability
//! index reads every rune in every tome, per mutating command. Both costs scale linearly
//! with catalog size, so the parse result is cached by content.

use anyhow::{Context, Result};
use nu_protocol::Value;
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    fs,
    path::Path,
    sync::{Mutex, OnceLock},
};

use crate::{nu::nuon_io, util::paths};

static MEMORY: OnceLock<Mutex<HashMap<String, Value>>> = OnceLock::new();

/// The parsed `export const package` value for `rune`, cached two ways: a per-process map
/// (the same rune is read several times within one command — candidates, capability index,
/// closure walker) and an on-disk NUON file under `cache/rune-meta/<version>/` keyed by the
/// sha256 of the rune bytes, so a catalog's runes are parsed once per *content*, not once
/// per command. The cached artifact is the const value itself — exactly what
/// `PackageMetadata::from_value` consumes — so the cache cannot drift from the parser, and
/// downstream metadata consumers never enter it.
///
/// The disk layer is best-effort on every path: an unreadable or corrupt entry is a miss
/// (and is overwritten), an unwritable cache directory just means re-parsing next time.
/// Parse *errors* are deliberately not cached — they stay loud.
pub(crate) fn cached_package_const(rune: &Path) -> Result<Value> {
    let bytes = fs::read(rune).with_context(|| format!("read {}", rune.display()))?;
    let key = format!("{:x}", Sha256::digest(&bytes));

    let memory = MEMORY.get_or_init(|| Mutex::new(HashMap::new()));
    if let Some(value) = memory.lock().expect("rune meta cache lock").get(&key) {
        return Ok(value.clone());
    }

    let disk_path = paths::rune_meta_cache_dir()
        .ok()
        .map(|dir| dir.join(format!("{key}.nuon")));
    if let Some(path) = &disk_path
        && let Ok(value) = nuon_io::read_nuon(path)
    {
        memory
            .lock()
            .expect("rune meta cache lock")
            .insert(key, value.clone());
        return Ok(value);
    }

    let value = super::eval::exported_const(rune, "package")?;
    if let Some(path) = &disk_path {
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        let _ = nuon_io::write_nuon(path, &value);
    }
    memory
        .lock()
        .expect("rune meta cache lock")
        .insert(key, value.clone());
    Ok(value)
}
