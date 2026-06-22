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

use super::world::InstalledWorld;
use super::*;

/// Ensures every build dependency in `deps` is installed store-only (no lockfile, no generation).
/// Missing deps are resolved through the solver and installed from substitutes or built from source.
/// Already-installed packages are reused.
pub(crate) fn ensure_build_deps_installed(deps: &[Dependency]) -> Result<()> {
    let mut world = InstalledWorld::load_default()?;
    let mut building = HashSet::new();
    ensure_build_deps_installed_inner(&mut world, deps, &mut building)
}

pub(crate) fn ensure_build_deps_installed_inner(
    world: &mut InstalledWorld,
    deps: &[Dependency],
    building: &mut HashSet<String>,
) -> Result<()> {
    if deps.is_empty() {
        return Ok(());
    }

    let mut installed = world.installed_versions();

    // A build dep is *missing* when nothing installed satisfies it, and *stale* when the
    // satisfying install no longer matches its rune — same version, different store hash
    // (e.g. the rune was edited after the dep was first realized). Stale deps re-resolve so
    // the rune stays the truth: the solver sees them as absent and realization replaces
    // them at their new address.
    let mut missing: Vec<Dependency> = Vec::new();
    let mut satisfied: Vec<PackageState> = Vec::new();
    for dep in deps {
        match world.resolve_dep(&dep.name) {
            None => missing.push(dep.clone()),
            Some(state) => satisfied.push(state.clone()),
        }
    }
    let stale_info = crate::store::closure::stale_installed(world);
    let stale: HashSet<String> = stale_info.iter().map(|s| s.name.clone()).collect();
    for dep in deps {
        if let Some(state) = world.resolve_dep(&dep.name)
            && stale.contains(&state.name)
        {
            let cause = stale_info
                .iter()
                .find(|s| s.name == state.name)
                .and_then(|s| s.env_change.clone())
                .map(|diff| format!("build environment changed: {diff}"))
                .unwrap_or_else(|| {
                    "its rune, a dependency, or the build environment changed".to_owned()
                });
            crate::util::output::warn(&format!(
                "{} {} drifted from its expected address ({cause}); rebuilding",
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

    let mut plan = solve::resolve(&missing, &installed, &HashSet::new(), None)?;
    plan.compute_store_hashes()
        .with_context(|| "compute store hashes for build dependencies")?;

    for step in plan.steps {
        if installed.contains_key(&step.name) {
            continue;
        }
        // The recursion below installs the build deps of anything built from source, so a
        // later step in this plan may have landed since the plan was resolved; reuse it
        // instead of realizing it twice (same staleness as `Installer::reuse_realized_step`).
        if step_already_realized(world, &step)? {
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
                world,
                &archive,
                Some(sub.entry.archive_hash.clone()),
                Some(&sub.store_hash),
                InstallOrigin::BuildDep,
            )
        } else if let Some(rune) = &step.rune {
            let store_hash =
                crate::store::closure::store_hash_for_rune(rune, &world.store_hashes())
                    .with_context(|| format!("cannot compute store hash for `{}`", step.name))?;
            let metadata =
                build::read_rune_metadata(rune, build::tome_name_for_rune(rune)?.as_deref())?;
            // Reuse a verified cached build of these exact inputs instead of rebuilding.
            if let Some(archive) = cached_build_archive(&metadata, &store_hash) {
                let installed_archive = install_store_only(
                    world,
                    &archive,
                    None,
                    Some(&store_hash),
                    InstallOrigin::BuildDep,
                )
                .with_context(|| format!("store-only install `{}` {}", step.name, step.version))?;
                installed.insert(installed_archive.name, installed_archive.version);
                building.remove(&step.name);
                continue;
            }
            let build_deps = build::effective_build_deps(rune, &metadata, &paths::target_triple())?;
            ensure_build_deps_installed_inner(world, &build_deps, building)
                .with_context(|| format!("install build dependencies for `{}`", step.name))?;
            let env = build::build_env_for_target(
                build_dep_bin_dirs(&build_deps)?,
                build_dep_env_vars(&build_deps)?,
                &paths::target_triple(),
                &metadata.name,
            )?;
            let result = build::build_package_with_env(
                &rune.to_string_lossy(),
                &paths::build_output_dir()?,
                &env,
                &store_hash,
                &world.store_hashes(),
            )?;
            install_store_only(
                world,
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
    let world = InstalledWorld::load_default()?;
    let mut dirs = Vec::new();
    for dep in deps {
        let Some(state) = world.resolve_dep(&dep.name) else {
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
    let world = InstalledWorld::load_default()?;
    let mut prefixes = Vec::new();
    let mut pkg_config_paths = Vec::new();
    let mut cpaths = Vec::new();
    let mut library_paths = Vec::new();
    let mut aclocal_paths = Vec::new();
    let mut prefix_vars = Vec::new();

    for dep in deps {
        let Some(state) = world.resolve_dep(&dep.name) else {
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
