use anyhow::Result;
use std::path::PathBuf;

use crate::{install, model::LockFile, nu::nuon_io, paths, tome};

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
        install::installed_states()?,
    );
    let path = lock_path()?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    nuon_io::write_nuon(&path, &lock.to_value())
}
