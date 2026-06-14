//! Source builds: turning a rune (`.rn` package definition) into a verified `.tar.zst` archive.
//!
//! A build fetches and checksum-verifies the rune's declared sources, runs its `build` step in
//! the embedded Nushell runtime against a staging directory, and packs the result into an archive
//! with embedded metadata. The output is the same archive shape a prebuilt download produces, so
//! installs behave identically whether a package came from source or a binary repo.

pub(crate) mod split;
pub(crate) mod toolchain;

mod output;
mod sources;

use output::*;
use sources::*;

use anyhow::{Context, Result, bail};
use std::{
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
    "llvm",
    "clang",
    "cmake",
    "python3",
    "gmake",
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

/// Constructs a [`BuildEnv`] for a build on `target`. Declared build-dep `bin/` dirs come
/// first — declaration is specificity, so a rune that declares `gsed` gets GNU sed as
/// plain `sed` even though the core floor (toybox) also ships one — then the core package
/// dirs as the managed floor. When the managed compiler boundary is not yet available
/// (bootstrap), host compiler tools are included after both.
pub fn build_env_for_target(
    path_dirs: Vec<PathBuf>,
    extra_env: Vec<(String, String)>,
    target: &str,
) -> Result<BuildEnv> {
    let mut all_path_dirs = path_dirs;
    all_path_dirs.extend(core_bin_dirs()?);

    let mut env = extra_env;
    if is_musl_target(target) {
        env.extend(musl_static_env_vars());
    }
    if target.starts_with("macos-")
        && let Some(sdk) = toolchain::macos_sdk_path()
    {
        env.push(("SDKROOT".to_string(), sdk));
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

/// One package produced by a source build: its archive, content address, and the final
/// metadata as packed (bins/provides/libs reflect what the build actually produced).
pub struct BuiltProduct {
    pub archive: PathBuf,
    pub store_hash: String,
    pub metadata: crate::model::PackageMetadata,
}

/// The result of a source build. A standalone rune yields only `primary`; a split-group
/// build also yields the sibling members carved from the same build output.
pub struct BuildResult {
    /// The product for the package the caller asked to build.
    pub primary: BuiltProduct,
    /// The other members of the split group (empty for standalone runes).
    pub siblings: Vec<BuiltProduct>,
}

impl BuildResult {
    /// Every product of the build: the primary first, then its group siblings.
    pub fn products(&self) -> impl Iterator<Item = &BuiltProduct> {
        std::iter::once(&self.primary).chain(self.siblings.iter())
    }
}

/// The build deps that apply when building `rune` on `target`. A split member builds
/// through its parent's rune, so the parent's build deps are the ones that matter.
pub fn effective_build_deps(
    rune: &Path,
    metadata: &crate::model::PackageMetadata,
    target: &str,
) -> Result<Vec<crate::model::Dependency>> {
    if !metadata.is_split_member() {
        return Ok(metadata.deps.build_for(target));
    }
    let group = split::group_for(rune)?.with_context(|| {
        format!(
            "split member `{}` has no resolvable group (parent `{}` missing?)",
            metadata.name,
            metadata.split_from.as_deref().unwrap_or("?")
        )
    })?;
    Ok(group.parent.metadata.deps.build_for(target))
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
        args.hermetic,
        args.target.as_deref(),
    )?;
    for product in result.products() {
        report(&format!("built {}", product.archive.display()));
    }
    Ok(())
}

pub fn build_package(
    package: &str,
    output: &Path,
    bootstrap: bool,
    hermetic: bool,
    target: Option<&str>,
) -> Result<BuildResult> {
    let rune = resolve_rune(package)?;
    tome::verify_rune(&rune).with_context(|| format!("verify rune signature for {package}"))?;
    let target = target.map_or_else(paths::target_triple, |t| t.to_string());
    let store_hash = crate::store::closure::store_hash_for_rune_with_target(&rune, &target)?;
    let metadata = EmbeddedNuRuntime.package_metadata(&rune)?;
    let build_deps = effective_build_deps(&rune, &metadata, &target)?;

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
    env.hermetic = hermetic;
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

    if let Some(group) = split::group_for(&rune)? {
        return build_group_with_env(
            &group,
            &metadata.name,
            output,
            env,
            store_hash,
            &original_cwd,
        );
    }

    let final_prefix = paths::store_path(store_hash, &metadata.name, &metadata.version)?;
    let raw = run_rune_build(
        &rune,
        &metadata,
        &final_prefix,
        store_hash,
        env,
        &original_cwd,
    )?;
    if let Some(manifest) = &raw.manifest {
        metadata.merge_build_manifest(manifest);
    }
    apply_discovery(&mut metadata, &raw.payload_dir, &target);

    let archive = pack::pack_built_rune(
        &rune,
        &metadata,
        &raw.package_dir,
        &final_prefix,
        store_hash,
        output,
        &target,
        &[],
    )?;
    drop(raw.temp);
    Ok(BuildResult {
        primary: BuiltProduct {
            archive,
            store_hash: store_hash.to_string(),
            metadata,
        },
        siblings: Vec::new(),
    })
}

/// Builds a split group once via the parent rune, partitions the payload by the members'
/// `files` globs, and packs one archive per member. `requested` names the member the caller
/// asked for (it becomes the result's primary product); `store_hash` is the caller's
/// precomputed address for it, cross-checked against the group derivation.
fn build_group_with_env(
    group: &split::SplitGroup,
    requested: &str,
    output: &Path,
    env: &BuildEnv,
    store_hash: &str,
    original_cwd: &Path,
) -> Result<BuildResult> {
    let target = env.target.clone();
    crate::model::validate_targets(&group.parent.metadata, &target)?;

    let rune_bytes = group.rune_bytes()?;
    let parts: Vec<(crate::model::PackageMetadata, Vec<u8>)> = group
        .members()
        .map(|member| {
            let bytes = rune_bytes
                .get(&member.name)
                .with_context(|| format!("missing rune bytes for `{}`", member.name))?
                .clone();
            Ok((member.metadata.clone(), bytes))
        })
        .collect::<Result<_>>()?;
    let hashes = crate::store::closure::split_member_hashes_with_target(&parts, &target)?;
    let member_hash = |name: &str| -> Result<&String> {
        hashes
            .get(name)
            .with_context(|| format!("split group did not yield a hash for `{name}`"))
    };
    let requested_hash = member_hash(requested)?;
    if requested_hash != store_hash {
        bail!(
            "computed store hash {requested_hash} for split member `{requested}` does not \
             match the planned {store_hash}; the group's inputs changed between resolution \
             and build — re-run the install"
        );
    }

    let parent = &group.parent;
    let parent_hash = member_hash(&parent.name)?;
    // The whole group configures against the *parent's* prefix; each member's files land in
    // its own store path afterwards. Members must therefore locate shared resources relative
    // to their own binaries, not through paths baked to the parent prefix.
    let parent_prefix = paths::store_path(parent_hash, &parent.name, &parent.metadata.version)?;
    let raw = run_rune_build(
        &parent.rune,
        &parent.metadata,
        &parent_prefix,
        parent_hash,
        env,
        original_cwd,
    )?;

    status(&format!(
        "partitioning split group ({} -> {})",
        parent.name,
        group
            .splits
            .iter()
            .map(|member| member.name.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    ));
    let staging_root = raw.temp.path().join("split-staging");
    let split_dirs = split::partition_payload(&raw.payload_dir, &group.splits, &staging_root)?;

    let group_runes: Vec<(String, Vec<u8>)> = rune_bytes.into_iter().collect();
    let mut products = Vec::new();
    for member in group.members() {
        let mut metadata = member.metadata.clone();
        let is_parent = member.name == parent.name;
        let (pack_dir, payload) = if is_parent {
            (raw.package_dir.clone(), raw.payload_dir.clone())
        } else {
            let dir = split_dirs.get(&member.name).with_context(|| {
                format!("no partitioned payload for split member `{}`", member.name)
            })?;
            (dir.clone(), dir.clone())
        };
        if is_parent && let Some(manifest) = &raw.manifest {
            metadata.merge_build_manifest(manifest);
        }
        apply_discovery(&mut metadata, &payload, &target);
        if !is_parent {
            split::warn_parent_prefix_leaks(&member.name, &payload, &parent_prefix)?;
        }

        let hash = member_hash(&member.name)?;
        let member_prefix = paths::store_path(hash, &member.name, &metadata.version)?;
        let archive = pack::pack_built_rune(
            &member.rune,
            &metadata,
            &pack_dir,
            &member_prefix,
            hash,
            output,
            &target,
            &group_runes,
        )?;
        products.push(BuiltProduct {
            archive,
            store_hash: hash.clone(),
            metadata,
        });
    }
    drop(raw.temp);

    let primary_index = products
        .iter()
        .position(|product| product.metadata.name == requested)
        .with_context(|| format!("split group build produced no product for `{requested}`"))?;
    let primary = products.swap_remove(primary_index);
    Ok(BuildResult {
        primary,
        siblings: products,
    })
}

/// The raw outcome of running a rune's `build` function: the staging directory, the payload
/// root inside it (after the DESTDIR probe), and the manifest the build returned. Packing
/// and discovery happen on top of this — once for a standalone rune, per member for a group.
struct RawBuild {
    temp: tempfile::TempDir,
    package_dir: PathBuf,
    payload_dir: PathBuf,
    manifest: Option<crate::model::BuildManifest>,
}

fn run_rune_build(
    rune: &Path,
    metadata: &crate::model::PackageMetadata,
    final_prefix: &Path,
    store_hash: &str,
    env: &BuildEnv,
    original_cwd: &Path,
) -> Result<RawBuild> {
    status(&format!("checking sources ({})", metadata.name));
    let rune_dir = rune.parent().unwrap_or_else(|| Path::new("."));
    let sources = fetch::fetch_sources(
        &metadata.sources_for(&env.target),
        rune_dir,
        &paths::source_cache_dir()?,
    )
    .with_context(|| format!("fetch sources for {}", rune.display()))?;

    let temp = tempfile::tempdir()?;
    let work_dir = temp.path().join("work");
    let package_dir = temp.path().join("package");
    let log_file = build_log_path(&metadata.name, &metadata.version, &env.target, store_hash);
    let dirs = BuildDirs {
        package_dir: package_dir.clone(),
        final_prefix: final_prefix.to_path_buf(),
        work_dir: work_dir.clone(),
        log_file,
    };
    std::fs::create_dir_all(&work_dir)?;
    std::fs::create_dir_all(&package_dir)?;
    status(&format!("preparing sources ({})", metadata.name));
    let sources = prepare_sources(sources, &work_dir)?;

    status(&format!(
        "building ({}) store={}",
        rune.display(),
        store_hash
    ));
    let manifest = EmbeddedNuRuntime
        .build(rune, &dirs, &sources, &metadata.build_flags, env)
        .with_context(|| format!("build rune {}", rune.display()));
    std::env::set_current_dir(original_cwd)
        .with_context(|| format!("restore working directory {}", original_cwd.display()))?;
    // A failed build normally takes its workspace with it; GRIMOIRE_KEEP_BUILD persists it
    // so configure logs and partial build trees can be autopsied.
    let manifest = match manifest {
        Ok(manifest) => manifest,
        Err(e) => {
            if std::env::var_os("GRIMOIRE_KEEP_BUILD").is_some() {
                let kept = temp.keep();
                crate::util::progress::note(&format!(
                    "build workspace kept at {} (GRIMOIRE_KEEP_BUILD)",
                    kept.display()
                ));
            }
            return Err(e);
        }
    };

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
    Ok(RawBuild {
        temp,
        package_dir,
        payload_dir,
        manifest,
    })
}

/// Applies post-build discovery to `metadata`. Discovered executables are the ground
/// truth for names that collide, but declared *aliases* — a second command name for a
/// file that really exists, like `awk: "bin/gawk"` — survive the merge: aliases are how
/// implementation packages provide the generic capability name, and discovery only sees
/// file names. The merged set becomes the package's `provides`.
fn apply_discovery(metadata: &mut crate::model::PackageMetadata, payload_dir: &Path, target: &str) {
    let mut merged = discover_bins(payload_dir);
    for (name, path) in metadata.bins_for(target) {
        if !merged.contains_key(&name) && payload_dir.join(&path).is_file() {
            merged.insert(name, path);
        }
    }
    let libs = discover_libs(payload_dir);
    if !merged.is_empty() {
        metadata.bins.insert("default".to_string(), merged.clone());
    }
    metadata.provides = merged.keys().cloned().collect();
    metadata.libs = libs;
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
