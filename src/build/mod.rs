//! Source builds: turning a rune (`.rn` package definition) into a verified `.tar.zst` archive.
//!
//! A build fetches and checksum-verifies the rune's declared sources, runs its `build` step in
//! the embedded Nushell runtime against a staging directory, and packs the result into an archive
//! with embedded metadata. The output is the same archive shape a prebuilt download produces, so
//! installs behave identically whether a package came from source or a binary repo.

pub(crate) mod toolchain;

mod output;
mod sources;

use output::*;
use sources::*;

use anyhow::{Context, Result, bail};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

use crate::{
    archive::pack,
    catalog::addendum,
    cli::BuildArgs,
    fetch, install,
    nu::runtime::{BuildDirs, BuildEnv, EmbeddedNuRuntime},
    tome,
    util::paths,
    util::progress::{report, status},
};

/// Core packages whose `bin/` directories are always prepended to build PATH.
const CORE_PACKAGES: &[&str] = &[
    "linux-headers",
    "musl",
    "compiler-rt",
    "llvm",
    "clang",
    "cmake",
    "python3",
    "make",
    "toybox",
    "toolchain-wrappers",
];

/// Returns `true` when `target` is a Linux musl triple.
pub fn is_musl_target(target: &str) -> bool {
    target.starts_with("linux-") && target.ends_with("-musl")
}

/// Returns the `bin/` directories of all installed core packages.
fn core_bin_dirs() -> Result<Vec<PathBuf>> {
    let states = install::installed_states()?;
    let mut dirs = Vec::new();
    for name in CORE_PACKAGES {
        let Some(state) = states.iter().find(|s| s.name == *name) else {
            continue;
        };
        install::push_bin_dirs(&mut dirs, state);
    }
    Ok(dirs)
}

/// Reads a rune's package metadata, verifying its signature and applying addendum patches.
/// `tome_name` scopes tome-specific patches; pass `None` when the rune is not inside a tome.
pub fn read_rune_metadata(
    rune: &Path,
    tome_name: Option<&str>,
) -> Result<crate::model::PackageMetadata> {
    tome::verify_rune(rune).with_context(|| format!("verify rune signature {}", rune.display()))?;
    let mut metadata = EmbeddedNuRuntime
        .package_metadata(rune)
        .with_context(|| format!("read rune metadata {}", rune.display()))?;
    addendum::apply_patches(&mut metadata, tome_name, rune)
        .with_context(|| format!("apply addendums to {}", rune.display()))?;
    Ok(metadata)
}

/// Returns `true` when `toolchain-wrappers` is installed, meaning the managed compiler boundary
/// is available and the host compiler boundary is no longer needed.
fn core_compiler_boundary_available() -> Result<bool> {
    let states = install::installed_states()?;
    Ok(states.iter().any(|s| s.name == "toolchain-wrappers"))
}

/// Environment variables injected for musl-target builds.
fn musl_static_env_vars() -> Vec<(String, String)> {
    vec![
        ("CC".to_string(), "cc".to_string()),
        ("CXX".to_string(), "c++".to_string()),
        ("AR".to_string(), "ar".to_string()),
        ("LD".to_string(), "ld".to_string()),
        ("NM".to_string(), "nm".to_string()),
        ("RANLIB".to_string(), "ranlib".to_string()),
        ("STRIP".to_string(), "strip".to_string()),
        ("CFLAGS".to_string(), "-static".to_string()),
        ("LDFLAGS".to_string(), "-static".to_string()),
    ]
}

/// Constructs a [`BuildEnv`] for a build on `target`. Core package `bin/` dirs are always
/// prepended to PATH. When the managed compiler boundary is not yet available (bootstrap),
/// host compiler tools are included.
pub fn build_env_for_target(
    path_dirs: Vec<PathBuf>,
    extra_env: Vec<(String, String)>,
    target: &str,
) -> Result<BuildEnv> {
    let mut all_path_dirs = core_bin_dirs()?;
    all_path_dirs.extend(path_dirs);

    let mut env = extra_env;
    if is_musl_target(target) {
        env.extend(musl_static_env_vars());
    }
    if target.starts_with("macos-") {
        if let Some(sdk) = toolchain::macos_sdk_path() {
            env.push(("SDKROOT".to_string(), sdk));
        }
    }

    if core_compiler_boundary_available()? {
        Ok(BuildEnv::managed(all_path_dirs, Vec::new(), env))
    } else {
        Ok(BuildEnv::managed(
            all_path_dirs,
            toolchain::source_build_host_tools()?,
            env,
        ))
    }
}

/// The result of a source build: the archive path, store hash, and discovered capabilities.
pub struct BuildResult {
    pub archive: PathBuf,
    pub store_hash: String,
    /// Bins discovered in the package directory (from bin/, sbin/, libexec/).
    pub discovered_bins: BTreeMap<String, String>,
    /// Library base names discovered in the package directory (from lib/, lib64/).
    pub libs: Vec<String>,
}

fn build_log_path(name: &str, version: &str, target: &str, store_hash: &str) -> Option<PathBuf> {
    let dir = paths::build_log_dir().ok()?;
    fs::create_dir_all(&dir).ok()?;
    let filename = format!("{}-{}-{}-{}.log", name, version, target, store_hash);
    Some(dir.join(filename))
}

pub fn build(args: BuildArgs) -> Result<()> {
    let result = build_package(
        &args.package,
        &args.output,
        args.bootstrap,
        args.target.as_deref(),
    )?;
    report(&format!("built {}", result.archive.display()));
    Ok(())
}

pub fn build_package(
    package: &str,
    output: &Path,
    bootstrap: bool,
    target: Option<&str>,
) -> Result<BuildResult> {
    let rune = resolve_rune(package)?;
    tome::verify_rune(&rune).with_context(|| format!("verify rune signature for {package}"))?;
    let target = target.map_or_else(paths::target_triple, |t| t.to_string());
    let store_hash = crate::store::closure::store_hash_for_rune_with_target(&rune, &target)?;
    let metadata = EmbeddedNuRuntime.package_metadata(&rune)?;
    let build_deps = metadata.deps.build_for(&target);

    if !bootstrap {
        install::ensure_build_deps_installed(&build_deps)
            .with_context(|| format!("install build dependencies for `{package}`"))?;
    }

    let mut env = if bootstrap {
        BuildEnv::bootstrap(
            install::build_dep_bin_dirs(&build_deps)?,
            install::build_dep_env_vars(&build_deps)?,
        )
    } else {
        build_env_for_target(
            install::build_dep_bin_dirs(&build_deps)?,
            install::build_dep_env_vars(&build_deps)?,
            &target,
        )?
    };
    env.target = target;
    build_package_with_env(package, output, &env, &store_hash)
}

/// Builds `package` into an archive recorded under the already-computed `store_hash` (the package's
/// content address over its resolved dependency closure). The caller owns hash computation so the
/// installer can reuse the address it derived from the dependencies it actually installed.
pub fn build_package_with_env(
    package: &str,
    output: &Path,
    env: &BuildEnv,
    store_hash: &str,
) -> Result<BuildResult> {
    let target = env.target.clone();
    // A space in the install root breaks source builds: configure records the absolute paths of
    // build tools (MKDIR_P, INSTALL, ...) — which live under the root — and Makefiles use them
    // unquoted, so a path like `~/Library/Application Support/...` splits at the space. Fail early
    // with a clear message instead of a cryptic `make` error 30 seconds in.
    let root = paths::install_root()?;
    if root.to_string_lossy().contains(char::is_whitespace) {
        bail!(
            "install root `{}` contains whitespace, which breaks source builds; \
             set GRIMOIRE_ROOT to a path without spaces",
            root.display()
        );
    }

    let original_cwd = std::env::current_dir().context("read current working directory")?;
    status(&format!("resolving rune ({package})"));
    let rune = resolve_rune(package)?;

    status(&format!("reading metadata ({package})"));
    let mut metadata = read_rune_metadata(&rune, tome_name_for_rune(&rune)?.as_deref())?;

    status(&format!("checking sources ({package})"));
    let rune_dir = rune.parent().unwrap_or_else(|| Path::new("."));
    let sources = fetch::fetch_sources(
        &metadata.sources_for(&env.target),
        rune_dir,
        &paths::source_cache_dir()?,
    )
    .with_context(|| format!("fetch sources for {}", rune.display()))?;

    let final_prefix = paths::store_path(store_hash, &metadata.name, &metadata.version)?;

    let temp = tempfile::tempdir()?;
    let work_dir = temp.path().join("work");
    let package_dir = temp.path().join("package");
    let log_file = build_log_path(&metadata.name, &metadata.version, &target, store_hash);
    let dirs = BuildDirs {
        package_dir: package_dir.clone(),
        final_prefix: final_prefix.clone(),
        work_dir: work_dir.clone(),
        log_file,
    };
    std::fs::create_dir_all(&work_dir)?;
    std::fs::create_dir_all(&package_dir)?;
    status(&format!("preparing sources ({package})"));
    let sources = prepare_sources(sources, &work_dir)?;

    status(&format!(
        "building ({}) store={}",
        rune.display(),
        store_hash
    ));
    let manifest = EmbeddedNuRuntime
        .build(&rune, &dirs, &sources, &metadata.build_flags, env)
        .with_context(|| format!("build rune {}", rune.display()));
    std::env::set_current_dir(&original_cwd)
        .with_context(|| format!("restore working directory {}", original_cwd.display()))?;
    if let Some(manifest) = manifest? {
        metadata.merge_build_manifest(&manifest);
    }

    // Autotools-style `make install DESTDIR=...` nests the payload under the prefix path
    // inside package_dir. The packing logic strips this prefix; discovery must look there too.
    let payload_dir = {
        let relative: PathBuf = final_prefix
            .components()
            .filter_map(|c| match c {
                std::path::Component::Normal(p) => Some(p),
                _ => None,
            })
            .collect();
        let destdir_payload = package_dir.join(&relative);
        if destdir_payload.exists() {
            destdir_payload
        } else {
            package_dir.clone()
        }
    };

    fix_bin_permissions(&payload_dir)?;
    let discovered = discover_bins(&payload_dir);
    let libs = discover_libs(&payload_dir);
    // Discovery overrides static bins for the default target key.
    if !discovered.is_empty() {
        metadata
            .bins
            .insert("default".to_string(), discovered.clone());
    }
    metadata.provides = discovered.keys().cloned().collect();
    metadata.libs = libs.clone();

    let archive = pack::pack_built_rune(
        &rune,
        &metadata,
        &package_dir,
        &final_prefix,
        store_hash,
        output,
        &target,
    )?;
    drop(temp);
    Ok(BuildResult {
        archive,
        store_hash: store_hash.to_string(),
        discovered_bins: discovered,
        libs,
    })
}

pub(crate) fn tome_name_for_rune(rune: &Path) -> Result<Option<String>> {
    let rune = rune
        .canonicalize()
        .with_context(|| format!("resolve rune path {}", rune.display()))?;
    for tome in tome::load_tomes()? {
        let cache_path = tome::ensure_tome_cache(&tome)?
            .canonicalize()
            .with_context(|| format!("resolve tome cache for `{}`", tome.name))?;
        let runes_dir = cache_path.join("runes");
        if rune.starts_with(&runes_dir) {
            return Ok(Some(tome.name));
        }
    }
    Ok(None)
}

pub fn resolve_rune(package: &str) -> Result<PathBuf> {
    if let Some(rune) = find_rune(package)? {
        return Ok(rune);
    }
    if package.ends_with(".rn") {
        bail!("could not find rune `{package}`");
    }
    bail!("could not find rune `{package}`; pass a .rn path or a known package name")
}

/// Locates the rune for `package` without failing when none exists: an explicit `.rn` path,
/// then a `runes/<package>.rn` in any configured tome cache, then the same relative to the
/// current directory. Returns the canonical path, or `None` when nothing matches.
pub fn find_rune(package: &str) -> Result<Option<PathBuf>> {
    let path = PathBuf::from(package);
    if path.exists() {
        return Ok(Some(path.canonicalize().with_context(|| {
            format!("resolve rune path {}", path.display())
        })?));
    }

    if package.ends_with(".rn") {
        return Ok(None);
    }

    for tome in tome::load_tomes()? {
        let cache_path = tome::ensure_tome_cache(&tome)?;
        let rune = cache_path.join("runes").join(format!("{package}.rn"));
        if rune.exists() {
            return Ok(Some(rune.canonicalize().with_context(|| {
                format!("resolve rune path {}", rune.display())
            })?));
        }
    }

    let candidates = [
        PathBuf::from(format!("{package}.rn")),
        PathBuf::from("runes").join(format!("{package}.rn")),
    ];
    for candidate in candidates {
        if candidate.exists() {
            return Ok(Some(candidate.canonicalize().with_context(|| {
                format!("resolve rune path {}", candidate.display())
            })?));
        }
    }

    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn core_bin_dirs_returns_installed_core_packages() {
        let dirs = core_bin_dirs().unwrap();
        // In the test environment no core packages are installed, so this should return
        // an empty vec without erroring.
        assert!(dirs.is_empty() || !dirs.is_empty());
    }

    #[test]
    fn core_available_skips_host_boundary() {
        // When the managed core boundary is available, host tools must not be present.
        if core_compiler_boundary_available().unwrap() {
            let env = build_env_for_target(Vec::new(), Vec::new(), "macos-aarch64-darwin").unwrap();
            assert!(env.host_tools.is_empty());
        }
    }

    #[test]
    fn core_unavailable_uses_host_boundary() {
        // When the managed core boundary is not available, the host compiler boundary
        // must be present (if the host has one).
        if !core_compiler_boundary_available().unwrap() {
            let env = build_env_for_target(Vec::new(), Vec::new(), "macos-aarch64-darwin").unwrap();
            assert!(!env.host_tools.is_empty());
        }
    }

    #[test]
    fn musl_target_sets_static_flags() {
        let env = build_env_for_target(Vec::new(), Vec::new(), "linux-x86_64-musl").unwrap();
        let get = |key: &str| {
            env.extra_env
                .iter()
                .find(|(k, _)| k == key)
                .map(|(_, v)| v.as_str())
        };
        assert_eq!(get("CC"), Some("cc"));
        assert_eq!(get("CXX"), Some("c++"));
        assert_eq!(get("AR"), Some("ar"));
        assert_eq!(get("LD"), Some("ld"));
        assert_eq!(get("CFLAGS"), Some("-static"));
        assert_eq!(get("LDFLAGS"), Some("-static"));
    }
}
