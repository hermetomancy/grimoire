//! Filesystem layout: the user-local install root and the directories Grimoire derives from it
//! (state, packages, profiles, caches, build output), plus the current target triple. `GRIMOIRE_ROOT`
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

/// On-disk rune metadata cache: parsed `export const package` values keyed by the sha256 of
/// the rune bytes. Versioned by crate release because the cached value reflects this
/// grimoire's Nushell parsing semantics.
pub fn rune_meta_cache_dir() -> Result<PathBuf> {
    Ok(install_root()?
        .join("cache")
        .join("rune-meta")
        .join(env!("CARGO_PKG_VERSION")))
}

/// Directory for full build logs written during source builds.
pub fn build_log_dir() -> Result<PathBuf> {
    Ok(install_root()?.join("logs").join("builds"))
}

/// The content-addressed store root.
///
/// By default this is `/grm/store` — a single fixed path that is identical on every machine
/// so baked absolute paths (RPATH, install_name, pkg-config prefix) are portable across
/// hosts and users.
///
/// When `GRIMOIRE_ROOT` is set (for testing or isolated installs), the store lives under
/// `<GRIMOIRE_ROOT>/store` instead. This forfeits cross-machine binary cache portability
/// but is necessary for tests and user-local installs.
pub fn store_root() -> Result<PathBuf> {
    if std::env::var_os("GRIMOIRE_ROOT").is_some() {
        Ok(install_root()?.join("store"))
    } else {
        Ok(PathBuf::from("/grm/store"))
    }
}

/// The directory where actual generation trees live.
///
/// When using the fixed store (`/grm`) this is `/grm/profiles/<user>` so that hard links
/// into the store are on the same filesystem and multiple users do not collide. When
/// `GRIMOIRE_ROOT` is set (tests, isolated installs) generations live under the install root.
pub fn profiles_dir() -> Result<PathBuf> {
    if std::env::var_os("GRIMOIRE_ROOT").is_some() {
        Ok(install_root()?.join("profiles"))
    } else {
        let user = std::env::var("USER").unwrap_or_else(|_| "unknown".to_string());
        Ok(PathBuf::from("/grm/profiles").join(user))
    }
}

/// The user-facing profile directory. This is always under the install root and holds the
/// `current` symlink that users put on their PATH.
pub fn user_profiles_dir() -> Result<PathBuf> {
    Ok(install_root()?.join("profiles"))
}

/// Returns platform-specific setup instructions when the fixed store directory (`/grm")
/// does not exist and `GRIMOIRE_ROOT` is not set. Returns `None` when everything is ready.
pub fn fixed_store_setup_instructions() -> Option<String> {
    if std::env::var_os("GRIMOIRE_ROOT").is_some() {
        return None;
    }
    let store = store_root().ok()?;
    let parent = store.parent()?;
    if parent.exists() {
        return None;
    }

    #[cfg(target_os = "macos")]
    return Some(format!(
        "the fixed store directory `{}` does not exist\n\n\
         Run `sudo grm setup` to register it, then reboot.\n\
         Alternatively, create it manually with:\n\
         \techo 'grm' | sudo tee -a /etc/synthetic.conf\n\
         \t# Reboot, then optionally mount a dedicated APFS volume:\n\
         \tsudo diskutil apfs addVolume disk1 'Grimoire' /grm\n",
        parent.display()
    ));

    #[cfg(target_os = "linux")]
    return Some(format!(
        "the fixed store directory `{}` does not exist\n\n\
         Run `sudo grm setup` to create it, or manually:\n\
         \tsudo mkdir /grm\n",
        parent.display()
    ));

    #[cfg(target_os = "freebsd")]
    return None;
}

pub fn store_path(hash: &str, name: &str, version: &str) -> Result<PathBuf> {
    Ok(store_root()?.join(crate::store::store_path_basename(hash, name, version)))
}

pub fn store_relative_dir(hash: &str, name: &str, version: &str) -> PathBuf {
    PathBuf::from(crate::store::store_path_basename(hash, name, version))
}

pub fn target_triple() -> String {
    resolve_target_triple(env::consts::OS, env::consts::ARCH)
}

fn resolve_target_triple(os: &str, arch: &str) -> String {
    let abi = match os {
        "macos" => "darwin",
        "linux" => "musl",
        _ => "unknown",
    };

    format!("{os}-{arch}-{abi}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn override_takes_precedence_over_home() -> Result<()> {
        let root = resolve_install_root(
            Some(OsString::from("/tmp/custom-root")),
            Some(PathBuf::from("/home/user")),
        )?;
        assert_eq!(root, Path::new("/tmp/custom-root"));
        Ok(())
    }

    #[test]
    fn defaults_to_dot_grimoire_under_home() -> Result<()> {
        let root = resolve_install_root(None, Some(PathBuf::from("/home/user")))?;
        assert_eq!(root, Path::new("/home/user/.grimoire"));
        Ok(())
    }

    #[test]
    fn errors_without_override_or_home() {
        assert!(resolve_install_root(None, None).is_err());
    }

    #[test]
    fn resolve_target_triple_linux_defaults_to_musl() {
        assert_eq!(
            resolve_target_triple("linux", "x86_64"),
            "linux-x86_64-musl"
        );
        assert_eq!(
            resolve_target_triple("linux", "aarch64"),
            "linux-aarch64-musl"
        );
    }

    #[test]
    fn resolve_target_triple_macos_defaults_to_darwin() {
        assert_eq!(
            resolve_target_triple("macos", "x86_64"),
            "macos-x86_64-darwin"
        );
        assert_eq!(
            resolve_target_triple("macos", "aarch64"),
            "macos-aarch64-darwin"
        );
    }

    #[test]
    fn resolve_target_triple_freebsd_defaults_to_unknown() {
        assert_eq!(
            resolve_target_triple("freebsd", "x86_64"),
            "freebsd-x86_64-unknown"
        );
        assert_eq!(
            resolve_target_triple("freebsd", "aarch64"),
            "freebsd-aarch64-unknown"
        );
    }
}
