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
    cli::PreferArgs,
    install,
    model::preferences::Preferences,
    profile,
    util::progress::{self, report},
    util::table::{self, Cell},
};

pub fn prefer(args: PreferArgs) -> Result<()> {
    match (&args.capability, &args.package, args.unset) {
        (None, None, false) => list(),
        (Some(capability), None, true) => unset(capability, args.dry_run),
        (Some(capability), Some(package), false) => set(capability, package, args.dry_run),
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
        println!("{}", progress::strong("preferred:"));
        let rows = preferences
            .providers
            .iter()
            .map(|(capability, package)| {
                vec![
                    Cell::plain(format!("  {capability}")),
                    Cell::strong(package),
                ]
            })
            .collect();
        table::print_rows(rows);
    }

    let mut contested: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let states = install::installed_states()?;
    // Only the linked set can contest: store-only packages never reach a generation.
    let linked = install::linked_set(&states);
    for state in states.iter().filter(|state| linked.contains(&state.name)) {
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
        println!("{}", progress::strong("contested (no preference set):"));
        let rows = contested
            .into_iter()
            .map(|(capability, providers)| {
                vec![
                    Cell::caution(format!("  {capability}")),
                    Cell::plain(providers.join(", ")),
                ]
            })
            .collect();
        table::print_rows(rows);
    }
    Ok(())
}

fn set(capability: &str, package: &str, dry_run: bool) -> Result<()> {
    validate_provider(capability, package)?;
    if dry_run {
        let claimants = installed_claimants(capability)?;
        println!(
            "would set `{capability}` → `{package}`{}",
            if claimants >= 2 {
                " and relink the active generation"
            } else {
                ""
            }
        );
        return Ok(());
    }
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

fn unset(capability: &str, dry_run: bool) -> Result<()> {
    let mut preferences = Preferences::load()?;
    if !preferences.providers.contains_key(capability) {
        report(&format!("no preference set for `{capability}`"));
        return Ok(());
    }
    if dry_run {
        println!("would clear the preference for `{capability}`");
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
    let providers = crate::cmd::files::capability_providers_detailed(capability)?;
    if providers.contains_key(package) {
        return Ok(());
    }
    if providers.is_empty() {
        bail!("nothing provides `{capability}` in installed packages or configured tomes");
    }
    // `providers` is a BTreeMap, so its keys are already sorted.
    bail!(
        "`{package}` does not provide `{capability}`; providers are: {}",
        providers.keys().cloned().collect::<Vec<_>>().join(", ")
    );
}

/// How many installed packages claim `capability` as a bin — two or more means a
/// preference change relinks the active generation.
fn installed_claimants(capability: &str) -> Result<usize> {
    Ok(install::installed_states()?
        .iter()
        .filter(|state| state.bins.contains_key(capability))
        .count())
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
