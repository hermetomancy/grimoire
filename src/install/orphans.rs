//! Removal and the orphan sweep that runs inside every removing transaction.
//!
//! Removal is store-preserving: it drops state records, the lockfile entry, and the package's
//! presence in the next generation, but leaves the store directory in place — older
//! generations symlink into it and `grm clean` reclaims it once nothing references it.

use anyhow::{Context, Result, bail};
use std::collections::{HashSet, VecDeque};

use crate::{
    model::PackageState,
    util::paths,
    util::progress::{accent, note, report, strong, warn},
};

use super::world::InstalledWorld;
use super::*;

pub fn remove(args: crate::cli::MutatePackagesArgs) -> Result<()> {
    if let Some(msg) = paths::fixed_store_setup_instructions() {
        bail!("{msg}");
    }
    if args.packages.is_empty() {
        bail!("specify at least one package to remove");
    }
    if args.dry_run {
        return dry_run_remove(&args.packages);
    }

    // Never break installed packages: a removal target that something outside the removal set
    // still requires (by name, bin, or capability) is kept and demoted to dependency status
    // instead, so the sweep takes it out the moment nothing needs it. Only *linked* dependents
    // count — a store-only package (cached build dep, residue) is not part of the environment
    // and cannot pin a package into it; its dangling dep is re-resolved if it is ever needed
    // again. The whole named set leaves together, so removing a package alongside its last
    // dependent removes both.
    let mut world = InstalledWorld::load_default()?;
    let linked = world.linked_immut();
    let removal_set: HashSet<&str> = args.packages.iter().map(String::as_str).collect();
    let mut to_remove = Vec::new();
    let mut demoted = 0usize;
    let states = world.to_states();
    for package in &args.packages {
        let Some(target) = world.get(package).cloned() else {
            bail!("package `{package}` is not installed");
        };
        let dependents = dependents_outside(&states, &target, &removal_set, &linked);
        if dependents.is_empty() {
            to_remove.push(target.name.clone());
        } else {
            set_requested(&mut world, &target.name, false, false)?;
            warn(&format!(
                "kept {package} — still required by {}; now a dependency, removed once nothing needs it",
                dependents.join(", ")
            ));
            demoted += 1;
        }
    }

    let mut all_runtime_deps = Vec::new();
    for package in &to_remove {
        let removed = remove_one(&mut world, package)?;
        report(&format!("removed {}", accent(package)));
        all_runtime_deps.extend(removed.runtime_deps);
    }
    let swept = sweep_orphans(&mut world, all_runtime_deps)?;
    if !to_remove.is_empty() || !swept.is_empty() || demoted > 0 {
        let mut tx = Transaction::new();
        world.commit(&mut tx)?;
        finalize_state(&mut tx, &world)?;
        tx.commit();
    }
    if to_remove.is_empty() && demoted > 0 {
        report("nothing removed");
    }
    Ok(())
}

/// `remove --dry-run`: the same target/demotion decisions a real removal makes, plus a pure
/// simulation of the orphan cascade, printed as a plan with nothing touched.
fn dry_run_remove(packages: &[String]) -> Result<()> {
    let world = InstalledWorld::load_default()?;
    let states = world.to_states();
    let linked = world.linked_immut();
    let removal_set: HashSet<&str> = packages.iter().map(String::as_str).collect();
    let mut to_remove = Vec::new();
    println!("plan:");
    for package in packages {
        let Some(target) = states.iter().find(|state| state.name == *package) else {
            bail!("package `{package}` is not installed");
        };
        let dependents = dependents_outside(&states, target, &removal_set, &linked);
        if dependents.is_empty() {
            println!("  - {} {}", target.name, target.version);
            to_remove.push(target.name.clone());
        } else {
            println!(
                "  ~ {} kept — still required by {}; demoted to dependency",
                target.name,
                dependents.join(", ")
            );
        }
    }
    for orphan in simulate_orphan_sweep(&states, &to_remove, &[]) {
        println!("  - {orphan} (unused dependency)");
    }
    Ok(())
}

/// Pure simulation of [`sweep_orphans`] after removing `removed`: which non-requested,
/// non-held packages would cascade out, computed on an in-memory copy of state.
/// `extra_seeds` adds candidates beyond the removed set's dependencies — `clean --dry-run`
/// seeds with every non-requested package, mirroring the real sweep.
pub(crate) fn simulate_orphan_sweep(
    states: &[PackageState],
    removed: &[String],
    extra_seeds: &[String],
) -> Vec<String> {
    let mut remaining: Vec<PackageState> = states
        .iter()
        .filter(|state| !removed.contains(&state.name))
        .cloned()
        .collect();
    let mut queue: VecDeque<String> = states
        .iter()
        .filter(|state| removed.contains(&state.name))
        .flat_map(|state| state.runtime_deps.iter().cloned())
        .chain(extra_seeds.iter().cloned())
        .collect();
    let mut seen: HashSet<String> = HashSet::new();
    let mut swept = Vec::new();
    while let Some(name) = queue.pop_front() {
        if !seen.insert(name.clone()) {
            continue;
        }
        let Some(index) = remaining.iter().position(|s| s.name == name) else {
            continue;
        };
        if remaining[index].requested || remaining[index].held {
            continue;
        }
        if referenced_by_other(remaining.iter(), &remaining[index]) {
            continue;
        }
        let state = remaining.swap_remove(index);
        for dep in &state.runtime_deps {
            queue.push_back(dep.clone());
        }
        swept.push(state.name);
    }
    swept
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
/// transaction for the state file — callers chaining multiple removes do not need to coordinate
/// rollback across them. The lockfile is rebuilt once at the command boundary. The store directory
/// is deliberately left in place: it is content-addressed and unreferenced state-less directories
/// are never re-trusted, so it acts as a cache until `grm clean` collects it.
/// Removes one installed package's state record from the in-memory world. The caller is
/// responsible for committing the world; this only mutates the authoritative in-memory state.
pub(crate) fn remove_one(world: &mut InstalledWorld, name: &str) -> Result<PackageState> {
    world
        .remove(name)
        .with_context(|| format!("package `{name}` is not installed"))
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
/// Removes runtime dependencies left orphaned by a removal or upgrade — packages no other
/// installed package still lists in its `runtime_deps`. Cascades transitively: a dep that
/// becomes orphaned mid-pass is itself a candidate. Explicitly requested and held packages are
/// never swept. Build dependencies are not considered; once a package is installed they are no
/// longer load-bearing for it. Returns the names removed.
pub(crate) fn sweep_orphans(
    world: &mut InstalledWorld,
    initial: Vec<String>,
) -> Result<Vec<String>> {
    let mut queue: VecDeque<String> = initial.into();
    let mut seen: HashSet<String> = HashSet::new();
    let mut removed_names = Vec::new();
    while let Some(name) = queue.pop_front() {
        if !seen.insert(name.clone()) {
            continue;
        }
        let Some(state) = world.get(&name).cloned() else {
            continue;
        };
        if state.requested || state.held {
            continue;
        }
        if referenced_by_other(world.iter(), &state) {
            continue;
        }
        world.remove(&name);
        note(&format!("removed unused dependency {}", strong(&name)));
        removed_names.push(name);
        for dep in state.runtime_deps {
            queue.push_back(dep);
        }
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
            upstream_version: None,
            conflicts: Vec::new(),
            replaces: Vec::new(),
            build_env: None,
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

    fn linked_names(states: &[PackageState]) -> HashSet<String> {
        let mut linked: HashSet<String> = HashSet::new();
        let mut queue: std::collections::VecDeque<&PackageState> = states
            .iter()
            .filter(|state| state.requested || state.held)
            .collect();
        while let Some(state) = queue.pop_front() {
            if !linked.insert(state.name.clone()) {
                continue;
            }
            for dep in &state.runtime_deps {
                if let Some(dep_state) = states.iter().find(|s| {
                    s.name == *dep
                        || s.bins.contains_key(dep)
                        || s.provides.iter().any(|p| p == dep)
                }) && !linked.contains(&dep_state.name)
                {
                    queue.push_back(dep_state);
                }
            }
        }
        linked
    }

    #[test]
    fn dependents_outside_ignores_the_removal_set() {
        let states = vec![
            state("app", &["lib"], true, false),
            state("other", &["lib"], true, false),
            state("lib", &[], false, false),
        ];
        let linked = linked_names(&states);
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
        let linked = linked_names(&states);
        assert!(!linked.contains("residue"), "residue must be store-only");
        let nobody: HashSet<&str> = HashSet::new();
        assert!(
            dependents_outside(&states, &states[0], &nobody, &linked).is_empty(),
            "a store-only referencer must not count as a dependent"
        );
    }
}
