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

    // A build dep is *missing* when nothing installed satisfies it, and *stale* when the
    // satisfying install no longer matches its rune — same version, different store hash
    // (e.g. the rune was edited after the dep was first realized). Stale deps re-resolve so
    // the rune stays the truth: the solver sees them as absent and realization replaces
    // them at their new address.
    let mut missing: Vec<Dependency> = Vec::new();
    let mut satisfied: Vec<PackageState> = Vec::new();
    for dep in deps {
        match find_dep_state(&states, &dep.name) {
            None => missing.push(dep.clone()),
            Some(state) => satisfied.push(state.clone()),
        }
    }
    let stale: HashSet<String> = crate::store::closure::stale_installed(&satisfied)
        .into_iter()
        .collect();
    for dep in deps {
        if let Some(state) = find_dep_state(&states, &dep.name)
            && stale.contains(&state.name)
        {
            crate::util::progress::warn(&format!(
                "{} {} no longer matches its rune; reinstalling",
                state.name, state.version
            ));
            missing.push(dep.clone());
        }
    }
    for name in &stale {
        installed.remove(name);
    }

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
        // The recursion below installs the build deps of anything built from source, so a
        // later step in this plan may have landed since the plan was resolved; reuse it
        // instead of realizing it twice (same staleness as `Installer::reuse_realized_step`).
        if step_already_realized(&step)? {
            installed.insert(step.name.clone(), step.version.clone());
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
                InstallOrigin::BuildDep,
            )
        } else if let Some(rune) = &step.rune {
            let store_hash = crate::store::closure::store_hash_for_rune(rune)
                .with_context(|| format!("cannot compute store hash for `{}`", step.name))?;
            let metadata =
                build::read_rune_metadata(rune, build::tome_name_for_rune(rune)?.as_deref())?;
            // Reuse a verified cached build of these exact inputs instead of rebuilding.
            if let Some(archive) = cached_build_archive(&metadata, &store_hash) {
                let installed_archive =
                    install_store_only(&archive, None, Some(&store_hash), InstallOrigin::BuildDep)
                        .with_context(|| {
                            format!("store-only install `{}` {}", step.name, step.version)
                        })?;
                installed.insert(installed_archive.name, installed_archive.version);
                building.remove(&step.name);
                continue;
            }
            let build_deps = build::effective_build_deps(rune, &metadata, &paths::target_triple())?;
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
            install_store_only(
                &result.primary.archive,
                None,
                Some(&result.primary.store_hash),
                InstallOrigin::BuildDep,
            )
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
/// First tries an exact package name match, then falls back to capability resolution: an
/// installed package whose `bins` map contains `name` as a key, or that lists it in
/// `provides`. With several installed providers the `grm prefer` choice wins, else the first
/// by name (states are sorted) — the same order the solver and the closure walker use, so
/// the linked set and `<DEP>_PREFIX` env vars agree with resolution.
pub(crate) fn find_dep_state<'a>(
    states: &'a [PackageState],
    name: &str,
) -> Option<&'a PackageState> {
    if let Some(state) = states.iter().find(|state| state.name == name) {
        return Some(state);
    }
    let providers: Vec<&PackageState> = states
        .iter()
        .filter(|state| state.bins.contains_key(name) || state.provides.iter().any(|p| p == name))
        .collect();
    match providers.len() {
        0 => None,
        1 => Some(providers[0]),
        _ => {
            let preferences = crate::model::preferences::Preferences::load().unwrap_or_default();
            preferences
                .providers
                .get(name)
                .and_then(|preferred| {
                    providers
                        .iter()
                        .find(|state| &state.name == preferred)
                        .copied()
                })
                .or(Some(providers[0]))
        }
    }
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
