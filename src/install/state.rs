//! Reading and flagging installed-package state: listing, hold/unhold, requested marking.

use anyhow::{Result, bail};
use semver::Version;
use std::{collections::BTreeMap, fs};

use crate::{
    cli::PackageArg,
    model::{PackageState, parse_version_relaxed},
    nu::nuon_io,
    paths,
    progress::report,
};

use super::*;

pub fn list() -> Result<()> {
    let states = installed_states()?;
    if states.is_empty() {
        println!("No packages are currently installed.");
        return Ok(());
    }
    for state in states {
        println!(
            "{}\t{}\t{}\t{}",
            state.name,
            state.version,
            state.target.as_deref().unwrap_or(""),
            if state.held { "held" } else { "" }
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
        set_hold(package, true)?;
    }
    Ok(())
}

pub fn unhold(args: PackageArg) -> Result<()> {
    if args.packages.is_empty() {
        bail!("specify at least one package to unhold");
    }
    for package in &args.packages {
        set_hold(package, false)?;
    }
    Ok(())
}

pub(crate) fn set_hold(name: &str, held: bool) -> Result<()> {
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
        report(&format!(
            "{name} is already {}",
            if held { "held" } else { "not held" }
        ));
        return Ok(());
    }
    state.held = held;
    nuon_io::write_nuon(&state_path, &state.to_value())?;
    report(&format!(
        "{name} {}",
        if held { "held" } else { "released" }
    ));
    Ok(())
}

/// Marks `name` as explicitly requested (or demotes it back to a dependency). `name` is
/// resolved like a dependency — an exact package name, a bin, or a provided capability — so
/// `grm install awk` marks whichever package actually satisfied `awk`.
pub(crate) fn set_requested(name: &str, requested: bool, announce: bool) -> Result<()> {
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
        return Ok(());
    }
    state.requested = requested;
    nuon_io::write_nuon(&state_path, &state.to_value())?;
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
    Ok(())
}

/// Demotes packages to dependency status so `grm autoremove` may reclaim them once nothing
/// references them. The inverse of the implicit promotion `grm install <name>` performs.
pub fn unrequest(args: PackageArg) -> Result<()> {
    if args.packages.is_empty() {
        bail!("specify at least one package to unrequest");
    }
    for package in &args.packages {
        set_requested(package, false, true)?;
    }
    Ok(())
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
