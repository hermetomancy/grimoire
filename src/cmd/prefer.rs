//! The `grm prefer` command: choose which package provides a contested capability or bin.
//!
//! Preferences are consulted in two places: the solver, when expanding a capability dependency
//! with multiple providers, and generation linking, when two installed packages declare the
//! same bin name. Setting or unsetting a preference relinks the active generation when the
//! choice affects installed packages. Under `--locked` installs the lockfile still wins: a
//! preference pointing at an unpinned provider fails loudly rather than drifting.

use anyhow::{Result, bail};
use std::collections::BTreeMap;

use crate::{
    cli::PreferArgs, cmd::query, install, model::preferences::Preferences, profile, solve,
    util::paths, util::progress::report,
};

pub fn prefer(args: PreferArgs) -> Result<()> {
    match (&args.capability, &args.package, args.unset) {
        (None, None, false) => list(),
        (Some(capability), None, true) => unset(capability),
        (Some(capability), Some(package), false) => set(capability, package),
        (Some(_), None, false) => {
            bail!("specify the package that should provide the capability, or pass --unset")
        }
        (None, _, true) => bail!("--unset requires the capability to clear"),
        (Some(_), Some(_), true) => bail!("--unset takes only the capability, not a package"),
        (None, Some(_), false) => unreachable!("clap requires capability before package"),
    }
}

/// Prints the recorded preferences, then any currently contested capabilities — bin names
/// declared by more than one installed package and capabilities with multiple providers —
/// that have no preference yet.
fn list() -> Result<()> {
    let preferences = Preferences::load()?;
    if !preferences.providers.is_empty() {
        println!("preferred:");
        for (capability, package) in &preferences.providers {
            println!("  {capability}\t{package}");
        }
    }

    let mut contested: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let states = install::installed_states()?;
    for state in &states {
        for bin_name in state.bins.keys() {
            contested
                .entry(bin_name.clone())
                .or_default()
                .push(state.name.clone());
        }
    }
    contested.retain(|capability, providers| {
        providers.len() > 1 && !preferences.providers.contains_key(capability)
    });

    if contested.is_empty() && preferences.providers.is_empty() {
        report("no preferences set and no contested capabilities");
        return Ok(());
    }
    if !contested.is_empty() {
        println!("contested (no preference set):");
        for (capability, providers) in contested {
            println!("  {capability}\t{}", providers.join(", "));
        }
    }
    Ok(())
}

fn set(capability: &str, package: &str) -> Result<()> {
    validate_provider(capability, package)?;
    let mut preferences = Preferences::load()?;
    let previous = preferences
        .providers
        .insert(capability.to_owned(), package.to_owned());
    if previous.as_deref() == Some(package) {
        report(&format!("`{capability}` already prefers `{package}`"));
        return Ok(());
    }
    preferences.save()?;
    report(&format!("`{capability}` now provided by `{package}`"));
    relink_if_contested(capability)
}

fn unset(capability: &str) -> Result<()> {
    let mut preferences = Preferences::load()?;
    if !preferences.providers.contains_key(capability) {
        report(&format!("no preference set for `{capability}`"));
        return Ok(());
    }
    // Refuse to clear a preference the active generation still depends on: without it the
    // next relink would fail on the contested bin, after the state change already landed.
    let states = install::installed_states()?;
    let claimants: Vec<&str> = states
        .iter()
        .filter(|state| state.bins.contains_key(capability))
        .map(|state| state.name.as_str())
        .collect();
    if claimants.len() > 1 {
        bail!(
            "clearing the preference for `{capability}` would leave it contested between {}; \
             remove one of those packages first",
            claimants.join(", ")
        );
    }
    preferences.providers.remove(capability);
    preferences.save()?;
    report(&format!("preference for `{capability}` cleared"));
    Ok(())
}

/// A preference must name a package that actually provides the capability — among installed
/// packages, configured tome runes, or published indexes. Refusing anything else keeps stale
/// preferences from accumulating silently; the error lists the real providers.
fn validate_provider(capability: &str, package: &str) -> Result<()> {
    let target = paths::target_triple();
    let mut providers: Vec<String> = Vec::new();

    for state in install::installed_states()? {
        if state.name == capability
            || state.bins.contains_key(capability)
            || state.provides.contains(&capability.to_owned())
        {
            providers.push(state.name.clone());
        }
    }
    for tome_package in query::tome_packages()? {
        let metadata = &tome_package.metadata;
        if metadata.name == capability
            || metadata.bins_for(&target).contains_key(capability)
            || metadata.provides.contains(&capability.to_owned())
        {
            providers.push(metadata.name.clone());
        }
    }
    providers.extend(solve::capability_providers(capability)?);
    providers.sort();
    providers.dedup();

    if providers.iter().any(|p| p == package) {
        return Ok(());
    }
    if providers.is_empty() {
        bail!("nothing provides `{capability}` in installed packages or configured tomes");
    }
    bail!(
        "`{package}` does not provide `{capability}`; providers are: {}",
        providers.join(", ")
    );
}

/// Rebuilds and activates a new generation when the preference change affects bins of
/// installed packages — i.e. at least two installed packages claim the capability as a bin.
/// A preference for not-yet-installed providers only steers future resolution; no relink.
fn relink_if_contested(capability: &str) -> Result<()> {
    let states = install::installed_states()?;
    let claimants = states
        .iter()
        .filter(|state| state.bins.contains_key(capability))
        .count();
    if claimants < 2 {
        return Ok(());
    }
    profile::rebuild_and_activate(&states)?;
    Ok(())
}
