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
    build,
    cli::InstallArgs,
    profile,
    solve::{self, Plan, PlanStep},
    util::paths,
    util::output::report,
};

pub(crate) mod lock;
mod steps;

mod build_deps;
mod orphans;
mod realize;
mod state;
mod transaction;
mod world;

pub(crate) use build_deps::*;
pub use orphans::*;
pub(crate) use realize::*;
pub use state::*;
pub(crate) use transaction::*;
pub use world::InstalledWorld;

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
    /// Authoritative in-memory installed state for this command. Mutations accumulate here and
    /// commit at the single transaction boundary in [`finalize`].
    world: InstalledWorld,
}

impl Installer {
    fn new(
        installed: BTreeMap<String, Version>,
        pins: Option<solve::Pins>,
        dry_run: bool,
        world: InstalledWorld,
    ) -> Self {
        Self {
            installed,
            pins,
            building: HashSet::new(),
            installed_now: Vec::new(),
            notes: Vec::new(),
            dry_run,
            world,
        }
    }

    /// Builds a new generation from the current installed state and atomically activates it.
    /// Called once after all install/remove/upgrade operations complete.
    fn finalize(&mut self) -> Result<()> {
        if self.dry_run {
            return Ok(());
        }
        let mut tx = Transaction::new();
        self.world.commit(&mut tx)?;
        finalize_state(&mut tx, &self.world)?;
        tx.commit();
        Ok(())
    }
}

/// Single commit point for the user-visible environment. Rebuilds the lockfile and activates
/// a generation from the authoritative installed state. Call exactly once at the end of every
/// mutating command, after all `state/packages/*.nuon` changes have landed.
pub fn finalize_state(tx: &mut Transaction, world: &InstalledWorld) -> Result<()> {
    realize::rebuild_lock(tx, world)?;
    profile::rebuild_and_activate(world)?;
    Ok(())
}

/// Whether an install argument denotes a local archive *file* rather than a package name to
/// resolve from tomes. A package name is a bare identifier — no path separators, no leading `.`
/// (see [`crate::model::value::validate_ident`]) — so an argument is only a local archive when it
/// *looks* like a path: it carries the `.tar.zst` extension, contains a path separator, or begins
/// with `.` (`./`, `../`).
///
/// The test is deliberately syntactic and never touches the filesystem. Routing on
/// `Path::exists()` instead would let any cwd entry whose name happens to match a package shadow
/// the named install — most painfully a `grimoire/` *directory* (a source checkout) sitting next
/// to `grm install grimoire`, which then handed the directory to the archive staging code and
/// failed with a cryptic `Is a directory (os error 21)`. Whether a path-shaped argument actually
/// resolves to a readable archive is [`Installer::install_local_root`]'s job to report.
fn is_local_archive_arg(package: &str) -> bool {
    package.ends_with(".tar.zst")
        || package.starts_with('.')
        || package.contains('/')
        || package.contains('\\')
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

    let world = InstalledWorld::load_default()?;
    let pins = if args.locked {
        enforce_locked_tome_commits(&lock::lock_path()?)?;
        Some(load_pins()?)
    } else {
        None
    };
    // Under `--locked`, only reuse an installed package when it matches its pin; an installed
    // version that drifted from the lock must be re-resolved to the pinned one.
    let mut installed = world.installed_versions_current()?;
    if let Some(pins) = &pins {
        installed.retain(|name, version| pins.get(name).is_some_and(|pin| &pin.version == version));
    }

    let mut installer = Installer::new(installed, pins, args.dry_run, world);

    // Announce implied work before the first fetch: a one-line install can pull a long
    // tail of missing or drifted build deps, and the user deserves the count (and the
    // big names) up front rather than 40 minutes in. Best-effort and names-only — rune
    // paths and local archives resolve too late to preview cheaply.
    if !args.dry_run {
        let plain: Vec<String> = args
            .packages
            .iter()
            .filter(|package| {
                !args.from_source && !package.ends_with(".rn") && !is_local_archive_arg(package)
            })
            .cloned()
            .collect();
        if let Ok(extra) = estimate_extra_realizations(&plain)
            && !extra.is_empty()
        {
            let shown: Vec<&str> = extra.iter().take(6).map(String::as_str).collect();
            let ellipsis = if extra.len() > shown.len() {
                ", …"
            } else {
                ""
            };
            crate::util::output::note(&format!(
                "+ {} build dep{} to realize: {}{ellipsis}",
                crate::util::output::strong(&extra.len().to_string()),
                if extra.len() == 1 { "" } else { "s" },
                shown.join(", ")
            ));
        }
    }

    let mut root_names = Vec::new();
    for package in &args.packages {
        let name = if args.from_source || package.ends_with(".rn") {
            installer.install_source_root(package)?
        } else if is_local_archive_arg(package) {
            installer.install_local_root(package, args.sha256.clone())?
        } else {
            settle_capability_intent(package, args.dry_run, args.locked)?;
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
    let mut promoted = false;
    for name in &root_names {
        promoted |= set_requested(&mut installer.world, name, true, false)?;
    }

    // A resolve that reuses an already-satisfying install produces no steps, so nothing above
    // reported anything. Tell the user the request was a no-op rather than printing silence.
    // Skip creating a new generation when nothing actually changed — but a promotion alone
    // does change it: requesting a store-only package (e.g. a cached build dep) pulls it into
    // the linked set, so the generation must be rebuilt to surface it.
    if installer.installed_now.is_empty() {
        let names = args.packages.join(", ");
        if !promoted {
            // State alone is not proof the environment is current: an earlier run can commit
            // its package transactions and then fail to build the generation (e.g. a contested
            // bin refusing the link). Relink in that case so the re-run converges — or repeats
            // the link error — instead of reporting success over a stale environment.
            if !profile::current_generation_is_stale(&installer.world)? {
                report(&format!("{names} already installed and up to date"));
                return Ok(());
            }
            report(&format!(
                "{names} already installed; relinking the out-of-date generation"
            ));
        } else {
            report(&format!("{names} already installed; marked as requested"));
        }
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
    let world = InstalledWorld::load_default()?;
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
    let mut installed = world.installed_versions_current()?;
    installed.retain(|name, version| pins.get(name).is_some_and(|pin| &pin.version == version));
    let mut installer = Installer::new(installed, Some(pins), args.dry_run, world);
    for name in &requested {
        installer
            .install_named(name)
            .with_context(|| format!("restore `{name}`"))?;
    }
    if args.dry_run {
        // The lock is the blueprint: after the per-package plans, say what falls outside it.
        let recorded: std::collections::HashSet<&str> =
            packages.iter().map(|pkg| pkg.name.as_str()).collect();
        let states = installer.world.to_states();
        let strays: Vec<String> = states
            .iter()
            .filter(|state| !recorded.contains(state.name.as_str()))
            .filter(|state| !state.requested && !state.held)
            .map(|state| state.name.clone())
            .collect();
        let swept = simulate_orphan_sweep(&states, &[], &strays);
        for name in swept {
            crate::util::output::line(&format!(
                "  - {name} (not recorded in the lock; would be swept)"
            ));
        }
        return Ok(());
    }

    // Restore the recorded intent for every locked package that is now installed, then sweep
    // whatever the lock does not account for as a dependency.
    for pkg in &packages {
        if installer.world.contains(&pkg.name) {
            set_requested(&mut installer.world, &pkg.name, pkg.requested, false)?;
            set_hold(&mut installer.world, &pkg.name, pkg.held, false)?;
        }
    }
    let seeds: Vec<String> = installer
        .world
        .iter()
        .filter(|state| !state.requested && !state.held)
        .map(|state| state.name.clone())
        .collect();
    sweep_orphans(&mut installer.world, seeds)?;

    installer.finalize()?;
    installer.report_notes();
    report(&format!(
        "{} {}",
        crate::util::output::accent(&format!(
            "restored {} requested package(s)",
            requested.len()
        )),
        crate::util::output::faint(&format!("from {}", lock_file.display()))
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
    let world = InstalledWorld::load_default()?;
    // An upgrade can drop dependency edges (the new version no longer needs a lib); capture
    // the pre-upgrade edges so the stale ones can be swept once the upgrades land.
    let pre_upgrade_deps: Vec<String> = world
        .iter()
        .filter(|state| names.contains(&state.name))
        .flat_map(|state| state.runtime_deps.iter().cloned())
        .collect();
    let mut installed = world.installed_versions_current()?;
    for name in names {
        installed.remove(name);
    }
    let mut installer = Installer::new(installed, None, false, world);
    for name in names {
        installer
            .install_named(name)
            .with_context(|| format!("upgrade `{name}`"))?;
    }
    if installer.installed_now.is_empty() {
        if !profile::current_generation_is_stale(&installer.world)? {
            report("all requested packages are already up to date");
            return Ok(());
        }
        report(
            "all requested packages are already up to date; relinking the out-of-date generation",
        );
    }
    // Sweep before finalize() so the single new generation reflects both the upgrades and
    // the removals. Each swept dependency is its own committed transaction; a failure
    // mid-sweep leaves the upgrades committed and the sweep partial, same containment as
    // `remove`.
    sweep_orphans(&mut installer.world, pre_upgrade_deps)?;
    installer.finalize()?;
    installer.report_notes();
    Ok(())
}

/// Prints a complete solver plan (header + body). For a `--dry-run` whose root step is the
/// solver-resolved package itself.
/// When a *human* explicitly installs a capability name (`grm install sed`) that several
/// packages provide and no preference exists, ask which implementation they meant and
/// record the answer as a `grm prefer` choice — resolution stays deterministic afterwards.
/// Only explicit requests prompt: rune-declared deps keep the deterministic chain
/// (preference → installed → first by name), because plan hashing and non-interactive
/// bootstraps cannot answer questions. Literal package names, locked installs, and
/// already-settled preferences pass through untouched; without a terminal the ambiguity is
/// an error naming the providers.
fn settle_capability_intent(name: &str, dry_run: bool, locked: bool) -> Result<()> {
    if locked {
        return Ok(()); // the lockfile already pinned a concrete provider
    }
    // A literal package (rune or published prebuilt) is not a capability request.
    if solve::newest_available(name)?.is_some() {
        return Ok(());
    }
    let providers = solve::capability_providers(name)?;
    if providers.len() < 2 {
        return Ok(()); // zero providers fails in resolution with the normal error
    }
    let preferences = crate::model::preferences::Preferences::load()?;
    if preferences.providers.contains_key(name) {
        return Ok(());
    }
    let mut providers = providers;
    providers.sort();
    providers.dedup();
    if dry_run {
        crate::util::output::warn(&format!(
            "`{name}` is a capability with multiple providers ({}); a real install will ask \
             which one you mean",
            providers.join(", ")
        ));
        return Ok(());
    }
    if !std::io::IsTerminal::is_terminal(&std::io::stdin()) {
        bail!(
            "`{name}` is provided by multiple packages: {}; choose one with \
             `grm prefer {name} <package>` and retry",
            providers.join(", ")
        );
    }

    crate::util::output::line(&format!("`{name}` is provided by multiple packages:"));
    for (index, provider) in providers.iter().enumerate() {
        crate::util::output::line(&format!("  {}) {provider}", index + 1));
    }
    crate::util::output::prompt(&format!(
        "which should provide `{name}`? [1-{}]: ",
        providers.len()
    ));
    let mut answer = String::new();
    std::io::stdin()
        .read_line(&mut answer)
        .context("read provider choice")?;
    let answer = answer.trim();
    let chosen = answer
        .parse::<usize>()
        .ok()
        .and_then(|index| index.checked_sub(1))
        .and_then(|index| providers.get(index))
        .cloned()
        .or_else(|| providers.iter().find(|p| p.as_str() == answer).cloned())
        .with_context(|| format!("`{answer}` is not one of the listed providers"))?;

    let mut preferences = crate::model::preferences::Preferences::load()?;
    preferences
        .providers
        .insert(name.to_owned(), chosen.clone());
    preferences.save()?;
    report(&format!("`{name}` now provided by `{chosen}`"));
    Ok(())
}

/// Plan-time safety net for the realize-time conflict gate. The resolver now avoids most
/// conflicting selections by backtracking, but this gate remains for cases the resolver's
/// metadata is incomplete (e.g. an installed package whose rune is no longer available).
/// It refuses the plan while a *linked* installed package (or another planned step) conflicts
/// with a step, before anything is fetched or built. Packages a step replaces are exempt —
/// the migration removes them in the same transaction. Store-only packages never conflict
/// (cache, not environment).
fn refuse_plan_conflicts(plan: &Plan) -> Result<()> {
    let world = InstalledWorld::load_default()?;
    let states = world.to_states();
    let mut linked = world.linked_immut();
    let replaced: std::collections::HashSet<&str> = plan
        .steps
        .iter()
        .flat_map(|step| step.replaces.iter().map(String::as_str))
        .collect();
    linked.retain(|name| !replaced.contains(name.as_str()));

    for step in &plan.steps {
        if let Some(hit) = states.iter().find(|state| {
            state.name != step.name
                && linked.contains(&state.name)
                && (step.conflicts.contains(&state.name) || state.conflicts.contains(&step.name))
        }) {
            bail!(
                "cannot install `{}`: it conflicts with installed `{}`; remove `{}` first",
                step.name,
                hit.name,
                hit.name
            );
        }
        if let Some(other) = plan.steps.iter().find(|other| {
            other.name != step.name
                && (step.conflicts.contains(&other.name) || other.conflicts.contains(&step.name))
        }) {
            bail!(
                "cannot install `{}` and `{}` together: they conflict",
                step.name,
                other.name
            );
        }
    }
    Ok(())
}

fn print_plan(plan: &Plan, installed: &BTreeMap<String, Version>) -> Result<()> {
    if plan.steps.is_empty() {
        crate::util::output::line("plan: already satisfied (no install steps)");
        return Ok(());
    }
    crate::util::output::line("plan:");
    print_plan_body(plan);
    print_plan_consequences(plan, installed)
}

/// Prints just the bullet list of plan steps, without the header — used when a `--dry-run`
/// has already printed a synthetic root step (source-rune or local-archive install).
fn print_plan_body(plan: &Plan) {
    for step in &plan.steps {
        crate::util::output::plan_item(
            '+',
            &format!("{} {} ({})", step.name, step.version, describe_origin(step)),
        );
    }
}

/// Prints everything a plan would pull in *beyond* its own steps: migrations of installed
/// packages a step replaces, and the transitive build-dependency closure of every step that
/// may build from source (installed store-only, never linked).
fn print_plan_consequences(plan: &Plan, installed: &BTreeMap<String, Version>) -> Result<()> {
    let world = InstalledWorld::load_default()?;
    let linked = world.linked_immut();
    for step in &plan.steps {
        for old in &step.replaces {
            if old != &step.name && linked.contains(old) {
                crate::util::output::plan_item('~', &format!("{old} → {} (replaced)", step.name));
            }
        }
    }

    for (for_name, dep_step) in build_dep_closure(&plan.steps, installed)? {
        crate::util::output::plan_item(
            '+',
            &format!(
                "{} {} ({}; build dep of {for_name}, store-only)",
                dep_step.name,
                dep_step.version,
                describe_origin(&dep_step)
            ),
        );
    }
    Ok(())
}

/// Walks the transitive build-dependency closure of every source-built step, resolving
/// each layer the way the build would: deps already installed *and current* are reused
/// and skipped; missing or drifted ones become steps. Returns each discovered step paired
/// with the package that pulled it in; `seen` starts from the plan's own steps so shared
/// build deps appear once.
fn build_dep_closure(
    roots: &[PlanStep],
    installed: &BTreeMap<String, Version>,
) -> Result<Vec<(String, PlanStep)>> {
    let target = paths::target_triple();
    let mut seen: std::collections::HashSet<String> =
        roots.iter().map(|step| step.name.clone()).collect();
    let mut queue: Vec<(String, PathBuf)> = roots
        .iter()
        .filter_map(|step| step.rune.clone().map(|rune| (step.name.clone(), rune)))
        .collect();
    let mut closure = Vec::new();
    while let Some((for_name, rune)) = queue.pop() {
        let metadata =
            build::read_rune_metadata(&rune, build::tome_name_for_rune(&rune)?.as_deref())?;
        let build_deps = build::effective_build_deps(&rune, &metadata, &target)?;
        if build_deps.is_empty() {
            continue;
        }
        let dep_plan = solve::resolve(&build_deps, installed, &HashSet::new(), None)
            .with_context(|| format!("plan build dependencies for `{for_name}`"))?;
        for dep_step in dep_plan.steps {
            if !seen.insert(dep_step.name.clone()) {
                continue;
            }
            if let Some(dep_rune) = dep_step.rune.clone() {
                queue.push((dep_step.name.clone(), dep_rune));
            }
            closure.push((for_name.clone(), dep_step));
        }
    }
    Ok(closure)
}

/// Best-effort preview of what realizing `names` pulls in beyond the named packages:
/// missing or drifted build dependencies, walked transitively the way the build would
/// resolve them. Feeds the "upgrading N packages" announcement, so a one-line upgrade
/// that implies an llvm rebuild says so before the first fetch instead of surprising the
/// user 40 minutes in. Best-effort by design — callers drop the preview on any error
/// rather than failing the operation over an announcement.
pub fn estimate_extra_realizations(names: &[String]) -> Result<Vec<String>> {
    let world = InstalledWorld::load_default()?;
    let mut installed = world.installed_versions_current()?;
    for name in names {
        installed.remove(name); // upgrades re-resolve the roots themselves
    }
    let deps: Vec<crate::model::Dependency> = names
        .iter()
        .map(|name| crate::model::Dependency::any(name.clone()))
        .collect();
    let linked = world.linked_immut();
    let plan = solve::resolve(&deps, &installed, &linked, None)?;
    Ok(build_dep_closure(&plan.steps, &installed)?
        .into_iter()
        .map(|(_, step)| step.name)
        .filter(|name| !names.contains(name))
        .collect())
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
