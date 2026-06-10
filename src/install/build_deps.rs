//! Build-dependency support: store-only installs of declared build deps and the managed
//! discovery environment (bin dirs, prefix variables) layered into rune builds.

use anyhow::{Context, Result, bail};
use std::{collections::HashSet, path::PathBuf};

use crate::{
    build, fetch,
    model::{Dependency, PackageState},
    solve,
    util::paths,
};

use super::*;

/// Ensures every build dependency in `deps` is installed store-only (no lockfile, no generation).
/// Missing deps are resolved through the solver and installed from substitutes or built from source.
/// Already-installed packages are reused.
pub(crate) fn ensure_build_deps_installed(deps: &[Dependency]) -> Result<()> {
    let mut building = HashSet::new();
    ensure_build_deps_installed_inner(deps, &mut building)
}

pub(crate) fn ensure_build_deps_installed_inner(
    deps: &[Dependency],
    building: &mut HashSet<String>,
) -> Result<()> {
    if deps.is_empty() {
        return Ok(());
    }

    let mut installed = installed_versions()?;
    let states = installed_states().unwrap_or_default();
    let missing: Vec<Dependency> = deps
        .iter()
        .filter(|dep| find_dep_state(&states, &dep.name).is_none())
        .cloned()
        .collect();

    if missing.is_empty() {
        return Ok(());
    }

    let mut plan = solve::resolve(&missing, &installed, None)?;
    plan.compute_store_hashes()
        .with_context(|| "compute store hashes for build dependencies")?;

    for step in plan.steps {
        if installed.contains_key(&step.name) {
            continue;
        }
        if !building.insert(step.name.clone()) {
            bail!("build dependency cycle detected involving `{}`", step.name);
        }

        let result = if let Some(sub) = step.substitutes.first() {
            let archive = fetch::fetch_verified(
                &sub.entry.archive,
                &sub.root,
                &sub.entry.archive_hash,
                &paths::archive_cache_dir()?,
                &format!("archive `{}` {}", sub.entry.name, sub.entry.version),
            )?;
            install_store_only(
                &archive,
                Some(sub.entry.archive_hash.clone()),
                Some(&sub.store_hash),
            )
        } else if let Some(rune) = &step.rune {
            let store_hash = crate::store::closure::store_hash_for_rune(rune)
                .with_context(|| format!("cannot compute store hash for `{}`", step.name))?;
            let metadata =
                build::read_rune_metadata(rune, build::tome_name_for_rune(rune)?.as_deref())?;
            let build_deps = metadata.deps.build_for(&paths::target_triple());
            ensure_build_deps_installed_inner(&build_deps, building)
                .with_context(|| format!("install build dependencies for `{}`", step.name))?;
            let env = build::build_env_for_target(
                build_dep_bin_dirs(&build_deps)?,
                build_dep_env_vars(&build_deps)?,
                &paths::target_triple(),
            )?;
            let result = build::build_package_with_env(
                &rune.to_string_lossy(),
                &paths::build_output_dir()?,
                &env,
                &store_hash,
            )?;
            install_store_only(&result.archive, None, Some(&result.store_hash))
        } else {
            bail!(
                "no installable prebuilt or source for `{}` {}",
                step.name,
                step.version
            )
        };

        let installed_archive = result
            .with_context(|| format!("store-only install `{}` {}", step.name, step.version))?;
        installed.insert(installed_archive.name, installed_archive.version);
        building.remove(&step.name);
    }

    Ok(())
}

/// Finds an installed package that satisfies the dependency `name`.
/// First tries an exact package name match, then falls back to capability resolution
/// (any installed package whose `bins` map contains `name` as a key).
pub(crate) fn find_dep_state<'a>(
    states: &'a [PackageState],
    name: &str,
) -> Option<&'a PackageState> {
    states
        .iter()
        .find(|state| state.name == name)
        .or_else(|| states.iter().find(|state| state.bins.contains_key(name)))
        .or_else(|| {
            states
                .iter()
                .find(|state| state.provides.iter().any(|p| p == name))
        })
}

pub(crate) fn push_bin_dirs(dirs: &mut Vec<PathBuf>, state: &PackageState) {
    for path in state.bins.values() {
        let bin = PathBuf::from(&state.store_path).join(path);
        let Some(parent) = bin.parent() else {
            continue;
        };
        let dir = parent.to_path_buf();
        if !dirs.contains(&dir) {
            dirs.push(dir);
        }
    }
}

pub(crate) fn build_dep_bin_dirs(deps: &[Dependency]) -> Result<Vec<PathBuf>> {
    let states = installed_states()?;
    let mut dirs = Vec::new();
    for dep in deps {
        let Some(state) = find_dep_state(&states, &dep.name) else {
            continue;
        };
        push_bin_dirs(&mut dirs, state);
    }
    Ok(dirs)
}

/// Computes additional environment variables from installed build dependencies so build systems
/// can discover only declared dependency prefixes. The runtime sandbox clears host discovery
/// variables first; these values are the managed search roots layered back in.
pub(crate) fn build_dep_env_vars(deps: &[Dependency]) -> Result<Vec<(String, String)>> {
    let states = installed_states()?;
    let mut prefixes = Vec::new();
    let mut pkg_config_paths = Vec::new();
    let mut cpaths = Vec::new();
    let mut library_paths = Vec::new();
    let mut aclocal_paths = Vec::new();
    let mut prefix_vars = Vec::new();

    for dep in deps {
        let Some(state) = find_dep_state(&states, &dep.name) else {
            continue;
        };
        let store = PathBuf::from(&state.store_path);
        if !prefixes.contains(&store) {
            prefixes.push(store.clone());
        }
        let pkgconfig = store.join("lib/pkgconfig");
        if pkgconfig.is_dir() && !pkg_config_paths.contains(&pkgconfig) {
            pkg_config_paths.push(pkgconfig);
        }
        let share_pkgconfig = store.join("share/pkgconfig");
        if share_pkgconfig.is_dir() && !pkg_config_paths.contains(&share_pkgconfig) {
            pkg_config_paths.push(share_pkgconfig);
        }
        let include = store.join("include");
        if include.is_dir() && !cpaths.contains(&include) {
            cpaths.push(include);
        }
        let lib = store.join("lib");
        if lib.is_dir() && !library_paths.contains(&lib) {
            library_paths.push(lib);
        }
        let aclocal = store.join("share/aclocal");
        if aclocal.is_dir() && !aclocal_paths.contains(&aclocal) {
            aclocal_paths.push(aclocal);
        }
        let env_name = format!("{}_PREFIX", dep.name.to_ascii_uppercase().replace('-', "_"));
        prefix_vars.push((env_name, state.store_path.clone()));
    }

    let mut env = Vec::new();
    if !prefixes.is_empty() {
        env.push(("CMAKE_PREFIX_PATH".to_string(), join_paths_lossy(&prefixes)));
    }
    if !pkg_config_paths.is_empty() {
        let pkg_config_path = join_paths_lossy(&pkg_config_paths);
        env.push(("PKG_CONFIG_PATH".to_string(), pkg_config_path.clone()));
        env.push(("PKG_CONFIG_LIBDIR".to_string(), pkg_config_path));
    }
    if !cpaths.is_empty() {
        env.push(("CPATH".to_string(), join_paths_lossy(&cpaths)));
    }
    if !library_paths.is_empty() {
        let library_path = join_paths_lossy(&library_paths);
        env.push(("LIBRARY_PATH".to_string(), library_path.clone()));
        env.push(("CMAKE_LIBRARY_PATH".to_string(), library_path));
    }
    if !aclocal_paths.is_empty() {
        env.push(("ACLOCAL_PATH".to_string(), join_paths_lossy(&aclocal_paths)));
    }
    env.extend(prefix_vars);
    Ok(env)
}

pub(crate) fn join_paths_lossy(paths: &[PathBuf]) -> String {
    std::env::join_paths(paths)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default()
}
