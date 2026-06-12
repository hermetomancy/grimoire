//! Reading and flagging installed-package state: listing, hold/unhold, requested marking.

use anyhow::{Result, bail};
use semver::Version;
use std::{
    collections::{BTreeMap, HashSet, VecDeque},
    fs,
};

use crate::{
    cli::PackageArg,
    model::{PackageState, parse_version_relaxed},
    nu::nuon_io,
    util::paths,
    util::progress::{report, status},
};

use super::*;

pub fn list() -> Result<()> {
    let states = installed_states()?;
    if states.is_empty() {
        println!("No packages are currently installed.");
        return Ok(());
    }
    let linked = linked_set(&states);
    for state in states {
        let flag = if state.held {
            "held"
        } else if !linked.contains(&state.name) {
            // Present in the store for reuse (build dep, residue) but not part of the
            // user's environment.
            "store-only"
        } else {
            ""
        };
        println!(
            "{}\t{}\t{}\t{}",
            state.name,
            state.version,
            state.target.as_deref().unwrap_or(""),
            flag
        );
    }
    Ok(())
}

/// Marks `name` as held so `grm upgrade` skips it. Idempotent: holding a held package is a
/// no-op that still reports success. Fails when the package is not installed.
pub fn hold(args: PackageArg) -> Result<()> {
    if args.packages.is_empty() {
        bail!("specify at least one package to hold");
    }
    for package in &args.packages {
        set_hold(package, true, true)?;
    }
    Ok(())
}

pub fn unhold(args: PackageArg) -> Result<()> {
    if args.packages.is_empty() {
        bail!("specify at least one package to unhold");
    }
    for package in &args.packages {
        set_hold(package, false, true)?;
    }
    Ok(())
}

pub(crate) fn set_hold(name: &str, held: bool, announce: bool) -> Result<()> {
    let root = paths::install_root()?;
    let state_path = root
        .join("state")
        .join("packages")
        .join(format!("{name}.nuon"));
    if !state_path.exists() {
        bail!("package `{name}` is not installed");
    }
    let mut state = PackageState::from_value(nuon_io::read_nuon(&state_path)?)?;
    if state.held == held {
        if announce {
            report(&format!(
                "{name} is already {}",
                if held { "held" } else { "not held" }
            ));
        }
        return Ok(());
    }
    state.held = held;
    nuon_io::write_nuon(&state_path, &state.to_value())?;
    // The lock records holds, so a flag change must refresh it like any other state change.
    lock::rebuild()?;
    if announce {
        report(&format!(
            "{name} {}",
            if held { "held" } else { "released" }
        ));
    }
    Ok(())
}

/// Marks `name` as explicitly requested (or demotes it back to a dependency). `name` is
/// resolved like a dependency — an exact package name, a bin, or a provided capability — so
/// `grm install awk` marks whichever package actually satisfied `awk`. Returns whether the
/// flag actually changed: a promotion can pull a store-only package into the linked set, so
/// the caller may need to rebuild the generation even when nothing was installed.
pub(crate) fn set_requested(name: &str, requested: bool, announce: bool) -> Result<bool> {
    let states = installed_states()?;
    let Some(found) = find_dep_state(&states, name) else {
        bail!("package `{name}` is not installed");
    };
    let mut state = found.clone();
    let state_path = paths::install_root()?
        .join("state")
        .join("packages")
        .join(format!("{}.nuon", state.name));
    if state.requested == requested {
        if announce {
            report(&format!(
                "{} is already {}",
                state.name,
                if requested {
                    "requested"
                } else {
                    "a dependency"
                }
            ));
        }
        return Ok(false);
    }
    state.requested = requested;
    nuon_io::write_nuon(&state_path, &state.to_value())?;
    // The lock records install reasons, so a flag change must refresh it.
    lock::rebuild()?;
    if announce {
        report(&format!(
            "{} marked as {}",
            state.name,
            if requested {
                "requested"
            } else {
                "a dependency"
            }
        ));
    }
    Ok(true)
}

/// The packages a generation actually surfaces: every requested or held package plus its
/// transitive runtime dependency closure (edges resolved by name, bin, or capability, like the
/// solver and the orphan sweep). Everything else in state — cached build dependencies, residue
/// from a failed multi-package install — is *store-only*: kept for reuse, never linked into
/// the profile, absent from the lockfile, and ignored by a bare `grm upgrade`.
pub(crate) fn linked_set(states: &[PackageState]) -> HashSet<String> {
    let mut linked: HashSet<String> = HashSet::new();
    let mut queue: VecDeque<&PackageState> = states
        .iter()
        .filter(|state| state.requested || state.held)
        .collect();
    while let Some(state) = queue.pop_front() {
        if !linked.insert(state.name.clone()) {
            continue;
        }
        for dep in &state.runtime_deps {
            if let Some(dep_state) = find_dep_state(states, dep)
                && !linked.contains(&dep_state.name)
            {
                queue.push_back(dep_state);
            }
        }
    }
    linked
}

pub fn installed_states() -> Result<Vec<PackageState>> {
    let state_dir = paths::install_root()?.join("state").join("packages");
    if !state_dir.exists() {
        return Ok(Vec::new());
    }

    let mut states = Vec::new();
    for entry in fs::read_dir(&state_dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("nuon") {
            continue;
        }
        let state = PackageState::from_value(nuon_io::read_nuon(&path)?)
            .with_context(|| format!("read package state {}", path.display()))?;
        states.push(state);
    }
    states.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(states)
}

/// Installed package names mapped to their concrete versions, for the solver. Recorded state
/// versions were validated as semver when written, so an unparsable one is skipped defensively.
pub(crate) fn installed_versions() -> Result<BTreeMap<String, Version>> {
    let mut versions = BTreeMap::new();
    for state in installed_states()? {
        if let Ok(version) = parse_version_relaxed(&state.version) {
            versions.insert(state.name, version);
        }
    }
    Ok(versions)
}

/// Like [`installed_versions`], but omitting packages whose installed bits have drifted from
/// their current rune — same version, different store hash (see
/// [`crate::store::closure::stale_installed`]). Handed to the solver, the omission makes it
/// re-realize a drifted package at its current address instead of reusing it by version, so a
/// rune edit propagates to the next install instead of being shadowed by version equality
/// forever.
pub(crate) fn installed_versions_current() -> Result<BTreeMap<String, Version>> {
    let states = installed_states()?;
    let stale: HashSet<String> = crate::store::closure::stale_installed(&states)
        .into_iter()
        .collect();
    let mut versions = BTreeMap::new();
    for state in states {
        if stale.contains(&state.name) {
            // A transient/verbose line, not a result line: a stale package is only
            // re-realized if this command's graph actually needs it (and a dry run realizes
            // nothing) — promising "reinstalling" here would often be false. The build-dep
            // path reports loudly at the point it really does reinstall.
            status(&format!(
                "{} {} drifted from its rune; not reusable",
                state.name, state.version
            ));
            continue;
        }
        if let Ok(version) = parse_version_relaxed(&state.version) {
            versions.insert(state.name, version);
        }
    }
    Ok(versions)
}
