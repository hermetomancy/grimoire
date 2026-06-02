//! Filesystem layout: the user-local install root and the directories Grimoire derives from it
//! (state, packages, shims, caches, build output), plus the current target triple. `GRIMOIRE_ROOT`
//! overrides the root, which otherwise lives under the platform data directory — never a system path.

use anyhow::{Context, Result};
use std::{env, ffi::OsString, path::PathBuf};

/// Resolves the user-local install root. `GRIMOIRE_ROOT` overrides everything (used by
/// tests and power users); otherwise installs live under the platform data directory
/// (`~/.local/share`, `~/Library/Application Support`, `%APPDATA%`), never a system path.
pub fn install_root() -> Result<PathBuf> {
    resolve_install_root(env::var_os("GRIMOIRE_ROOT"), dirs::data_dir())
}

fn resolve_install_root(
    override_root: Option<OsString>,
    data_dir: Option<PathBuf>,
) -> Result<PathBuf> {
    if let Some(root) = override_root {
        return Ok(PathBuf::from(root));
    }

    let data_dir = data_dir.context(
        "could not determine a user-local data directory; set GRIMOIRE_ROOT to choose one",
    )?;
    Ok(data_dir.join("grimoire"))
}

/// Cache directory for fetched, checksum-verified source artifacts.
pub fn source_cache_dir() -> Result<PathBuf> {
    Ok(install_root()?.join("cache").join("sources"))
}

/// Cache directory for fetched, checksum-verified binary package archives.
pub fn archive_cache_dir() -> Result<PathBuf> {
    Ok(install_root()?.join("cache").join("archives"))
}

/// Output directory for archives produced by source builds before they are installed.
pub fn build_output_dir() -> Result<PathBuf> {
    Ok(install_root()?.join("cache").join("builds"))
}

pub fn target_triple() -> String {
    let os = env::consts::OS;
    let arch = env::consts::ARCH;
    let abi = match os {
        "macos" => "darwin",
        "windows" | "linux" => "gnu",
        _ => "unknown",
    };

    format!("{os}-{arch}-{abi}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn override_takes_precedence_over_data_dir() {
        let root = resolve_install_root(
            Some(OsString::from("/tmp/custom-root")),
            Some(PathBuf::from("/home/user/.local/share")),
        )
        .expect("override resolves");
        assert_eq!(root, Path::new("/tmp/custom-root"));
    }

    #[test]
    fn defaults_under_data_dir_when_no_override() {
        let root = resolve_install_root(None, Some(PathBuf::from("/home/user/.local/share")))
            .expect("data dir resolves");
        assert_eq!(root, Path::new("/home/user/.local/share/grimoire"));
    }

    #[test]
    fn errors_without_override_or_data_dir() {
        assert!(resolve_install_root(None, None).is_err());
    }
}
