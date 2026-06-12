//! Removal and the orphan sweep that runs inside every removing transaction.
//!
//! Removal is store-preserving: it drops state records, the lockfile entry, and the package's
//! presence in the next generation, but leaves the store directory in place — older
//! generations hard-link from it and `grm clean` reclaims it once nothing references it.

use anyhow::{Context, Result, bail};
use std::{
    collections::{HashSet, VecDeque},
    fs,
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

    // Never break installed packages: a removal target that something outside the removal set
    // still requires (by name, bin, or capability) is kept and demoted to dependency status
    // instead, so the sweep takes it out the moment nothing needs it. Only *linked* dependents
    // count — a store-only package (cached build dep, residue) is not part of the environment
    // and cannot pin a package into it; its dangling dep is re-resolved if it is ever needed
    // again. The whole named set leaves together, so removing a package alongside its last
    // dependent removes both.
    let states = installed_states()?;
    let linked = linked_set(&states);
    let removal_set: HashSet<&str> = args.packages.iter().map(String::as_str).collect();
    let mut to_remove = Vec::new();
    let mut demoted = 0usize;
    for package in &args.packages {
        let Some(target) = states.iter().find(|state| state.name == *package) else {
            bail!("package `{package}` is not installed");
        };
        let dependents = dependents_outside(&states, target, &removal_set, &linked);
        if dependents.is_empty() {
            to_remove.push(target.name.clone());
        } else {
            set_requested(&target.name, false, false)?;
            report(&format!(
                "kept {package} — still required by {}; now a dependency, removed once nothing needs it",
                dependents.join(", ")
            ));
            demoted += 1;
        }
    }

    let mut all_runtime_deps = Vec::new();
    for package in &to_remove {
        let removed = remove_one(package)?;
        report(&format!("removed {package}"));
        all_runtime_deps.extend(removed.runtime_deps);
    }
    let swept = sweep_orphans(all_runtime_deps)?;
    if !to_remove.is_empty() || !swept.is_empty() {
        let states = installed_states()?;
        profile::rebuild_and_activate(&states)?;
    } else if demoted > 0 {
        // Demotion only flips intent flags; the generation's contents are unchanged.
        report("nothing removed");
    }
    Ok(())
}

/// The *linked* installed packages outside `removal_set` that require `target` in their
/// `runtime_deps` — by its package name, one of its bins, or a capability it provides.
/// Store-only packages are ignored: they are cache, not environment.
fn dependents_outside<'a>(
    states: &'a [PackageState],
    target: &PackageState,
    removal_set: &HashSet<&str>,
    linked: &HashSet<String>,
) -> Vec<&'a str> {
    states
        .iter()
        .filter(|other| {
            other.name != target.name
                && !removal_set.contains(other.name.as_str())
                && linked.contains(&other.name)
        })
        .filter(|other| {
            other.runtime_deps.iter().any(|dep| {
                dep == &target.name
                    || target.bins.contains_key(dep)
                    || target.provides.iter().any(|p| p == dep)
            })
        })
        .map(|state| state.name.as_str())
        .collect()
}

/// Removes one installed package's state record and returns it. Each call is a complete
/// transaction (state file plus lockfile) — callers chaining multiple removes do not need to
/// coordinate rollback across them. The store directory is deliberately left in place: it is
/// content-addressed and unreferenced state-less directories are never re-trusted, so it acts
/// as a cache until `grm clean` collects it.
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
    // a transaction: a failure partway through restores the state record rather than leaving
    // the package half-removed.
    let mut tx = Transaction::new();

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

/// Removes runtime dependencies left orphaned by a removal or upgrade — packages no other
/// installed package still lists in its `runtime_deps`. Cascades transitively: a dep that
/// becomes orphaned mid-pass is itself a candidate. Explicitly requested and held packages are
/// never swept. Build dependencies are not considered; once a package is installed they are no
/// longer load-bearing for it. Returns the names removed.
pub(crate) fn sweep_orphans(initial: Vec<String>) -> Result<Vec<String>> {
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
            remove_one(&name).with_context(|| format!("remove unused dependency `{name}`"))?;
        report(&format!("removed unused dependency {name}"));
        removed_names.push(name);
        for dep in removed.runtime_deps {
            queue.push_back(dep);
        }
        states = installed_states()?;
    }
    Ok(removed_names)
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
    fn referenced_dep_is_not_orphaned() {
        let states = [
            state("app", &["lib"], true, false),
            state("lib", &[], false, false),
        ];
        assert!(referenced_by_other(states.iter(), &states[1]));
    }

    #[test]
    fn unreferenced_dep_is_orphaned() {
        let states = [
            state("app", &[], true, false),
            state("lib", &[], false, false),
        ];
        assert!(!referenced_by_other(states.iter(), &states[1]));
    }

    #[test]
    fn capability_reference_counts_as_a_dependent() {
        let mut provider = state("gawk", &[], false, false);
        provider.provides = vec!["awk".to_owned()];
        let states = [state("app", &["awk"], true, false), provider.clone()];
        assert!(referenced_by_other(states.iter(), &provider));
    }

    #[test]
    fn dependents_outside_ignores_the_removal_set() {
        let states = vec![
            state("app", &["lib"], true, false),
            state("other", &["lib"], true, false),
            state("lib", &[], false, false),
        ];
        let linked = linked_set(&states);
        let everyone: HashSet<&str> = HashSet::new();
        assert_eq!(
            dependents_outside(&states, &states[2], &everyone, &linked),
            vec!["app", "other"]
        );
        let with_app: HashSet<&str> = ["app"].into();
        assert_eq!(
            dependents_outside(&states, &states[2], &with_app, &linked),
            vec!["other"]
        );
        let both: HashSet<&str> = ["app", "other"].into();
        assert!(dependents_outside(&states, &states[2], &both, &linked).is_empty());
    }

    #[test]
    fn store_only_dependents_cannot_pin_a_package() {
        // `residue` (not requested, referenced by nothing linked) runtime-depends on `tool`,
        // but residue is cache, not environment: removing `tool` must not be blocked by it.
        let states = vec![
            state("tool", &[], true, false),
            state("residue", &["tool"], false, false),
        ];
        let linked = linked_set(&states);
        assert!(!linked.contains("residue"), "residue must be store-only");
        let nobody: HashSet<&str> = HashSet::new();
        assert!(
            dependents_outside(&states, &states[0], &nobody, &linked).is_empty(),
            "a store-only referencer must not count as a dependent"
        );
    }
}
