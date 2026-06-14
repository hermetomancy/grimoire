//! The managed build environment: sandboxed env vars, the controlled PATH order
//! (AGENTS.md §5), and host-tool boundary symlinks for bootstrap.

use anyhow::{Context, Result};
use nu_protocol::Value;
use std::{
    fs,
    path::{Path, PathBuf},
};

use crate::{build::toolchain::HostTool, util::paths};

use super::*;

#[derive(Debug)]
pub struct BuildEnv {
    pub path_dirs: Vec<PathBuf>,
    pub host_tools: Vec<HostTool>,
    pub inherit_host_path: bool,
    /// Additional environment variables to set in the build sandbox.
    pub extra_env: Vec<(String, String)>,
    /// Target triple the build is being performed for.
    pub target: String,
    /// When set, the POSIX ambient tail (`/usr/bin`, `/bin`) is dropped from build PATH so the
    /// build sees only declared deps and the managed core floor — the `--hermetic` enumeration
    /// mode that surfaces silent host-userland leaks. Diagnostic only; never affects the store hash.
    pub hermetic: bool,
}

#[derive(Debug)]
pub struct BuildDirs {
    pub package_dir: PathBuf,
    pub final_prefix: PathBuf,
    pub work_dir: PathBuf,
    pub log_file: Option<PathBuf>,
}

impl BuildEnv {
    /// Stage-0 authoring/bootstrap builds inherit the host PATH but still include installed
    /// build dependencies so later packages in a tome can find seeds built earlier.
    pub fn bootstrap(path_dirs: Vec<PathBuf>, extra_env: Vec<(String, String)>) -> Self {
        Self {
            path_dirs,
            host_tools: Vec::new(),
            inherit_host_path: true,
            extra_env,
            target: paths::target_triple(),
            hermetic: false,
        }
    }

    pub fn managed(
        path_dirs: Vec<PathBuf>,
        host_tools: Vec<HostTool>,
        extra_env: Vec<(String, String)>,
    ) -> Self {
        Self {
            path_dirs,
            host_tools,
            inherit_host_path: false,
            extra_env,
            target: paths::target_triple(),
            hermetic: false,
        }
    }
}

impl Default for BuildEnv {
    fn default() -> Self {
        Self::bootstrap(Vec::new(), Vec::new())
    }
}

#[derive(Debug)]
pub(crate) struct BuildSandboxEnv {
    /// Values exposed to runes through `ctx.env`.
    pub(crate) context: Vec<(String, String)>,
    /// Values installed into Nushell's execution environment for external build commands.
    pub(crate) process: Vec<(String, String)>,
}

pub(crate) fn sandbox_env_vars(
    work_dir: &Path,
    extra_env: &[(String, String)],
) -> Result<BuildSandboxEnv> {
    let home = work_dir.join(".grimoire-home");
    let tmp = work_dir.join(".grimoire-tmp");
    let xdg_cache = work_dir.join(".grimoire-xdg").join("cache");
    let xdg_config = work_dir.join(".grimoire-xdg").join("config");
    let xdg_data = work_dir.join(".grimoire-xdg").join("data");
    fs::create_dir_all(&home).with_context(|| format!("create sandbox HOME {}", home.display()))?;
    fs::create_dir_all(&tmp).with_context(|| format!("create sandbox TMPDIR {}", tmp.display()))?;
    fs::create_dir_all(&xdg_cache)
        .with_context(|| format!("create sandbox XDG cache {}", xdg_cache.display()))?;
    fs::create_dir_all(&xdg_config)
        .with_context(|| format!("create sandbox XDG config {}", xdg_config.display()))?;
    fs::create_dir_all(&xdg_data)
        .with_context(|| format!("create sandbox XDG data {}", xdg_data.display()))?;

    let mut context = Vec::new();
    set_env_value(&mut context, "HOME", home.display().to_string());
    set_env_value(&mut context, "TMPDIR", tmp.display().to_string());
    set_env_value(&mut context, "TEMP", tmp.display().to_string());
    set_env_value(&mut context, "TMP", tmp.display().to_string());
    set_env_value(
        &mut context,
        "XDG_CACHE_HOME",
        xdg_cache.display().to_string(),
    );
    set_env_value(
        &mut context,
        "XDG_CONFIG_HOME",
        xdg_config.display().to_string(),
    );
    set_env_value(
        &mut context,
        "XDG_DATA_HOME",
        xdg_data.display().to_string(),
    );
    set_env_value(&mut context, "GRIMOIRE_SANDBOX", "managed-env".to_string());

    // rustc bakes each source file's absolute path into panic/backtrace metadata, so an
    // unremapped Rust build smears this build's ephemeral work dir (the crate source and
    // the per-build cargo registry under HOME/.cargo) through the output binary —
    // leaking the builder's temp path and making archive bytes differ every build even
    // though the input-addressed store hash does not. Remap the work dir to a stable
    // sentinel so two builders of the same rune produce byte-identical binaries. rustc
    // embeds the *canonical* path (`/private/var/...` on macOS, where `/var` is a
    // symlink), so remap the canonicalized form; a rune that needs its own RUSTFLAGS sets
    // them inside `build` and overrides this. The flag is codegen-cosmetic — it rewrites
    // recorded paths only, never affecting the compiled semantics.
    let canonical_work = fs::canonicalize(work_dir).unwrap_or_else(|_| work_dir.to_path_buf());
    set_env_value(
        &mut context,
        "RUSTFLAGS",
        format!(
            "--remap-path-prefix={}=/grimoire-build",
            canonical_work.display()
        ),
    );
    // CMake bakes the Homebrew/MacPorts prefixes into its *built-in* platform search paths
    // on macOS, which no amount of env scrubbing reaches — only the ignore lists do. The
    // *environment* variants are parsed with the platform path separator (`:` on POSIX,
    // unlike the `;`-separated CMake variables); a `;` here silently degrades to one
    // nonexistent path and the ignore list does nothing (how LLVM 22 found Homebrew zstd).
    set_env_value(
        &mut context,
        "CMAKE_IGNORE_PREFIX_PATH",
        "/opt/homebrew:/usr/local:/opt/local".to_string(),
    );
    set_env_value(
        &mut context,
        "CMAKE_SYSTEM_IGNORE_PREFIX_PATH",
        "/opt/homebrew:/usr/local:/opt/local".to_string(),
    );

    for key in SCRUBBED_DISCOVERY_ENV {
        set_env_value(&mut context, key, String::new());
    }
    for (key, value) in extra_env {
        set_env_value(&mut context, key, value.clone());
    }

    let mut process = blank_inherited_env();
    for (key, value) in &context {
        set_env_value(&mut process, key, value.clone());
    }
    Ok(BuildSandboxEnv { context, process })
}

pub(crate) fn set_env_value(env: &mut Vec<(String, String)>, key: &str, value: String) {
    if let Some((_, existing)) = env.iter_mut().find(|(name, _)| name == key) {
        *existing = value;
    } else {
        env.push((key.to_string(), value));
    }
}

pub(crate) fn blank_inherited_env() -> Vec<(String, String)> {
    std::env::vars()
        .filter(|(key, _)| !PRESERVED_ENGINE_ENV.contains(&key.as_str()))
        .map(|(key, _)| (key, String::new()))
        .collect()
}

pub(crate) const PRESERVED_ENGINE_ENV: &[&str] = &["PATH", "PWD"];

pub(crate) const SCRUBBED_DISCOVERY_ENV: &[&str] = &[
    "ACLOCAL_PATH",
    "C_INCLUDE_PATH",
    "CMAKE_APPBUNDLE_PATH",
    "CMAKE_FRAMEWORK_PATH",
    "CMAKE_INCLUDE_PATH",
    "CMAKE_LIBRARY_PATH",
    "CMAKE_PREFIX_PATH",
    "CMAKE_PROGRAM_PATH",
    "CPLUS_INCLUDE_PATH",
    "CARGO_HOME",
    "CPATH",
    "DYLD_FALLBACK_LIBRARY_PATH",
    "DYLD_LIBRARY_PATH",
    "GEM_HOME",
    "GEM_PATH",
    "GOPATH",
    "HOMEBREW_CELLAR",
    "HOMEBREW_PREFIX",
    "HOMEBREW_REPOSITORY",
    "LD_LIBRARY_PATH",
    "LIBRARY_PATH",
    "NODE_PATH",
    "NPM_CONFIG_PREFIX",
    "PERL5LIB",
    "PKG_CONFIG_LIBDIR",
    "PKG_CONFIG_PATH",
    "PYTHONHOME",
    "PYTHONPATH",
    "RUBYLIB",
    "RUSTUP_HOME",
];

/// Renders a string as a NUON string literal so it can be safely interpolated into the
/// generated Nushell build runner. Routed through `nuon_io` per the single-NUON-layer rule.
/// Directories containing POSIX-mandated utilities that the host OS provides.
/// These are always included in managed build PATH so runes don't need to declare
/// ambient POSIX tools as managed build dependencies.
pub(crate) fn posix_ambient_dirs() -> Vec<PathBuf> {
    vec![PathBuf::from("/usr/bin"), PathBuf::from("/bin")]
}

pub(crate) fn build_path_entries(env: &BuildEnv, host_tool_dir: Option<&Path>) -> Vec<PathBuf> {
    let mut entries = env.path_dirs.clone();
    if let Some(dir) = host_tool_dir {
        entries.push(dir.to_path_buf());
    }
    // POSIX ambient utilities are available in managed builds unless the build is hermetic:
    // sed, grep, awk, find, mkdir, cp, chmod, expr, test, etc. `--hermetic` drops them to
    // enumerate which runes silently reach for host tools toybox does not ship (stage-2 work).
    if !env.hermetic {
        for dir in posix_ambient_dirs() {
            if dir.is_dir() && !entries.contains(&dir) {
                entries.push(dir);
            }
        }
    }
    if env.inherit_host_path {
        let Some(existing) = std::env::var_os("PATH") else {
            return entries;
        };
        entries.extend(std::env::split_paths(&existing));
    }
    entries
}

pub(crate) fn prepare_host_tool_dir(
    work_dir: &Path,
    host_tools: &[HostTool],
) -> Result<Option<PathBuf>> {
    if host_tools.is_empty() {
        return Ok(None);
    }

    let dir = work_dir.join(".grimoire-host-tools");
    fs::create_dir_all(&dir).with_context(|| format!("create host tool dir {}", dir.display()))?;
    for tool in host_tools {
        link_host_tool(&dir.join(&tool.name), &tool.path)?;
    }
    Ok(Some(dir))
}

pub(crate) fn link_host_tool(link: &Path, source: &Path) -> Result<()> {
    if link.exists() {
        fs::remove_file(link).with_context(|| format!("replace host tool {}", link.display()))?;
    }
    std::os::unix::fs::symlink(source, link)
        .with_context(|| format!("link host tool {} -> {}", link.display(), source.display()))
}

pub(crate) fn build_path_string(path_entries: &[PathBuf]) -> Option<String> {
    if path_entries.is_empty() {
        return None;
    }
    std::env::join_paths(path_entries)
        .ok()
        .map(|path| path.to_string_lossy().into_owned())
}

pub(crate) fn path_env_assignment(path_entries: &[PathBuf]) -> Result<String> {
    if path_entries.is_empty() {
        return Ok(String::new());
    }
    Ok(format!(
        "$env.PATH = {}\n",
        nuon_io::to_nuon_string(&path_list_value(path_entries))?
    ))
}

pub(crate) fn path_list_value(path_entries: &[PathBuf]) -> Value {
    Value::list(
        path_entries
            .iter()
            .map(|path| path_value(path.as_path()))
            .collect(),
        nu_protocol::Span::unknown(),
    )
}

pub(crate) fn path_value_from_string(path: &str) -> Value {
    let entries = std::env::split_paths(path).collect::<Vec<_>>();
    path_list_value(&entries)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sandbox_env_scrubs_host_discovery_and_allows_managed_overrides() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let managed_pkgconfig = "/grm/store/pkg/lib/pkgconfig";
        let env = sandbox_env_vars(
            temp.path(),
            &[
                ("PKG_CONFIG_PATH".to_string(), managed_pkgconfig.to_string()),
                ("LLVM_PREFIX".to_string(), "/grm/store/llvm".to_string()),
            ],
        )?;

        assert_eq!(
            env_value(&env.context, "PKG_CONFIG_PATH"),
            Some(managed_pkgconfig)
        );
        assert_eq!(
            env_value(&env.context, "LLVM_PREFIX"),
            Some("/grm/store/llvm")
        );
        assert_eq!(env_value(&env.context, "PYTHONPATH"), Some(""));
        assert_eq!(env_value(&env.context, "HOMEBREW_PREFIX"), Some(""));
        // The env-variable form is POSIX-colon separated; a `;` would make CMake treat the
        // whole list as one nonexistent prefix and ignore nothing.
        assert_eq!(
            env_value(&env.context, "CMAKE_IGNORE_PREFIX_PATH"),
            Some("/opt/homebrew:/usr/local:/opt/local")
        );
        assert_eq!(
            env_value(&env.context, "CMAKE_SYSTEM_IGNORE_PREFIX_PATH"),
            Some("/opt/homebrew:/usr/local:/opt/local")
        );
        assert_eq!(
            env_value(&env.process, "PKG_CONFIG_PATH"),
            Some(managed_pkgconfig)
        );
        assert_eq!(env_value(&env.process, "PYTHONPATH"), Some(""));
        if let Some((parent_key, _)) =
            std::env::vars().find(|(key, _)| !PRESERVED_ENGINE_ENV.contains(&key.as_str()))
        {
            assert_eq!(env_value(&env.process, &parent_key), Some(""));
        }

        let home = PathBuf::from(env_value(&env.context, "HOME").context("sandbox HOME")?);
        let tmp = PathBuf::from(env_value(&env.context, "TMPDIR").context("sandbox TMPDIR")?);
        let xdg_cache =
            PathBuf::from(env_value(&env.context, "XDG_CACHE_HOME").context("sandbox cache")?);
        assert!(home.starts_with(temp.path()));
        assert!(home.is_dir());
        assert!(tmp.starts_with(temp.path()));
        assert!(tmp.is_dir());
        assert!(xdg_cache.starts_with(temp.path()));
        assert!(xdg_cache.is_dir());
        Ok(())
    }

    fn env_value<'a>(env: &'a [(String, String)], key: &str) -> Option<&'a str> {
        env.iter()
            .find(|(name, _)| name == key)
            .map(|(_, value)| value.as_str())
    }

    #[test]
    fn hermetic_drops_posix_ambient_tail() {
        // A managed (non-bootstrap) build appends the ambient POSIX dirs so runes can reach
        // sed/grep/awk/… without declaring them; `--hermetic` drops that tail to surface which
        // runes silently reach for host tools the core floor does not ship.
        let ambient = posix_ambient_dirs();
        let existing: Vec<_> = ambient.iter().filter(|dir| dir.is_dir()).cloned().collect();

        let mut env = BuildEnv::managed(Vec::new(), Vec::new(), Vec::new());
        let with_ambient = build_path_entries(&env, None);
        for dir in &existing {
            assert!(
                with_ambient.contains(dir),
                "non-hermetic build must include ambient {}",
                dir.display()
            );
        }

        env.hermetic = true;
        let hermetic = build_path_entries(&env, None);
        for dir in &ambient {
            assert!(
                !hermetic.contains(dir),
                "hermetic build must drop ambient {}",
                dir.display()
            );
        }
    }
}
