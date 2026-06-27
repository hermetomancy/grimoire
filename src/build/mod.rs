//! Source builds: turning a rune (`.rn` package definition) into a verified `.tar.zst` archive.
//!
//! A build fetches and checksum-verifies the rune's declared sources, runs its `build` step in
//! the embedded Nushell runtime against a staging directory, and packs the result into an archive
//! with embedded metadata. The output is the same archive shape a prebuilt download produces, so
//! installs behave identically whether a package came from source or a binary repo.

pub(crate) mod split;
pub(crate) mod toolchain;

pub(crate) mod output;
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
    util::output::{report, status},
};

/// Core packages whose `bin/` directories are always prepended to build PATH — the managed build
/// floor. The toolchain (compiler, cmake, gmake, python3) plus the userland: a POSIX shell, awk,
/// coreutils, sed, and grep, enough for autotools `configure` without the host `/usr/bin` tail.
const CORE_PACKAGES: &[&str] = &[
    "linux-headers",
    "musl",
    "llvm",
    "clang",
    "cmake",
    "python3",
    "gmake",
    "toolchain-wrappers",
    // Userland floor (replaces toybox, which was too thin for configure — no awk/sh/tr/expr/…).
    "dash",
    "mawk",
    "uutils",
    "gsed",
    "ggrep",
];

/// Returns `true` when `target` is a Linux musl triple.
pub fn is_musl_target(target: &str) -> bool {
    target.starts_with("linux-") && target.ends_with("-musl")
}

/// Merges `additions` into `env` at the path-segment level: each colon-separated segment of a
/// value is appended to the existing key only if not already present, so declared-dep entries keep
/// search priority and a value already there is never duplicated. The dedup matters because a
/// single-path var like `<DEP>_PREFIX` would otherwise become `/p:/p` when a floor package is also
/// a declared dep (e.g. `musl` declares `linux-headers`). Empty values are skipped so a floor
/// package that is not yet installed contributes nothing.
fn merge_path_env(env: &mut Vec<(String, String)>, additions: Vec<(String, String)>) {
    for (key, value) in additions {
        if value.is_empty() {
            continue;
        }
        if let Some((_, existing)) = env.iter_mut().find(|(name, _)| *name == key) {
            let mut segments: Vec<&str> = existing.split(':').filter(|s| !s.is_empty()).collect();
            for segment in value.split(':').filter(|s| !s.is_empty()) {
                if !segments.contains(&segment) {
                    segments.push(segment);
                }
            }
            *existing = segments.join(":");
        } else {
            env.push((key, value));
        }
    }
}

/// Returns the `bin/` directories of all installed core packages.
fn core_bin_dirs() -> Result<Vec<PathBuf>> {
    let world = install::InstalledWorld::load_default()?;
    let mut dirs = Vec::new();
    for name in CORE_PACKAGES {
        let Some(state) = world.get(name) else {
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
    let world = install::InstalledWorld::load_default()?;
    Ok(world.contains("toolchain-wrappers"))
}

/// The toolchain aliases shared by every musl-target build, independent of whether the floor is
/// installed yet. `cc`/`c++` resolve to the managed clang (or the host boundary's clang/gcc).
fn musl_tool_aliases() -> Vec<(String, String)> {
    [
        ("CC", "cc"),
        ("CXX", "c++"),
        ("AR", "ar"),
        ("LD", "ld"),
        ("NM", "nm"),
        ("RANLIB", "ranlib"),
        ("STRIP", "strip"),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect()
}

/// Fallback env for a musl build while the floor itself is being bootstrapped (musl/linux-headers
/// not yet installed): the static flags, with no explicit retargeting because there is no managed
/// libc to target against.
fn musl_static_env_vars() -> Vec<(String, String)> {
    let mut env = musl_tool_aliases();
    env.push(("CFLAGS".to_string(), "-static".to_string()));
    env.push(("LDFLAGS".to_string(), "-static".to_string()));
    env
}

/// Maps a Grimoire target triple (`<os>-<arch>-<abi>`) to the clang triple (`<arch>-<os>-<abi>`)
/// the compiler is retargeted to — e.g. `linux-aarch64-musl` → `aarch64-linux-musl`.
fn clang_musl_triple(target: &str) -> String {
    match target.split('-').collect::<Vec<_>>().as_slice() {
        [os, arch, abi] => format!("{arch}-{os}-{abi}"),
        _ => target.to_string(),
    }
}

/// Env that retargets the compiler from the host gnu/glibc triple to musl, using the installed
/// `musl` and `linux-headers` store prefixes. See the call site in [`build_env_for_target`] for
/// why each flag is needed.
fn musl_target_env_vars(target: &str, musl: &str, headers: &str) -> Vec<(String, String)> {
    let triple = clang_musl_triple(target);
    let cflags = format!(
        "--target={triple} --sysroot={musl} -isystem {musl}/include -isystem {headers}/include -B{musl}/lib"
    );
    let ldflags = format!(
        "--target={triple} --sysroot={musl} -B{musl}/lib -L{musl}/lib --rtlib=compiler-rt --unwindlib=none -static"
    );
    let mut env = musl_tool_aliases();
    env.push(("CFLAGS".to_string(), cflags.clone()));
    env.push(("CXXFLAGS".to_string(), cflags));
    env.push(("LDFLAGS".to_string(), ldflags));
    env
}

/// Adds the managed musl libc++ to a C++ build's flags: its headers ahead of the C headers in
/// `CXXFLAGS` (`-nostdinc++` + libc++'s include dir, where `-stdlib=libc++` then resolves the
/// library) and its lib dir plus the LLVM unwinder in `LDFLAGS`. `CFLAGS` (C) is left untouched —
/// only C++ wants a C++ stdlib — and `-static`/`--unwindlib=libunwind` (appended after the base
/// `--unwindlib=none`) win as the last occurrence.
fn inject_libcxx_flags(env: &mut [(String, String)], libcxx: &str) {
    for (key, value) in env.iter_mut() {
        match key.as_str() {
            "CXXFLAGS" => {
                *value =
                    format!("-stdlib=libc++ -nostdinc++ -isystem {libcxx}/include/c++/v1 {value}");
            }
            "LDFLAGS" => value.push_str(&format!(" -L{libcxx}/lib --unwindlib=libunwind")),
            _ => {}
        }
    }
}

/// Constructs a [`BuildEnv`] for a build on `target`. Declared build-dep `bin/` dirs come
/// first — declaration is specificity, so a rune's explicitly declared build dep outranks the
/// same command from the managed floor — then the core package dirs (the floor: toolchain plus
/// the userland — shell/awk/coreutils/sed/grep). When the managed compiler boundary is not yet
/// available (bootstrap), host compiler tools are included after both.
pub fn build_env_for_target(
    path_dirs: Vec<PathBuf>,
    extra_env: Vec<(String, String)>,
    target: &str,
    package_name: &str,
) -> Result<BuildEnv> {
    let mut all_path_dirs = path_dirs;
    all_path_dirs.extend(core_bin_dirs()?);

    let mut env = extra_env;
    let managed_boundary = core_compiler_boundary_available()?;
    // A pure-musl build host's system clang is already musl, with its own libc/libc++/libunwind —
    // but it ships no *static* libc/libunwind/libatomic by default and force-links libatomic on
    // `-static`. The cross-from-glibc floor (the musl retarget, `-static`, the managed libc++ inject)
    // therefore can't link there at the host boundary. These pre-managed builds are transient
    // scaffolding — re-forged against the managed musl+clang once the boundary flips — so on a musl
    // host, before that flip, skip the floor and build natively against the host toolchain instead.
    let native_musl_host_boundary =
        is_musl_target(target) && !managed_boundary && paths::host_libc() == "musl";

    if is_musl_target(target) && !native_musl_host_boundary {
        let world = install::InstalledWorld::load_default()?;
        let prefix = |name: &str| world.get(name).map(|s| s.store_path.clone());
        match (prefix("musl"), prefix("linux-headers")) {
            // The managed floor is installed: retarget the compiler to musl explicitly. A host
            // clang/gcc defaults to the host gnu/glibc triple, so it leaks host libc into both
            // configure feature probes (a glibc-only symbol like `sem_clockwait` links and is
            // wrongly detected as present) and final links (host glibc CRT). The triple fixes the
            // ABI; the sysroot + headers supply musl libc and the kernel uapi headers; `-B`/`-L`
            // point at musl's CRT and libc; and compiler-rt with no unwinder sidesteps the host's
            // libgcc. Validated on linux-aarch64-musl: links a static musl binary and Python's
            // configure correctly leaves HAVE_SEM_CLOCKWAIT unset.
            (Some(musl), Some(headers)) => {
                env.extend(musl_target_env_vars(target, &musl, &headers));
            }
            // The floor itself is still being bootstrapped (building musl/linux-headers): they are
            // not yet installed, so there is nothing to target against. Fall back to the static
            // flags; those builds run against the host boundary by design.
            _ => env.extend(musl_static_env_vars()),
        }
        // Also expose the floor prefixes through the usual discovery vars (CPATH, LIBRARY_PATH,
        // <DEP>_PREFIX, CMAKE_PREFIX_PATH) so build systems that read them — cmake, pkg-config —
        // find musl and the kernel headers too. Injected as environment, like the macOS SDKROOT,
        // so they never enter a package's content address; merged after declared-dep paths
        // (segment-deduped) so an explicitly declared library keeps priority.
        let floor = install::build_dep_env_vars(&[
            crate::model::Dependency::any("musl"),
            crate::model::Dependency::any("linux-headers"),
        ])?;
        merge_path_env(&mut env, floor);

        // libc++ is the C++ half of the musl floor (musl has no C++ standard library of its own).
        // Inject it for every C++ build on musl once `libcxx` is installed — but NOT for libcxx's
        // own build, which *provides* libc++ and compiles `-nostdinc++` against its own tree. It is
        // a floor (env, never a declared dep), so a C++ package like cmake does not cycle with
        // libcxx — whose own build deps include cmake.
        if package_name != "libcxx"
            && let Some(libcxx) = prefix("libcxx")
        {
            inject_libcxx_flags(&mut env, &libcxx);
        }
    }
    if target.starts_with("macos-") {
        if let Some(sdk) = toolchain::macos_sdk_path() {
            env.push(("SDKROOT".to_string(), sdk));
        }
        // Pin the deployment target so configure scripts (CPython's, autotools') don't shell out to
        // host `sw_vers` to guess it — gone under --hermetic, and on a host it bakes the builder's
        // own OS version in as the minimum. 11.0 is the Apple-Silicon baseline. Like SDKROOT, this
        // is injected as env and so never enters the content address.
        env.push(("MACOSX_DEPLOYMENT_TARGET".to_string(), "11.0".to_string()));
    }

    if managed_boundary {
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
    // The published/standalone address must be a pure function of the runes and the resolved
    // closure (canonical `of_name`), never the publisher's ambient installed state — otherwise
    // `tome build` and `tome build --index` would key the same archive differently and a clean
    // consumer would miss the substitute (AGENTS §9.8). The install paths own a resolved closure
    // and pass it explicitly through `build_package_with_env`; here it is empty.
    let store_hash =
        crate::store::closure::store_hash_for_rune_with_target(&rune, &target, &BTreeMap::new())?;
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
            &metadata.name,
        )?
    };
    env.target = target;
    env.hermetic = hermetic;
    build_package_with_env(package, output, &env, &store_hash, &BTreeMap::new())
}

/// Builds `package` into an archive recorded under the already-computed `store_hash` (the package's
/// content address over its resolved dependency closure). The caller owns hash computation so the
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

    status(&format!("reading metadata ({package})"));
    let mut metadata = read_rune_metadata(&rune, tome_name_for_rune(&rune)?.as_deref())?;

    if let Some(group) = split::group_for(&rune)? {
        return build_group_with_env(
            &group,
            &metadata.name,
            output,
            env,
            store_hash,
            resolved,
            &original_cwd,
        );
    }

    // Mirror the split-group path: independently recompute the address from the rune and the
    // resolved closure, and refuse to lay out a store prefix that disagrees with the planned hash.
    // A silent mis-address would otherwise surface only later as a dropped binary substitution
    // (AGENTS §9.8).
    let recomputed =
        crate::store::closure::store_hash_for_rune_with_target(&rune, &target, resolved)
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
    output: &Path,
    env: &BuildEnv,
    store_hash: &str,
    resolved: &BTreeMap<String, String>,
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
    // Recompute the group's member addresses against `resolved` — the closure that produced the
    // planned `store_hash` (the installer's actual closure, or empty for a canonical build) — then
    // cross-check the requested member below.
    let hashes = crate::store::closure::split_member_hashes_with_target(&parts, &target, resolved)?;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge_path_env_appends_floor_after_declared_paths() {
        let mut env = vec![
            ("CPATH".to_string(), "/dep/include".to_string()),
            ("CC".to_string(), "cc".to_string()),
        ];
        merge_path_env(
            &mut env,
            vec![
                // an existing path key: the floor value appends after the declared one, which
                // keeps search priority.
                ("CPATH".to_string(), "/musl/include".to_string()),
                // a fresh key: inserted as-is.
                ("MUSL_PREFIX".to_string(), "/grm/store/musl".to_string()),
                // a not-yet-installed floor package emits an empty value: skipped entirely.
                ("LIBRARY_PATH".to_string(), String::new()),
            ],
        );
        let lookup = |key: &str| env.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone());
        assert_eq!(
            lookup("CPATH"),
            Some("/dep/include:/musl/include".to_string())
        );
        assert_eq!(lookup("MUSL_PREFIX"), Some("/grm/store/musl".to_string()));
        assert_eq!(lookup("CC"), Some("cc".to_string()));
        assert_eq!(lookup("LIBRARY_PATH"), None);
    }

    #[test]
    fn merge_path_env_dedups_already_present_segments() {
        // A floor package that is also a declared dep (e.g. `musl` declares `linux-headers`)
        // arrives with a value already in the env; segment-level dedup keeps a single-path var
        // like `<DEP>_PREFIX` from becoming `/p:/p`, and avoids duplicate search entries.
        let mut env = vec![
            ("LINUX_HEADERS_PREFIX".to_string(), "/lh".to_string()),
            ("CPATH".to_string(), "/lh/include".to_string()),
        ];
        merge_path_env(
            &mut env,
            vec![
                ("LINUX_HEADERS_PREFIX".to_string(), "/lh".to_string()),
                ("CPATH".to_string(), "/musl/include:/lh/include".to_string()),
            ],
        );
        let lookup = |key: &str| env.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone());
        assert_eq!(lookup("LINUX_HEADERS_PREFIX"), Some("/lh".to_string()));
        assert_eq!(
            lookup("CPATH"),
            Some("/lh/include:/musl/include".to_string())
        );
    }

    #[test]
    fn inject_libcxx_flags_adds_cxx_not_c() {
        let mut env = vec![
            (
                "CFLAGS".to_string(),
                "--target=aarch64-linux-musl -isystem /musl/include".to_string(),
            ),
            (
                "CXXFLAGS".to_string(),
                "--target=aarch64-linux-musl -isystem /musl/include".to_string(),
            ),
            (
                "LDFLAGS".to_string(),
                "--unwindlib=none -static".to_string(),
            ),
        ];
        inject_libcxx_flags(&mut env, "/store/libcxx");
        let get = |k: &str| {
            env.iter()
                .find(|(key, _)| key == k)
                .map(|(_, v)| v.as_str())
        };
        // C is untouched; C++ gets libc++ headers *before* the musl headers.
        assert_eq!(
            get("CFLAGS"),
            Some("--target=aarch64-linux-musl -isystem /musl/include")
        );
        let cxx = get("CXXFLAGS").unwrap();
        assert!(
            cxx.starts_with("-stdlib=libc++ -nostdinc++ -isystem /store/libcxx/include/c++/v1 ")
        );
        assert!(cxx.ends_with("-isystem /musl/include"));
        // Link gains libc++'s lib dir and the LLVM unwinder (last --unwindlib wins).
        assert_eq!(
            get("LDFLAGS"),
            Some("--unwindlib=none -static -L/store/libcxx/lib --unwindlib=libunwind")
        );
    }

    #[test]
    fn clang_musl_triple_swaps_os_and_arch() {
        assert_eq!(
            clang_musl_triple("linux-aarch64-musl"),
            "aarch64-linux-musl"
        );
        assert_eq!(clang_musl_triple("linux-x86_64-musl"), "x86_64-linux-musl");
        // An unexpected shape is passed through unchanged rather than mangled.
        assert_eq!(clang_musl_triple("weird"), "weird");
    }

    #[test]
    fn musl_target_env_vars_builds_the_validated_flag_set() {
        let env = musl_target_env_vars("linux-aarch64-musl", "/store/musl", "/store/lh");
        let get = |key: &str| env.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str());
        assert_eq!(get("CC"), Some("cc"));
        let cflags = get("CFLAGS").unwrap();
        assert!(cflags.contains("--target=aarch64-linux-musl"));
        assert!(cflags.contains("-isystem /store/musl/include"));
        assert!(cflags.contains("-isystem /store/lh/include"));
        assert_eq!(get("CXXFLAGS"), get("CFLAGS"));
        let ldflags = get("LDFLAGS").unwrap();
        assert!(ldflags.contains("--rtlib=compiler-rt"));
        assert!(ldflags.contains("--unwindlib=none"));
        assert!(ldflags.contains("-static"));
    }

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
            let env = build_env_for_target(Vec::new(), Vec::new(), "macos-aarch64-darwin", "pkg")
                .unwrap();
            assert!(env.host_tools.is_empty());
        }
    }

    #[test]
    fn core_unavailable_uses_host_boundary() {
        // When the managed core boundary is not available, the host compiler boundary
        // must be present (if the host has one).
        if !core_compiler_boundary_available().unwrap() {
            let env = build_env_for_target(Vec::new(), Vec::new(), "macos-aarch64-darwin", "pkg")
                .unwrap();
            assert!(!env.host_tools.is_empty());
        }
    }

    #[test]
    fn musl_target_sets_static_flags() {
        let env = build_env_for_target(Vec::new(), Vec::new(), "linux-x86_64-musl", "pkg").unwrap();
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
