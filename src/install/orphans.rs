//! Removal and orphan cleanup: explicit removes, the autoremove cascade, and the
//! read-only orphan listing.

use anyhow::{Context, Result, bail};
use std::{
    collections::{HashSet, VecDeque},
    fs,
    path::PathBuf,
};

use crate::{
    cli::PackageArg, model::PackageState, nu::nuon_io, profile, util::paths, util::progress::report,
};

use super::*;

pub fn remove(args: PackageArg) -> Result<()> {
    if let Some(msg) = paths::fixed_store_setup_instructions() {
        bail!("{msg}");
    }
    if args.packages.is_empty() {
        bail!("specify at least one package to remove");
    }

    // Refuse to break installed packages: a removal target that something outside the
    // removal set still requires (by name, bin, or capability) is an error naming the
    // dependents. The whole named set leaves together, so removing a package alongside
    // its last dependent is fine.
    let states = installed_states()?;
    let removal_set: HashSet<&str> = args.packages.iter().map(String::as_str).collect();
    for package in &args.packages {
        let Some(target) = states.iter().find(|state| state.name == *package) else {
            continue; // remove_one reports "not installed" with the right message
        };
        let dependents: Vec<&str> = states
            .iter()
            .filter(|other| other.name != target.name && !removal_set.contains(other.name.as_str()))
            .filter(|other| {
                other.runtime_deps.iter().any(|dep| {
                    dep == &target.name
                        || target.bins.contains_key(dep)
                        || target.provides.iter().any(|p| p == dep)
                })
            })
            .map(|state| state.name.as_str())
            .collect();
        if !dependents.is_empty() {
            bail!(
                "cannot remove `{package}`: still required by {}. Remove the dependents \
                 first (or in the same command) to proceed",
                dependents.join(", ")
            );
        }
    }

    let mut all_runtime_deps = Vec::new();
    for package in &args.packages {
        let removed = remove_one(package)?;
        report(&format!("removed {package}"));
        all_runtime_deps.extend(removed.runtime_deps);
    }
    autoremove_orphans(all_runtime_deps)?;
    let states = installed_states()?;
    profile::rebuild_and_activate(&states)?;
    Ok(())
}

/// Removes one installed package and returns its prior state record. Each call is a complete
/// transaction (package directory, state file, lockfile, profile generation) — callers chaining multiple
/// removes do not need to coordinate rollback across them.
pub(crate) fn remove_one(name: &str) -> Result<PackageState> {
    let root = paths::install_root()?;
    let state_path = root
        .join("state")
        .join("packages")
        .join(format!("{name}.nuon"));

    if !state_path.exists() {
        bail!("package `{name}` is not installed");
    }

    let state = PackageState::from_value(nuon_io::read_nuon(&state_path)?)?;

    // Removal mutates the same shared install state as an install, so stage every step against
    // a transaction: a failure partway through restores the package files and state record
    // rather than leaving the package half-removed.
    let mut tx = Transaction::new();

    // Move the package dir aside rather than deleting outright, so a later failure can restore
    // it; the backup is dropped only once the whole removal commits.
    let store_root = paths::store_root()?;
    let package_dir = PathBuf::from(&state.store_path);
    if !package_dir.starts_with(&store_root) {
        bail!(
            "package `{}` store path `{}` is outside the store root `{}`; refusing to remove",
            state.name,
            package_dir.display(),
            store_root.display()
        );
    }
    let backup = backup_path(&package_dir)?;
    let had_package = package_dir.exists();
    if had_package {
        if backup.exists() {
            fs::remove_dir_all(&backup)?;
        }
        fs::rename(&package_dir, &backup)
            .with_context(|| format!("move aside package {}", package_dir.display()))?;
        let package_dir = package_dir.clone();
        let backup = backup.clone();
        tx.on_rollback(move || {
            let _ = fs::rename(&backup, &package_dir);
        });
    }

    let state_bytes = fs::read(&state_path)?;
    {
        let state_path = state_path.clone();
        tx.on_rollback(move || {
            let _ = fs::write(&state_path, &state_bytes);
        });
    }
    fs::remove_file(&state_path)?;

    rebuild_lock(&mut tx)?;

    tx.commit();
    if had_package {
        let _ = fs::remove_dir_all(&backup);
    }
    Ok(state)
}

/// Whether any *other* package in `others` lists `state` in its `runtime_deps` — by package
/// name, by one of its bins, or by a capability it provides.
pub(crate) fn referenced_by_other<'a>(
    others: impl IntoIterator<Item = &'a PackageState>,
    state: &PackageState,
) -> bool {
    others.into_iter().any(|other| {
        if other.name == state.name {
            return false;
        }
        other.runtime_deps.iter().any(|dep| {
            dep == &state.name
                || state.bins.contains_key(dep)
                || state.provides.iter().any(|p| p == dep)
        })
    })
}

/// The packages `grm autoremove` would delete: dependency-installed (`!requested`), not held,
/// and unreferenced by any remaining package. Iterates to a fixed point so chains collapse —
/// removing an orphan can orphan its own dependencies in turn. Read-only; serves `grm orphans`.
pub(crate) fn orphan_candidates(states: &[PackageState]) -> Vec<String> {
    let mut remaining: Vec<&PackageState> = states.iter().collect();
    let mut orphans = Vec::new();
    loop {
        let (gone, kept): (Vec<&PackageState>, Vec<&PackageState>) =
            remaining.iter().copied().partition(|state| {
                !state.requested
                    && !state.held
                    && !referenced_by_other(remaining.iter().copied(), state)
            });
        if gone.is_empty() {
            break;
        }
        orphans.extend(gone.iter().map(|state| state.name.clone()));
        remaining = kept;
    }
    orphans.sort();
    orphans
}

/// Removes runtime dependencies left orphaned by a previous removal or upgrade — packages no
/// other installed package still lists in its `runtime_deps`. Cascades transitively: a dep
/// that becomes orphaned mid-pass is itself a candidate. Explicitly requested and held
/// packages are never autoremoved. Build dependencies are not considered; once a package is
/// installed they are no longer load-bearing for it. Returns the names removed.
pub(crate) fn autoremove_orphans(initial: Vec<String>) -> Result<Vec<String>> {
    let mut queue: VecDeque<String> = initial.into();
    let mut seen: HashSet<String> = HashSet::new();
    let mut removed_names = Vec::new();
    let mut states = installed_states()?;
    while let Some(name) = queue.pop_front() {
        if !seen.insert(name.clone()) {
            continue;
        }
        let Some(name_state) = states.iter().find(|s| s.name == name) else {
            continue;
        };
        if name_state.requested || name_state.held {
            continue;
        }
        if referenced_by_other(states.iter(), name_state) {
            continue;
        }
        let removed =
            remove_one(&name).with_context(|| format!("autoremove unused dependency `{name}`"))?;
        report(&format!("autoremoved unused dependency {name}"));
        removed_names.push(name);
        for dep in removed.runtime_deps {
            queue.push_back(dep);
        }
        states = installed_states()?;
    }
    Ok(removed_names)
}

/// Lists the packages `grm autoremove` would remove, without removing anything.
pub fn orphans() -> Result<()> {
    let states = installed_states()?;
    let candidates = orphan_candidates(&states);
    if candidates.is_empty() {
        report("no orphaned packages");
        return Ok(());
    }
    for name in candidates {
        let version = states
            .iter()
            .find(|s| s.name == name)
            .map(|s| s.version.as_str())
            .unwrap_or("");
        println!("{name}\t{version}");
    }
    Ok(())
}

/// Removes every orphaned dependency package: installed as a dependency (not requested, not
/// held) and referenced by no remaining package. Rebuilds the active generation when anything
/// was removed.
pub fn autoremove() -> Result<()> {
    if let Some(msg) = paths::fixed_store_setup_instructions() {
        bail!("{msg}");
    }
    let states = installed_states()?;
    let seeds: Vec<String> = states
        .iter()
        .filter(|state| !state.requested && !state.held)
        .map(|state| state.name.clone())
        .collect();
    let removed = autoremove_orphans(seeds)?;
    if removed.is_empty() {
        report("no orphaned packages");
        return Ok(());
    }
    let states = installed_states()?;
    profile::rebuild_and_activate(&states)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn state(name: &str, runtime_deps: &[&str], requested: bool, held: bool) -> PackageState {
        PackageState {
            name: name.to_owned(),
            version: "1.0.0".to_owned(),
            target: None,
            archive_hash: "0".repeat(64),
            store_hash: "deadbeef".to_owned(),
            store_path: format!("/grm/store/deadbeef-{name}-1.0.0"),
            bins: BTreeMap::new(),
            runtime_deps: runtime_deps.iter().map(|s| s.to_string()).collect(),
            build_deps: Vec::new(),
            source_hashes: BTreeMap::new(),
            held,
            requested,
            provides: Vec::new(),
            libs: Vec::new(),
            notes: Vec::new(),
        }
    }

    #[test]
    fn orphan_candidates_keeps_referenced_deps() {
        let states = vec![
            state("app", &["lib"], true, false),
            state("lib", &[], false, false),
        ];
        assert!(orphan_candidates(&states).is_empty());
    }

    #[test]
    fn orphan_candidates_finds_unreferenced_dep() {
        let states = vec![
            state("app", &[], true, false),
            state("lib", &[], false, false),
        ];
        assert_eq!(orphan_candidates(&states), vec!["lib".to_owned()]);
    }

    #[test]
    fn orphan_candidates_cascades_chains() {
        // app no longer depends on lib-a; lib-a -> lib-b becomes a dead chain.
        let states = vec![
            state("app", &[], true, false),
            state("lib-a", &["lib-b"], false, false),
            state("lib-b", &[], false, false),
        ];
        assert_eq!(
            orphan_candidates(&states),
            vec!["lib-a".to_owned(), "lib-b".to_owned()]
        );
    }

    #[test]
    fn orphan_candidates_spares_requested_and_held() {
        let states = vec![
            state("explicit", &[], true, false),
            state("pinned", &[], false, true),
            state("dep-of-pinned", &[], false, false),
        ];
        // `pinned` is held so it survives, and it references nothing, so `dep-of-pinned`
        // (referenced by nobody) is the only orphan.
        assert_eq!(orphan_candidates(&states), vec!["dep-of-pinned".to_owned()]);
    }

    #[test]
    fn orphan_candidates_resolves_capability_references() {
        let mut provider = state("gawk", &[], false, false);
        provider.provides = vec!["awk".to_owned()];
        let states = vec![state("app", &["awk"], true, false), provider];
        assert!(orphan_candidates(&states).is_empty());
    }
}
