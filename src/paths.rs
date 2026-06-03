//! Filesystem layout: the user-local install root and the directories Grimoire derives from it
//! (state, packages, shims, caches, build output), plus the current target triple. `GRIMOIRE_ROOT`
//! overrides the root, which otherwise is `~/.grimoire` — never a system path.

use anyhow::{Context, Result};
use std::{env, ffi::OsString, path::PathBuf};

/// Resolves the user-local install root. `GRIMOIRE_ROOT` overrides everything (used by tests and
/// power users); otherwise installs live in `~/.grimoire` on every platform.
///
/// The root is intentionally `~/.grimoire` (like `~/.cargo`/`~/.rustup`) rather than a platform
/// data directory: it is consistent cross-platform and, critically, **space-free**. A space in the
/// install root breaks source builds — autotools bakes the absolute paths of build tools (e.g.
/// `MKDIR_P`) into Makefiles unquoted, so `~/Library/Application Support/...` splits at the space.
pub fn install_root() -> Result<PathBuf> {
    resolve_install_root(env::var_os("GRIMOIRE_ROOT"), dirs::home_dir())
}

fn resolve_install_root(
    override_root: Option<OsString>,
    home_dir: Option<PathBuf>,
) -> Result<PathBuf> {
    if let Some(root) = override_root {
        return Ok(PathBuf::from(root));
    }

    let home = home_dir.context(
        "could not determine the home directory; set GRIMOIRE_ROOT to choose an install root",
    )?;
    Ok(home.join(".grimoire"))
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

pub fn package_dir(name: &str, version: &str) -> Result<PathBuf> {
    Ok(install_root()?.join(package_relative_dir(name, version)))
}

pub fn package_relative_dir(name: &str, version: &str) -> PathBuf {
    PathBuf::from("packages").join(name).join(version)
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
    fn override_takes_precedence_over_home() {
        let root = resolve_install_root(
            Some(OsString::from("/tmp/custom-root")),
            Some(PathBuf::from("/home/user")),
        )
        .expect("override resolves");
        assert_eq!(root, Path::new("/tmp/custom-root"));
    }

    #[test]
    fn defaults_to_dot_grimoire_under_home() {
        let root =
            resolve_install_root(None, Some(PathBuf::from("/home/user"))).expect("home resolves");
        assert_eq!(root, Path::new("/home/user/.grimoire"));
    }

    #[test]
    fn errors_without_override_or_home() {
        assert!(resolve_install_root(None, None).is_err());
    }
}
