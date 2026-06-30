//! The install snapshot lockfile (`grimoire.lock.nuon`).
//!
//! [`rebuild`] regenerates it from committed installed-package and tome state after every install
//! and removal, recording the exact versions, archive hashes, dependencies, and tome commits.
//! [`read_locked_packages`] reads it back so `install --locked` can reproduce a prior install.

use anyhow::{Context, Result, bail};
use nu_protocol::{Record, Value};
use semver::Version;
use std::path::PathBuf;

use crate::{
    install::InstalledWorld,
    model::{LockFile, parse_version_relaxed},
    nu::nuon_io,
    tome,
    util::paths,
};

/// One package as recorded in `grimoire.lock.nuon`: enough to pin a reproducible reinstall to the
/// exact version, verified archive hash, and content address that was last installed, and to
/// restore the user's install-reason and hold intent.
#[derive(Debug, Clone)]
pub struct LockedPackage {
    pub name: String,
    pub version: Version,
    pub archive_hash: String,
    /// The content address recorded at lock time; `None` for hand-written locks that omit it.
    pub store_hash: Option<String>,
    pub requested: bool,
    pub held: bool,
}

/// One tome as recorded in the lockfile: the commit the catalog was checked out at, when the
/// tome had one (local-path tomes do not).
#[derive(Debug, Clone)]
pub struct LockedTome {
    pub name: String,
    pub commit: Option<String>,
}

/// Path of the install snapshot lockfile, kept under install-root state alongside the other
/// NUON state. Grimoire is a user-local manager rather than per-project, so the lock belongs
/// with the install root it describes, not a working directory.
pub fn lock_path() -> Result<PathBuf> {
    Ok(paths::install_root()?
        .join("state")
        .join("grimoire.lock.nuon"))
}

/// Regenerates `grimoire.lock.nuon` from the given installed-package and tome state. Call once
/// at the end of a mutating command so the lock reflects committed state. Only the linked set is
/// recorded: the lock is the blueprint of the user's environment, and store-only packages
/// (cached build deps, residue) are reproducible scaffolding that `restore` would only sweep again.
pub fn rebuild(world: &InstalledWorld) -> Result<()> {
    let linked = world.linked_immut();
    let packages = world
        .iter()
        .filter(|state| linked.contains(&state.name))
        .cloned()
        .collect();
    let lock = LockFile {
        target: paths::target_triple(),
        tomes: tome::load_tomes()?,
        packages,
    };
    let path = lock_path()?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    nuon_io::write_nuon(&path, &lock.to_value())
}

/// `grm generation lock`: export the current install lockfile to `dest`, for sharing or
/// reproducing the recorded set elsewhere (the inverse of `grm generation restore --lockfile`).
/// The live lock at `state/grimoire.lock.nuon` is regenerated on every mutating command; this
/// copies that snapshot out. Errors when nothing is installed yet (no lock to export).
pub fn export(dest: &std::path::Path) -> Result<()> {
    let src = lock_path()?;
    if !src.exists() {
        bail!("no lockfile to export yet — install something first");
    }
    if let Some(parent) = dest.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    std::fs::copy(&src, dest).with_context(|| format!("write lockfile to {}", dest.display()))?;
    crate::util::output::report(&crate::util::output::accent(&format!(
        "exported lockfile to {}",
        dest.display()
    )));
    Ok(())
}

/// Reads the recorded packages from `grimoire.lock.nuon`. Returns `None` when no lockfile exists
/// yet, so callers can distinguish "nothing locked" from a parse failure. The lockfile is inert
/// data read through the shared NUON layer (AGENTS.md §4).
pub fn read_locked_packages() -> Result<Option<Vec<LockedPackage>>> {
    read_locked_packages_from(&lock_path()?)
}

/// Like [`read_locked_packages`], for an explicit lockfile path (`grm restore --lockfile`).
pub fn read_locked_packages_from(path: &std::path::Path) -> Result<Option<Vec<LockedPackage>>> {
    let Some(val) = read_lock_root(path)? else {
        return Ok(None);
    };
    let Some(Value::List { vals, .. }) = val.get("packages") else {
        bail!("lockfile is missing a `packages` list");
    };

    let mut packages = Vec::new();
    for entry in vals {
        let Value::Record { val, .. } = entry else {
            bail!("lockfile package entry must be a record");
        };
        let name = lock_field_string(val, "name")?;
        let version_raw = lock_field_string(val, "version")?;
        let version = parse_version_relaxed(&version_raw)
            .with_context(|| format!("lockfile version `{version_raw}` for `{name}`"))?;
        let archive_hash = lock_field_string(val, "archive_hash")?;
        let store_hash = match val.get("store_hash") {
            Some(Value::String { val, .. }) if !val.is_empty() => Some(val.clone()),
            _ => None,
        };
        packages.push(LockedPackage {
            name,
            version,
            archive_hash,
            store_hash,
            requested: lock_field_bool(val, "requested"),
            held: lock_field_bool(val, "held"),
        });
    }
    Ok(Some(packages))
}

/// Reads the recorded tomes (name + pinned commit) from a lockfile.
pub fn read_locked_tomes_from(path: &std::path::Path) -> Result<Option<Vec<LockedTome>>> {
    let Some(val) = read_lock_root(path)? else {
        return Ok(None);
    };
    // A minimal or hand-written lock may omit the list entirely; nothing to enforce then.
    let Some(Value::List { vals, .. }) = val.get("tomes") else {
        return Ok(Some(Vec::new()));
    };
    let mut tomes = Vec::new();
    for entry in vals {
        let Value::Record { val, .. } = entry else {
            bail!("lockfile tome entry must be a record");
        };
        let commit = match val.get("source_commit") {
            Some(Value::String { val, .. }) if !val.is_empty() => Some(val.clone()),
            _ => None,
        };
        tomes.push(LockedTome {
            name: lock_field_string(val, "name")?,
            commit,
        });
    }
    Ok(Some(tomes))
}

fn read_lock_root(path: &std::path::Path) -> Result<Option<Record>> {
    if !path.exists() {
        return Ok(None);
    }
    let value =
        nuon_io::read_nuon(path).with_context(|| format!("read lockfile {}", path.display()))?;
    let Value::Record { val, .. } = value else {
        bail!("lockfile root must be a record");
    };
    Ok(Some((*val).clone()))
}

fn lock_field_string(record: &Record, field: &str) -> Result<String> {
    match record.get(field) {
        Some(Value::String { val, .. }) => Ok(val.clone()),
        Some(_) => bail!("lockfile field `{field}` must be a string"),
        None => bail!("lockfile package entry is missing field `{field}`"),
    }
}

fn lock_field_bool(record: &Record, field: &str) -> bool {
    matches!(record.get(field), Some(Value::Bool { val: true, .. }))
}
