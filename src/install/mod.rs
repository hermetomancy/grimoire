//! Installing, removing, and upgrading packages.
//!
//! [`install`] resolves a package and its dependencies through the solver, then realizes each
//! step — fetching and verifying a binary archive or building a rune from source — into the
//! install root. Every install stages into a transaction directory and promotes with atomic
//! renames, rolling back the active profile and state on failure (AGENTS.md §9). `--locked` constrains
//! resolution to the lockfile's recorded versions and hashes for a reproducible reinstall.

use anyhow::{Context, Result, bail};
use semver::Version;
use std::{
    collections::{BTreeMap, HashSet},
    path::PathBuf,
};

use crate::{
    cli::InstallArgs,
    profile,
    solve::{self, Plan, PlanStep},
    util::paths,
    util::progress::report,
};

pub(crate) mod lock;
mod steps;

mod build_deps;
mod orphans;
mod realize;
mod state;
mod transaction;

pub(crate) use build_deps::*;
pub use orphans::*;
pub(crate) use realize::*;
pub use state::*;
pub(crate) use transaction::*;

/// Drives one top-level install and its dependency tree. `installed` maps already-installed
/// package names to their versions; it is read from disk once up front, handed to the solver so
/// it can reuse satisfying installs, and updated as packages land. `building` records names whose
/// source build is in progress so a build-dependency cycle terminates instead of recursing.
struct Installer {
    installed: BTreeMap<String, Version>,
    /// Lockfile pins for a `--locked` install: resolution is constrained to these exact
    /// versions/hashes. `None` for an ordinary install, which resolves freely.
    pins: Option<solve::Pins>,
    building: HashSet<String>,
    /// Packages actually (re)installed during this run, in install order. Used to print a final
    /// summary and to detect the "nothing to do" case where every requested package was already
    /// satisfied and the solver produced no steps.
    installed_now: Vec<String>,
    /// Post-install notes collected from each installed package, printed once after the whole
    /// command commits so they land after the progress output instead of interleaved with it.
    notes: Vec<(String, Vec<String>)>,
    /// When true, every install path stops after planning and prints the plan to stdout —
    /// no fetches, no builds, no state writes. Wired from `--dry-run` / `--explain`.
    dry_run: bool,
}

impl Installer {
    fn new(installed: BTreeMap<String, Version>, pins: Option<solve::Pins>) -> Self {
        Self {
            installed,
            pins,
            building: HashSet::new(),
            installed_now: Vec::new(),
            notes: Vec::new(),
            dry_run: false,
        }
    }

    fn with_dry_run(mut self, dry_run: bool) -> Self {
        self.dry_run = dry_run;
        self
    }

    /// Builds a new generation from the current installed state and atomically activates it.
    /// Called once after all install/remove/upgrade operations complete.
    fn finalize(&self) -> Result<()> {
        if self.dry_run {
            return Ok(());
        }
        let states = installed_states()?;
        profile::rebuild_and_activate(&states)?;
        Ok(())
    }
}

pub fn install(args: InstallArgs) -> Result<()> {
    if let Some(msg) = paths::fixed_store_setup_instructions() {
        bail!("{msg}");
    }
    if args.packages.is_empty() {
        bail!("specify at least one package to install");
    }
    if args.sha256.is_some() && args.packages.len() > 1 {
        bail!("--sha256 is only valid when installing a single local archive");
    }

    let pins = if args.locked {
        enforce_locked_tome_commits(&lock::lock_path()?)?;
        Some(load_pins()?)
    } else {
        None
    };
    // Under `--locked`, only reuse an installed package when it matches its pin; an installed
    // version that drifted from the lock must be re-resolved to the pinned one.
    let mut installed = installed_versions()?;
    if let Some(pins) = &pins {
        installed.retain(|name, version| pins.get(name).is_some_and(|pin| &pin.version == version));
    }

    let mut installer = Installer::new(installed, pins).with_dry_run(args.dry_run);

    let mut root_names = Vec::new();
    for package in &args.packages {
        let name = if args.from_source || package.ends_with(".rn") {
            installer.install_source_root(package)?
        } else if PathBuf::from(package).exists() || package.ends_with(".tar.zst") {
            installer.install_local_root(package, args.sha256.clone())?
        } else {
            installer.install_named(package)?
        };
        root_names.push(name);
    }

    if args.dry_run {
        return Ok(());
    }

    // The user asked for these by name, so they are exempt from orphan cleanup — including
    // when the package was already installed as a mere dependency and this install produced
    // no steps: an explicit install promotes it. The marking sits outside the per-package
    // transactions; it is idempotent, and a crash here just leaves a root marked as a dep,
    // fixed by re-running the install.
    for name in &root_names {
        set_requested(name, true, false)?;
    }

    // A resolve that reuses an already-satisfying install produces no steps, so nothing above
    // reported anything. Tell the user the request was a no-op rather than printing silence.
    // Skip creating a new generation when nothing actually changed.
    if installer.installed_now.is_empty() {
        let names = args.packages.join(", ");
        report(&format!("{names} already installed and up to date"));
        return Ok(());
    }
    installer.finalize()?;
    installer.report_notes();
    Ok(())
}

/// Loads lockfile pins for a `--locked` install. A missing lockfile is a hard error: there is
/// nothing to reproduce, so the flag cannot be honored.
fn load_pins() -> Result<solve::Pins> {
    let Some(packages) = lock::read_locked_packages()? else {
        bail!("no lockfile found; run an install first to record `grimoire.lock.nuon`");
    };
    Ok(packages
        .into_iter()
        .map(|pkg| {
            (
                pkg.name,
                solve::Pin {
                    version: pkg.version,
                    archive_hash: pkg.archive_hash,
                    store_hash: pkg.store_hash,
                },
            )
        })
        .collect())
}

/// Restores the package set a lockfile records: installs every `requested` package under the
/// lock's pins, restores `requested`/`held` intent for everything the lock describes, then
/// sweeps orphans — converging the install root on exactly the recorded set.
pub fn restore(args: crate::cli::RestoreArgs) -> Result<()> {
    if let Some(msg) = paths::fixed_store_setup_instructions() {
        bail!("{msg}");
    }
    let lock_file = match &args.lockfile {
        Some(path) => path.clone(),
        None => lock::lock_path()?,
    };
    let packages = lock::read_locked_packages_from(&lock_file)?
        .with_context(|| format!("no lockfile at {}", lock_file.display()))?;
    enforce_locked_tome_commits(&lock_file)?;

    let pins: solve::Pins = packages
        .iter()
        .map(|pkg| {
            (
                pkg.name.clone(),
                solve::Pin {
                    version: pkg.version.clone(),
                    archive_hash: pkg.archive_hash.clone(),
                    store_hash: pkg.store_hash.clone(),
                },
            )
        })
        .collect();
    let requested: Vec<String> = packages
        .iter()
        .filter(|pkg| pkg.requested)
        .map(|pkg| pkg.name.clone())
        .collect();
    if requested.is_empty() {
        bail!(
            "lockfile {} records no requested packages to restore (locks written before \
             install-reason tracking cannot drive a restore)",
            lock_file.display()
        );
    }

    // Reuse an installed package only when it already matches its pin, like `--locked`.
    let mut installed = installed_versions()?;
    installed.retain(|name, version| pins.get(name).is_some_and(|pin| &pin.version == version));
    let mut installer = Installer::new(installed, Some(pins));
    for name in &requested {
        installer
            .install_named(name)
            .with_context(|| format!("restore `{name}`"))?;
    }

    // Restore the recorded intent for every locked package that is now installed, then sweep
    // whatever the lock does not account for as a dependency.
    let states = installed_states()?;
    for pkg in &packages {
        if states.iter().any(|state| state.name == pkg.name) {
            set_requested(&pkg.name, pkg.requested, false)?;
            set_hold(&pkg.name, pkg.held, false)?;
        }
    }
    let seeds: Vec<String> = installed_states()?
        .iter()
        .filter(|state| !state.requested && !state.held)
        .map(|state| state.name.clone())
        .collect();
    autoremove_orphans(seeds)?;

    installer.finalize()?;
    installer.report_notes();
    report(&format!(
        "restored {} requested package(s) from {}",
        requested.len(),
        lock_file.display()
    ));
    Ok(())
}

/// Refuses a locked operation when a tome's cache has moved off the commit the lock records.
/// Without this, a moved ref silently changes the candidate universe `--locked` resolves
/// against. Tomes without a recorded commit (local-path tomes) are skipped.
fn enforce_locked_tome_commits(lock_file: &std::path::Path) -> Result<()> {
    let Some(tomes) = lock::read_locked_tomes_from(lock_file)? else {
        return Ok(());
    };
    for locked in tomes {
        let Some(pinned) = locked.commit else {
            continue;
        };
        let cache = crate::catalog::sync_common::cache_path("tomes", &locked.name)?;
        let actual = if cache.exists() {
            crate::tome::git::head_commit(&cache)?
        } else {
            None
        };
        verify_pinned_tome_commit(&locked.name, &pinned, actual.as_deref())?;
    }
    Ok(())
}

fn verify_pinned_tome_commit(name: &str, pinned: &str, actual: Option<&str>) -> Result<()> {
    match actual {
        Some(actual) if actual == pinned => Ok(()),
        Some(actual) => bail!(
            "tome `{name}` is at commit {actual} but the lockfile pins {pinned}; the catalog \
             moved since the lock was written. Re-sync the tome at the pinned commit, or run a \
             normal install to refresh the lock"
        ),
        None => bail!(
            "tome `{name}` has no synced commit but the lockfile pins {pinned}; run \
             `grm tome update {name}` first"
        ),
    }
}

fn require_store_hash(step: &PlanStep) -> Result<&str> {
    step.store_hash
        .as_deref()
        .with_context(|| format!("cannot compute store hash for `{}`", step.name))
}

/// Reinstalls each package in `names` at the newest available version, for `upgrade`. The named
/// packages are dropped from the known-installed set so the solver re-resolves them to the newest
/// candidate instead of reusing the currently installed (older) version; every other installed
/// package is still reused to satisfy dependencies.
pub fn upgrade_packages(names: &[String]) -> Result<()> {
    if let Some(msg) = paths::fixed_store_setup_instructions() {
        bail!("{msg}");
    }
    // An upgrade can drop dependency edges (the new version no longer needs a lib); capture
    // the pre-upgrade edges so the stale ones can be swept once the upgrades land.
    let pre_upgrade_deps: Vec<String> = installed_states()?
        .iter()
        .filter(|state| names.contains(&state.name))
        .flat_map(|state| state.runtime_deps.iter().cloned())
        .collect();
    let mut installed = installed_versions()?;
    for name in names {
        installed.remove(name);
    }
    let mut installer = Installer::new(installed, None);
    for name in names {
        installer
            .install_named(name)
            .with_context(|| format!("upgrade `{name}`"))?;
    }
    if installer.installed_now.is_empty() {
        report("all requested packages are already up to date");
        return Ok(());
    }
    // Sweep before finalize() so the single new generation reflects both the upgrades and
    // the removals. Each autoremove is its own committed transaction; a failure mid-sweep
    // leaves the upgrades committed and the sweep partial, same containment as `remove`.
    autoremove_orphans(pre_upgrade_deps)?;
    installer.finalize()?;
    installer.report_notes();
    Ok(())
}

/// Prints a complete solver plan (header + body). For a `--dry-run` whose root step is the
/// solver-resolved package itself.
fn print_plan(plan: &Plan) {
    if plan.steps.is_empty() {
        println!("plan: already satisfied (no install steps)");
        return;
    }
    println!("plan:");
    print_plan_body(plan);
}

/// Prints just the bullet list of plan steps, without the header — used when a `--dry-run`
/// has already printed a synthetic root step (source-rune or local-archive install).
fn print_plan_body(plan: &Plan) {
    for step in &plan.steps {
        println!(
            "  + {} {} ({})",
            step.name,
            step.version,
            describe_origin(step)
        );
    }
}

/// A human-readable summary of how a step will be realized. When both a prebuilt and a rune are
/// available the exact route depends on the store-hash match resolved at install time, so the plan
/// names the prebuilt archive and notes that a source build is the fallback.
fn describe_origin(step: &PlanStep) -> String {
    match (step.substitutes.first(), &step.rune) {
        (Some(sub), Some(_)) => format!("binary archive {} or source", sub.entry.archive),
        (Some(sub), None) => format!("binary archive {}", sub.entry.archive),
        (None, Some(rune)) => format!("source rune {}", rune.display()),
        (None, None) => "unavailable".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::verify_pinned_tome_commit;

    #[test]
    fn pinned_commit_matching_head_is_accepted() {
        assert!(verify_pinned_tome_commit("core", "abc123", Some("abc123")).is_ok());
    }

    #[test]
    fn moved_ref_is_refused() {
        let err = verify_pinned_tome_commit("core", "abc123", Some("def456")).unwrap_err();
        assert!(err.to_string().contains("the catalog moved"), "{err}");
    }

    #[test]
    fn missing_commit_is_refused() {
        let err = verify_pinned_tome_commit("core", "abc123", None).unwrap_err();
        assert!(err.to_string().contains("no synced commit"), "{err}");
    }
}
