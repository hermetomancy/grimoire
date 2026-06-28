//! Managed build environment construction.

use anyhow::{Result, bail};
use std::path::PathBuf;

use crate::{install, nu::runtime::BuildEnv, util::paths};

/// Core packages whose `bin/` directories are always prepended to build PATH — the managed build
/// floor. The toolchain (compiler, cmake, gmake, python3) plus the userland: a POSIX shell, awk,
/// coreutils, sed, grep, and tar, enough for autotools/Makefile builds without the host
/// `/usr/bin` tail.
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
    "gtar",
    // find/cmp: the base POSIX tools the generated configure/Makefile contract assumes (uutils
    // ships neither), e.g. git's build and the autotools move-if-change idiom.
    "findutils",
    "diffutils",
];

/// Returns `true` when `target` is a Linux musl triple.
pub fn is_musl_target(target: &str) -> bool {
    target.starts_with("linux-") && target.ends_with("-musl")
}

fn target_has_build_env(target: &str) -> bool {
    is_musl_target(target) || target.starts_with("macos-")
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
    if !target_has_build_env(target) {
        bail!(
            "target `{target}` is recognized but its managed build environment is not wired yet; supported source-build targets today are linux-*-musl and macos-*-darwin"
        );
    }
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
        //
        // Two injection paths must both be suppressed for libcxx's own build: the env-level one
        // below, and the `c++`/`g++` wrappers, which *also* bake a `-isystem` to the installed libc++
        // (toolchain-wrappers.rn). With both an installed and a fresh in-tree libc++ on the include
        // path they share `<string.h>`'s include guard, so the installed tree shadows the fresh one's
        // `#include_next` and musl's C declarations never resolve. This is the single place that
        // knows the package name, so it sets the flag the wrapper reads (GRIMOIRE_SUPPRESS_LIBCXX).
        if package_name == "libcxx" {
            env.push(("GRIMOIRE_SUPPRESS_LIBCXX".to_string(), "1".to_string()));
        } else if let Some(libcxx) = prefix("libcxx") {
            inject_libcxx_flags(&mut env, &libcxx);
        }
    }
    if target.starts_with("macos-") {
        if let Some(sdk) = super::toolchain::macos_sdk_path() {
            env.push(("SDKROOT".to_string(), sdk));
        }
        // Pin the deployment target so configure scripts (CPython's, autotools') don't shell out to
        // host `sw_vers` to guess it — unavailable in hermetic builds, and on a host it bakes the builder's
        // own OS version in as the minimum. 11.0 is the Apple-Silicon baseline. Like SDKROOT, this
        // is injected as env and so never enters the content address.
        env.push(("MACOSX_DEPLOYMENT_TARGET".to_string(), "11.0".to_string()));
    }

    if managed_boundary {
        Ok(BuildEnv::managed(all_path_dirs, Vec::new(), env))
    } else {
        Ok(BuildEnv::managed(
            all_path_dirs,
            super::toolchain::source_build_host_tools()?,
            env,
        ))
    }
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
        let _dirs = core_bin_dirs().unwrap();
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

    #[test]
    fn unwired_targets_fail_before_build_env_construction() {
        for target in ["linux-x86_64-gnu", "freebsd-x86_64-unknown"] {
            let err = build_env_for_target(Vec::new(), Vec::new(), target, "pkg").unwrap_err();
            assert!(
                err.to_string()
                    .contains("managed build environment is not wired yet"),
                "unexpected error for {target}: {err:#}"
            );
        }
    }

    #[test]
    fn libcxx_own_build_sets_the_wrapper_suppress_flag() {
        // libcxx's own build must signal the wrapper to drop its baked libc++ -isystem, else the
        // installed libc++ shadows the fresh in-tree headers' #include_next against musl.
        let env =
            build_env_for_target(Vec::new(), Vec::new(), "linux-x86_64-musl", "libcxx").unwrap();
        let get = |k: &str| {
            env.extra_env
                .iter()
                .find(|(key, _)| key == k)
                .map(|(_, v)| v.as_str())
        };
        assert_eq!(get("GRIMOIRE_SUPPRESS_LIBCXX"), Some("1"));
        // A normal C++ package on musl does not get the suppress flag.
        let other =
            build_env_for_target(Vec::new(), Vec::new(), "linux-x86_64-musl", "cmake").unwrap();
        assert!(
            other
                .extra_env
                .iter()
                .all(|(k, _)| k != "GRIMOIRE_SUPPRESS_LIBCXX")
        );
    }
}
