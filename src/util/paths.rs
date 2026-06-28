//! Filesystem layout: the user-local install root and the directories Grimoire derives from it
//! (state, packages, profiles, caches, build output), plus the current target triple. `GRIMOIRE_ROOT`
//! overrides the root, which otherwise is `~/.grimoire` — never a system path.

use anyhow::{Context, Result, bail};
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

/// Disk-backed scratch root for source builds. The per-build work/package/sandbox tree is created
/// under here via `tempfile::tempdir_in` instead of inheriting `$TMPDIR` (which defaults to `/tmp`,
/// a small tmpfs on many hosts that an llvm-sized build overflows: `No space left on device`).
/// `GRIMOIRE_ROOT` relocates it alongside the rest of the install root for tests/isolated installs.
pub fn build_tmp_dir() -> Result<PathBuf> {
    Ok(install_root()?.join("buildtmp"))
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
/// When using the fixed store (`/grm`) this is `/grm/profiles/<user>` so that multiple users
/// do not collide. (Generations are symlinks into the store, which cross filesystems freely,
/// so co-location is no longer required.) When `GRIMOIRE_ROOT` is set (tests, isolated
/// installs) generations live under the install root.
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

/// Parses a CLI target argument, accepting only Grimoire's supported POSIX target triples.
/// Dependency/platform filters may use globs and OS shorthands; an actual build/archive target
/// must be an exact triple.
pub fn parse_target_arg(target: &str) -> std::result::Result<String, String> {
    validate_target_triple(target)
        .map(|()| target.to_owned())
        .map_err(|err| err.to_string())
}

pub fn validate_target_triple(target: &str) -> Result<()> {
    let mut parts = target.split('-');
    let (Some(os), Some(arch), Some(abi), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        bail!(
            "target `{target}` is not a supported triple; expected <os>-<arch>-<abi>, e.g. linux-aarch64-musl"
        );
    };
    if !matches!(arch, "x86_64" | "aarch64") {
        bail!(
            "target `{target}` uses unsupported architecture `{arch}`; expected x86_64 or aarch64"
        );
    }
    let valid = matches!(
        (os, abi),
        ("linux", "musl" | "gnu") | ("macos", "darwin") | ("freebsd", "unknown")
    );
    if !valid {
        bail!(
            "target `{target}` is unsupported; expected linux-*-musl, linux-*-gnu, macos-*-darwin, or freebsd-*-unknown"
        );
    }
    Ok(())
}

/// Validates a target-keyed metadata map key (`deps.build`, `bins`, build-manifest `bins`).
///
/// These keys select one concrete target layer: `default`, an OS shorthand, or a supported exact
/// triple. Globs belong on dependency/source platform selectors, not on target-keyed maps.
pub fn validate_target_key(key: &str, label: &str) -> Result<()> {
    if key == "default" || matches!(key, "linux" | "macos" | "freebsd") {
        return Ok(());
    }
    validate_target_triple(key)
        .with_context(|| format!("{label} target key `{key}` is not `default`, a supported OS key, or a supported exact target triple"))
}

fn resolve_target_triple(os: &str, arch: &str) -> String {
    let abi = match os {
        "macos" => "darwin",
        "linux" => "musl",
        _ => "unknown",
    };

    format!("{os}-{arch}-{abi}")
}

/// The build *host's* libc, detected at runtime and cached. Distinct from `target_triple()` (the
/// OUTPUT ABI — always `-musl` on Linux): this reflects the machine grm runs on. A glibc host
/// cross-builds the musl toolchain from a gnu rust seed; a pure-musl host (e.g. Chimera Linux) must
/// seed from the musl release, whose binaries only its `ld-musl` loader can run. Probed by the
/// presence of the musl dynamic loader (the canonical signal — same soname family as `tome::lint`'s
/// LIBC_FLOOR). `GRM_HOST_LIBC` overrides for unusual hosts. "musl" | "glibc" on Linux, "none" else.
pub fn host_libc() -> &'static str {
    static CACHE: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    CACHE.get_or_init(detect_host_libc).as_str()
}

fn detect_host_libc() -> String {
    if let Some(over) = env::var_os("GRM_HOST_LIBC") {
        return over.to_string_lossy().into_owned();
    }
    if env::consts::OS != "linux" {
        return "none".to_string();
    }
    // musl ships exactly one `ld-musl-<arch>.so.1`; its presence is the host-is-musl signal.
    if std::path::Path::new(&format!("/lib/ld-musl-{}.so.1", env::consts::ARCH)).exists() {
        "musl".to_string()
    } else {
        "glibc".to_string()
    }
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

    #[test]
    fn validate_target_triple_accepts_supported_targets() {
        for target in [
            "linux-x86_64-musl",
            "linux-aarch64-gnu",
            "macos-aarch64-darwin",
            "freebsd-x86_64-unknown",
        ] {
            validate_target_triple(target).unwrap();
        }
    }

    #[test]
    fn validate_target_triple_rejects_patterns_and_unknown_targets() {
        for target in [
            "linux",
            "linux-*",
            "windows-x86_64-msvc",
            "linux-riscv64-musl",
        ] {
            assert!(validate_target_triple(target).is_err(), "{target}");
        }
    }

    #[test]
    fn validate_target_key_accepts_map_layers_not_globs() {
        for key in [
            "default",
            "linux",
            "macos",
            "freebsd",
            "linux-x86_64-musl",
            "macos-aarch64-darwin",
        ] {
            validate_target_key(key, "test map").unwrap();
        }
        for key in ["linux-*", "linxu", "x86_64-unknown-linux-gnu"] {
            assert!(validate_target_key(key, "test map").is_err(), "{key}");
        }
    }
}
