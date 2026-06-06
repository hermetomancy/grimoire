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
    addendum, install,
    model::{LockFile, parse_version_relaxed},
    nu::nuon_io,
    paths, tome,
};

/// One package as recorded in `grimoire.lock.nuon`: enough to pin a reproducible reinstall to the
/// exact version and verified archive hash that was last installed.
#[derive(Debug, Clone)]
pub struct LockedPackage {
    pub name: String,
    pub version: Version,
    pub archive_hash: String,
}

/// Path of the install snapshot lockfile, kept under install-root state alongside the other
/// NUON state. Grimoire is a user-local manager rather than per-project, so the lock belongs
/// with the install root it describes, not a working directory.
pub fn lock_path() -> Result<PathBuf> {
    Ok(paths::install_root()?
        .join("state")
        .join("grimoire.lock.nuon"))
}

/// Regenerates `grimoire.lock.nuon` from the current installed-package and tome state. Called
/// after every install and removal so the lock always reflects committed state.
pub fn rebuild() -> Result<()> {
    let lock = LockFile::new(
        paths::target_triple(),
        tome::load_tomes()?,
        addendum::load_addendums()?,
        install::installed_states()?,
    );
    let path = lock_path()?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    nuon_io::write_nuon(&path, &lock.to_value())
}

/// Reads the recorded packages from `grimoire.lock.nuon`. Returns `None` when no lockfile exists
/// yet, so callers can distinguish "nothing locked" from a parse failure. The lockfile is inert
/// data read through the shared NUON layer (AGENTS.md §3).
pub fn read_locked_packages() -> Result<Option<Vec<LockedPackage>>> {
    let path = lock_path()?;
    if !path.exists() {
        return Ok(None);
    }

    let value =
        nuon_io::read_nuon(&path).with_context(|| format!("read lockfile {}", path.display()))?;
    let Value::Record { val, .. } = &value else {
        bail!("lockfile root must be a record");
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
        packages.push(LockedPackage {
            name,
            version,
            archive_hash,
        });
    }
    Ok(Some(packages))
}

fn lock_field_string(record: &Record, field: &str) -> Result<String> {
    match record.get(field) {
        Some(Value::String { val, .. }) => Ok(val.clone()),
        Some(_) => bail!("lockfile field `{field}` must be a string"),
        None => bail!("lockfile package entry is missing field `{field}`"),
    }
}
