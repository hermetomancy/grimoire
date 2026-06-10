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
    path::{Path, PathBuf},
};

use crate::{
    build,
    cli::InstallArgs,
    fetch,
    model::{Dependency, validate_targets},
    profile,
    solve::{self, Plan, PlanStep, Substitute},
    tome,
    util::paths,
    util::progress::{report, status},
};

pub(crate) mod lock;

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
                },
            )
        })
        .collect())
}

impl Installer {
    /// Installs `name` and its transitive runtime dependencies. The solver picks a concrete
    /// version for every package in the graph and orders the plan so dependencies install first.
    fn install_named(&mut self, name: &str) -> Result<String> {
        let mut plan = solve::resolve(
            &[Dependency::any(name)],
            &self.installed,
            self.pins.as_ref(),
        )?;
        plan.compute_store_hashes()
            .with_context(|| format!("compute store hashes for `{name}`"))?;
        if self.dry_run {
            print_plan(&plan);
            return Ok(name.to_owned());
        }
        self.execute_plan(plan)?;
        Ok(name.to_owned())
    }

    /// Builds `package` (a rune path or known name) from source as the root, then resolves and
    /// installs its runtime dependencies through the solver.
    fn install_source_root(&mut self, package: &str) -> Result<String> {
        let rune = build::resolve_rune(package)?;
        if self.dry_run {
            self.dry_run_source_root(&rune)?;
            return Ok(package.to_owned());
        }
        let store_hash = crate::store::closure::store_hash_for_rune(&rune)
            .with_context(|| format!("compute store hash for source root `{package}`"))?;
        let installed = self.build_and_install(&rune, &store_hash)?;
        let name = installed.name.clone();
        let runtime = installed.runtime_deps.clone();
        self.record(installed);
        self.install_deps(&runtime)?;
        Ok(name)
    }

    /// Installs a local pre-built archive as the root, verifying it against `sha256` when given,
    /// then resolves and installs the runtime dependencies its embedded metadata declares.
    fn install_local_root(&mut self, package: &str, sha256: Option<String>) -> Result<String> {
        if self.dry_run {
            self.dry_run_local_root(package)?;
            return Ok(package.to_owned());
        }
        let installed = install_archive(&PathBuf::from(package), sha256, None)?;
        let name = installed.name.clone();
        let runtime = installed.runtime_deps.clone();
        self.record(installed);
        self.install_deps(&runtime)?;
        Ok(name)
    }

    /// Prints the plan for a source-rune root install: the rune itself, plus the solver plan
    /// for its build and runtime dependencies (everything that would land in the install root).
    fn dry_run_source_root(&self, rune: &Path) -> Result<()> {
        let metadata =
            build::read_rune_metadata(rune, build::tome_name_for_rune(rune)?.as_deref())?;
        println!(
            "plan:\n  + {} {} (source rune {})",
            metadata.name,
            metadata.version,
            rune.display()
        );
        let target = paths::target_triple();
        let mut combined = metadata.deps.build_for(&target);
        combined.extend(
            metadata
                .deps
                .runtime
                .iter()
                .filter(|d| d.matches_platform(&target))
                .cloned(),
        );
        if combined.is_empty() {
            return Ok(());
        }
        let plan = solve::resolve(&combined, &self.installed, self.pins.as_ref())?;
        print_plan_body(&plan);
        Ok(())
    }

    /// Prints the plan for a local-archive root install: the archive itself plus the solver
    /// plan for its embedded runtime dependencies.
    fn dry_run_local_root(&self, package: &str) -> Result<()> {
        let archive_path = PathBuf::from(package);
        let metadata = inspect_archive(&archive_path)?;
        println!(
            "plan:\n  + {} {} (local archive {})",
            metadata.name,
            metadata.version,
            archive_path.display()
        );
        let target = paths::target_triple();
        let runtime: Vec<Dependency> = metadata
            .deps
            .runtime
            .iter()
            .filter(|d| d.matches_platform(&target))
            .cloned()
            .collect();
        if runtime.is_empty() {
            return Ok(());
        }
        let plan = solve::resolve(&runtime, &self.installed, self.pins.as_ref())?;
        print_plan_body(&plan);
        Ok(())
    }

    /// Resolves `deps` into a plan and executes it. Already-installed satisfying packages are
    /// reused by the solver and produce no step.
    fn install_deps(&mut self, deps: &[Dependency]) -> Result<()> {
        if deps.is_empty() {
            return Ok(());
        }
        let mut plan = solve::resolve(deps, &self.installed, self.pins.as_ref())?;
        plan.compute_store_hashes()
            .with_context(|| "compute store hashes for build dependencies")?;
        self.execute_plan(plan)
    }

    fn execute_plan(&mut self, plan: Plan) -> Result<()> {
        for step in plan.steps {
            self.execute_step(step)?;
        }
        Ok(())
    }

    /// Realizes one planned step: fetch and verify a binary archive, or build a rune from source.
    /// Runtime dependencies are separate, earlier steps in the plan, so they are already
    /// installed by the time a step runs.
    fn execute_step(&mut self, step: PlanStep) -> Result<()> {
        let installed = self
            .realize_step(&step)
            .with_context(|| format!("install `{}` {}", step.name, step.version))?;
        self.record(installed);
        Ok(())
    }

    /// Realizes a resolved step by querying its prebuilt substitutes by store hash, falling back to
    /// a source build.
    ///
    /// The binhost is keyed by content address: when a source rune is available, the store hash is
    /// recomputed from it (with the resolved runtime dependency versions and the host toolchain) and
    /// a substitute is accepted only if its published `store_hash` matches — a mismatch means the
    /// prebuilt is stale (changed sources, flags, or dependency closure) and the package is built
    /// instead. A substitute that carries no `store_hash` is unverifiable and trusted as-is (a host
    /// with no compiler boundary cannot rebuild anyway, and legacy indexes predate the field).
    ///
    /// Under `--locked` the lockfile already pinned the exact archive — the solver filtered
    /// substitutes to it — so freshness is not re-litigated here.
    fn realize_step(&mut self, step: &PlanStep) -> Result<InstalledArchive> {
        if self.pins.is_some() {
            return match (step.substitutes.first(), &step.rune) {
                (Some(sub), _) => self.install_substitute(sub),
                (None, Some(rune)) => self.build_and_install(rune, require_store_hash(step)?),
                (None, None) => bail!("no pinned artifact available for `{}`", step.name),
            };
        }

        if let Some(hash) = &step.store_hash {
            if let Some(sub) = step.substitutes.iter().find(|s| s.store_hash == *hash) {
                return self.install_substitute(sub);
            }
        }

        match &step.rune {
            Some(rune) => {
                if !step.substitutes.is_empty() {
                    status(&format!(
                        "no prebuilt for `{}` {} matches local inputs; building from source",
                        step.name, step.version
                    ));
                }
                self.build_and_install(rune, require_store_hash(step)?)
            }
            None => bail!(
                "no installable prebuilt or source for `{}` {}",
                step.name,
                step.version
            ),
        }
    }

    /// Fetches, verifies, and installs a prebuilt substitute.
    fn install_substitute(&self, sub: &Substitute) -> Result<InstalledArchive> {
        let source_archive = sub.root.join(&sub.entry.archive);
        if let Some(tome) = tome::load_tomes()?
            .into_iter()
            .find(|t| t.name == sub.tome_name)
        {
            tome::verify_archive(&source_archive, &tome).with_context(|| {
                format!(
                    "verify archive signature for `{}` {}",
                    sub.entry.name, sub.entry.version
                )
            })?;
        }
        let archive = fetch::fetch_verified(
            &sub.entry.archive,
            &sub.root,
            &sub.entry.archive_hash,
            &paths::archive_cache_dir()?,
            &format!("archive `{}` {}", sub.entry.name, sub.entry.version),
        )?;
        install_archive(
            &archive,
            Some(sub.entry.archive_hash.clone()),
            Some(&sub.store_hash),
        )
    }

    /// The content address the package defined by `rune` would have if built here — computed over
    /// its dependency closure via [`crate::closure`], the same path the builder and the `store-hash`
    /// seam use, so the addresses agree by construction. Matched against a published `store_hash` to
    /// decide whether a prebuilt is a valid substitute.
    ///
    /// Returns `None` for a *compiled* package when this host has no toolchain boundary to reproduce
    /// the build environment: such a host cannot rebuild anyway and takes the published prebuilt as
    /// authoritative. A fixed-output package is always reproducible (its address ignores the
    /// toolchain), so it is always `Some`.
    /// Builds the rune at `rune` from source and installs the resulting archive. Build
    /// dependencies are resolved and installed first so they are present when the rune runs; the
    /// `building` guard rejects a build dependency that cycles back to the package being built.
    fn build_and_install(&mut self, rune: &Path, store_hash: &str) -> Result<InstalledArchive> {
        let metadata =
            build::read_rune_metadata(rune, build::tome_name_for_rune(rune)?.as_deref())?;
        validate_targets(&metadata, &paths::target_triple())
            .with_context(|| format!("validate target for `{}`", metadata.name))?;
        if !self.building.insert(metadata.name.clone()) {
            bail!("build dependency cycle involving `{}`", metadata.name);
        }
        let result = (|| {
            let expected_hash = match &self.pins {
                Some(pins) => Some(
                    pins.get(&metadata.name)
                        .with_context(|| {
                            format!(
                                "`{}` is required but is not recorded in the lockfile; cannot install --locked",
                                metadata.name
                            )
                        })?
                        .archive_hash
                        .clone(),
                ),
                None => None,
            };
            let build_deps = metadata.deps.build_for(&paths::target_triple());
            self.install_deps(&build_deps)
                .with_context(|| format!("install build dependencies for `{}`", metadata.name))?;
            let env = build::build_env_for_target(
                build_dep_bin_dirs(&build_deps)?,
                build_dep_env_vars(&build_deps)?,
                &paths::target_triple(),
            )?;
            let result = build::build_package_with_env(
                &rune.to_string_lossy(),
                &paths::build_output_dir()?,
                &env,
                store_hash,
            )?;
            install_archive(&result.archive, expected_hash, Some(&result.store_hash))
        })();
        self.building.remove(&metadata.name);
        result
    }

    fn record(&mut self, installed: InstalledArchive) {
        self.installed_now.push(installed.name.clone());
        if !installed.notes.is_empty() {
            self.notes.push((installed.name.clone(), installed.notes));
        }
        self.installed.insert(installed.name, installed.version);
    }

    /// Prints the collected post-install notes, one block per package. Called after the new
    /// generation is active so the notes are the last thing the user reads.
    fn report_notes(&self) {
        for (name, lines) in &self.notes {
            report(&format!("notes for {name}:"));
            for line in lines {
                report(&format!("  {line}"));
            }
        }
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
