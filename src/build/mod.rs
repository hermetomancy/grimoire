//! Source builds: turning a rune (`.rn` package definition) into a verified `.tar.zst` archive.
//!
//! A build fetches and checksum-verifies the rune's declared sources, runs its `build` step in
//! the embedded Nushell runtime against a staging directory, and packs the result into an archive
//! with embedded metadata. The output is the same archive shape a prebuilt download produces, so
//! installs behave identically whether a package came from source or a binary repo.

pub(crate) mod split;
pub(crate) mod toolchain;

mod env;
pub(crate) mod output;
mod sources;

pub use env::{build_env_for_target, is_musl_target, managed_floor_readiness};
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
    cli::BuildArgs,
    fetch, install,
    nu::runtime::{BuildDirs, BuildEnv, EmbeddedNuRuntime},
    tome,
    util::output::{report, status},
    util::paths,
};

/// Reads a rune's package metadata after verifying its signature.
/// `_tome_name` is kept until the addendum design settles, so callers do not grow two metadata
/// loading paths again.
pub fn read_rune_metadata(
    rune: &Path,
    _tome_name: Option<&str>,
) -> Result<crate::model::PackageMetadata> {
    tome::verify_rune(rune).with_context(|| format!("verify rune signature {}", rune.display()))?;
    EmbeddedNuRuntime
        .package_metadata(rune)
        .with_context(|| format!("read rune metadata {}", rune.display()))
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

struct BuildInvocation<'a> {
    output: &'a Path,
    env: &'a BuildEnv,
    store_hash: &'a str,
    resolved: &'a BTreeMap<String, String>,
    local_roots: &'a [PathBuf],
    original_cwd: &'a Path,
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
    let hermetic = effective_source_build_hermetic(args.bootstrap, args.impure)?;
    let result = build_package(
        &args.package,
        &args.output,
        args.bootstrap,
        hermetic,
        args.target.as_deref(),
    )?;
    for product in result.products() {
        report(&format!("built {}", product.archive.display()));
    }
    Ok(())
}

/// Source builds can drop the ambient POSIX tail only after the cached `build-env` contract is
/// installed. Before that point, bootstrap builds still need `/usr/bin` and `/bin` to realize the
/// floor itself, and their store hashes use the impure build-environment identity.
pub fn effective_source_build_hermetic(bootstrap: bool, impure: bool) -> Result<bool> {
    Ok(source_build_hermetic_with_floor(
        bootstrap,
        impure,
        env::managed_floor_available()?,
    ))
}

fn source_build_hermetic_with_floor(bootstrap: bool, impure: bool, floor_available: bool) -> bool {
    !bootstrap && !impure && floor_available
}

pub fn build_package(
    package: &str,
    output: &Path,
    bootstrap: bool,
    hermetic: bool,
    target: Option<&str>,
) -> Result<BuildResult> {
    let rune = resolve_rune(package)?;
    let local_roots = tome_roots_for_rune(&rune);
    let target = target.map_or_else(paths::target_triple, |t| t.to_string());
    let metadata = read_rune_metadata(&rune, tome_name_for_rune(&rune)?.as_deref())?;
    let build_deps = effective_build_deps(&rune, &metadata, &target)?;

    if !bootstrap {
        install::ensure_build_deps_installed_with_mode_and_roots(
            &build_deps,
            hermetic,
            &local_roots,
        )
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
            &metadata.name,
        )?
    };
    env.target = target;
    env.hermetic = hermetic;
    // The published/standalone address must be a pure function of the runes and the resolved
    // closures (canonical `of_name`), never the publisher's ambient installed state — otherwise
    // `tome build` and `tome build --index` would key the same archive differently and a clean
    // consumer would miss the substitute (AGENTS §9.8). The install paths own a resolved closure
    // and pass it explicitly through `build_package_with_env`; here it is empty. The build env id
    // is taken from the actual env so `--impure` has a distinct address.
    let build_env_id = toolchain::store_build_env_id_for_target(env.hermetic, &env.target);
    let store_hash = crate::store::closure::store_hash_for_rune_with_target_env_and_roots(
        &rune,
        &env.target,
        &build_env_id,
        &BTreeMap::new(),
        &local_roots,
    )?;
    build_package_with_env(package, output, &env, &store_hash, &BTreeMap::new())
}

/// Builds `package` into an archive recorded under the already-computed `store_hash` (the package's
/// content address over its resolved dependency closures). The caller owns hash computation so the
/// installer can reuse the address it derived from the dependencies it actually installed. For a
/// split group, `resolved` is the closure that produced `store_hash` (the installer's actual
/// closure; empty for a canonical publisher/standalone build), so the recompute-and-cross-check
/// addresses the members identically.
pub fn build_package_with_env(
    package: &str,
    output: &Path,
    env: &BuildEnv,
    store_hash: &str,
    resolved: &BTreeMap<String, String>,
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
    let local_roots = tome_roots_for_rune(&rune);

    status(&format!("reading metadata ({package})"));
    let mut metadata = read_rune_metadata(&rune, tome_name_for_rune(&rune)?.as_deref())?;

    if let Some(group) = split::group_for(&rune)? {
        let build = BuildInvocation {
            output,
            env,
            store_hash,
            resolved,
            local_roots: &local_roots,
            original_cwd: &original_cwd,
        };
        return build_group_with_env(&group, &metadata.name, build);
    }

    // Mirror the split-group path: independently recompute the address from the rune and the
    // resolved closure, and refuse to lay out a store prefix that disagrees with the planned hash.
    // A silent mis-address would otherwise surface only later as a dropped binary substitution
    // (AGENTS §9.8).
    let build_env_id = build_env_id_for_resolved(env.hermetic, &target, resolved);
    let recomputed = crate::store::closure::store_hash_for_rune_with_target_env_and_roots(
        &rune,
        &target,
        &build_env_id,
        resolved,
        &local_roots,
    )
    .with_context(|| format!("recompute store hash for `{}`", metadata.name))?;
    if recomputed != store_hash {
        bail!(
            "computed store hash {recomputed} for `{}` does not match the planned {store_hash}; \
             its inputs changed between resolution and build — re-run the install",
            metadata.name
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
    build: BuildInvocation<'_>,
) -> Result<BuildResult> {
    let target = build.env.target.clone();
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
    // Recompute the group's member addresses against `resolved` — the closure that produced the
    // planned `store_hash` (the installer's actual closure, or empty for a canonical build) — then
    // cross-check the requested member below.
    let build_env_id = build_env_id_for_resolved(build.env.hermetic, &target, build.resolved);
    let hashes = crate::store::closure::split_member_hashes_with_target_env_and_roots(
        &parts,
        &target,
        &build_env_id,
        build.resolved,
        build.local_roots,
    )?;
    let member_hash = |name: &str| -> Result<&String> {
        hashes
            .get(name)
            .with_context(|| format!("split group did not yield a hash for `{name}`"))
    };
    let requested_hash = member_hash(requested)?;
    if requested_hash != build.store_hash {
        bail!(
            "computed store hash {requested_hash} for split member `{requested}` does not \
             match the planned {}; the group's inputs changed between resolution \
             and build — re-run the install",
            build.store_hash
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
        build.env,
        build.original_cwd,
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
            build.output,
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

pub fn build_env_id_for_resolved(
    hermetic: bool,
    target: &str,
    resolved: &BTreeMap<String, String>,
) -> String {
    if resolved.is_empty() {
        toolchain::store_build_env_id_for_target(hermetic, target)
    } else {
        toolchain::store_build_env_id_for_target_with_resolved(hermetic, target, resolved)
    }
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

    // Build scratch lives under a disk-backed root, not `$TMPDIR`/`/tmp` — the latter is a small
    // tmpfs on many hosts and an llvm-sized build overflows it (`No space left on device`).
    let build_tmp = paths::build_tmp_dir()?;
    std::fs::create_dir_all(&build_tmp)
        .with_context(|| format!("create build scratch root {}", build_tmp.display()))?;
    let temp = tempfile::tempdir_in(&build_tmp)?;
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
                crate::util::output::note(&format!(
                    "build workspace kept at {} (GRIMOIRE_KEEP_BUILD)",
                    kept.display()
                ));
            }
            return Err(e);
        }
    };

    // Discovery must inspect the same tree packing will — the one shared helper, so a DESTDIR
    // payload (or its absence) is resolved identically on both sides.
    let payload_dir = pack::package_payload_dir(&package_dir, final_prefix)?;

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
    let mut libs = metadata.libs.clone();
    libs.extend(discover_libs(payload_dir));
    libs.sort();
    libs.dedup();
    if !merged.is_empty() {
        metadata.bins.insert("default".to_string(), merged.clone());
    }
    metadata.provides.extend(merged.into_keys());
    metadata.provides.sort();
    metadata.provides.dedup();
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
    // A rune is always a file. Testing `is_file()` rather than `exists()` keeps a directory whose
    // name collides with `package` from being accepted as a rune — e.g. a `grimoire/` source
    // checkout in the cwd while resolving the package `grimoire`, which would otherwise be read as
    // rune metadata and fail with a cryptic `Is a directory (os error 21)`.
    let path = PathBuf::from(package);
    if path.is_file() {
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
        if rune.is_file() {
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
        if candidate.is_file() {
            return Ok(Some(candidate.canonicalize().with_context(|| {
                format!("resolve rune path {}", candidate.display())
            })?));
        }
    }

    Ok(None)
}

pub(crate) fn find_rune_prefer_roots(package: &str, roots: &[PathBuf]) -> Result<Option<PathBuf>> {
    let path = PathBuf::from(package);
    if path.is_file() {
        return Ok(Some(path.canonicalize().with_context(|| {
            format!("resolve rune path {}", path.display())
        })?));
    }
    if package.ends_with(".rn") {
        return Ok(None);
    }
    if let Some(rune) = find_rune_in_roots(package, roots)? {
        return Ok(Some(rune));
    }
    find_rune(package)
}

pub(crate) fn find_rune_in_roots(package: &str, roots: &[PathBuf]) -> Result<Option<PathBuf>> {
    for root in roots {
        let rune = root.join("runes").join(format!("{package}.rn"));
        if rune.is_file() {
            return Ok(Some(rune.canonicalize().with_context(|| {
                format!("resolve rune path {}", rune.display())
            })?));
        }
    }
    Ok(None)
}

fn tome_roots_for_rune(rune: &Path) -> Vec<PathBuf> {
    let Some(runes_dir) = rune.parent() else {
        return Vec::new();
    };
    if runes_dir.file_name().and_then(|name| name.to_str()) != Some("runes") {
        return Vec::new();
    }
    runes_dir
        .parent()
        .map(|root| vec![root.to_path_buf()])
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn metadata_with_provides(provides: &[&str]) -> crate::model::PackageMetadata {
        crate::model::PackageMetadata {
            name: "toolpkg".to_owned(),
            version: "1.0.0".to_owned(),
            target: None,
            store_path: None,
            targets: Vec::new(),
            fixed_output: false,
            build_only: false,
            summary: None,
            bins: BTreeMap::new(),
            sources: BTreeMap::new(),
            deps: Default::default(),
            build_flags: BTreeMap::new(),
            provides: provides.iter().map(|p| (*p).to_owned()).collect(),
            libs: Vec::new(),
            notes: Vec::new(),
            upstream_version: None,
            conflicts: Vec::new(),
            replaces: Vec::new(),
            split_from: None,
            files: Vec::new(),
        }
    }

    #[test]
    fn discovery_preserves_declared_non_bin_provides() {
        let temp = tempfile::tempdir().unwrap();
        let bin = temp.path().join("bin");
        std::fs::create_dir(&bin).unwrap();
        let tool = bin.join("tool");
        std::fs::write(&tool, b"#!/usr/bin/env sh\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&tool).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&tool, perms).unwrap();
        }

        let mut metadata = metadata_with_provides(&["text-processor"]);

        apply_discovery(&mut metadata, temp.path(), "linux-x86_64-musl");
        assert_eq!(
            metadata.provides,
            vec!["text-processor".to_owned(), "tool".to_owned()]
        );
    }

    #[test]
    fn discovery_preserves_declared_non_file_libs() {
        let temp = tempfile::tempdir().unwrap();
        let lib = temp.path().join("lib");
        std::fs::create_dir(&lib).unwrap();
        std::fs::write(lib.join("libreal.a"), b"archive").unwrap();

        let mut metadata = metadata_with_provides(&[]);
        metadata.libs = vec!["virtual-lib".to_owned()];

        apply_discovery(&mut metadata, temp.path(), "linux-x86_64-musl");
        assert_eq!(
            metadata.libs,
            vec!["real".to_owned(), "virtual-lib".to_owned()]
        );
    }

    #[test]
    fn source_builds_become_hermetic_only_after_the_floor_exists() {
        assert!(!source_build_hermetic_with_floor(true, false, true));
        assert!(!source_build_hermetic_with_floor(false, true, true));
        assert!(!source_build_hermetic_with_floor(false, false, false));
        assert!(source_build_hermetic_with_floor(false, false, true));
    }
}
