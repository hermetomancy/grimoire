//! Installing, removing, and upgrading packages.
//!
//! [`install`] resolves a package and its dependencies through the solver, then realizes each
//! step — fetching and verifying a binary archive or building a rune from source — into the
//! install root. Every install stages into a transaction directory and promotes with atomic
//! renames, rolling back the active profile and state on failure (AGENTS.md §4). `--locked` constrains
//! resolution to the lockfile's recorded versions and hashes for a reproducible reinstall.

use anyhow::{Context, Result, anyhow, bail};
use semver::Version;
use std::{
    collections::{BTreeMap, BTreeSet, HashSet, VecDeque},
    fs::{self, File},
    io::Read,
    path::{Path, PathBuf},
};

use crate::{
    addendum, archive, build,
    cli::{InstallArgs, PackageArg},
    fetch, lock,
    model::{
        Dependency, PackageMetadata, PackageState, parse_version_relaxed,
        validate_relative_package_path, validate_sha256, validate_target, validate_targets,
    },
    nu::{
        nuon_io,
        runtime::{EmbeddedNuRuntime, RuneRuntime},
    },
    paths, profile,
    progress::{report, status, success},
    solve::{self, Plan, PlanStep, Substitute},
    tome,
};

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

    for package in &args.packages {
        if args.from_source || package.ends_with(".rn") {
            installer.install_source_root(package)?;
        } else if PathBuf::from(package).exists() || package.ends_with(".tar.zst") {
            installer.install_local_root(package, args.sha256.clone())?;
        } else {
            installer.install_named(package)?;
        }
    }

    if args.dry_run {
        return Ok(());
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

/// The installed package's name, concrete version, and the runtime dependencies declared in its
/// embedded metadata, returned so the caller can record it as installed and resolve those deps.
pub(crate) struct InstalledArchive {
    pub name: String,
    pub version: Version,
    pub runtime_deps: Vec<Dependency>,
}

/// Verifies, extracts, and promotes the resolved archive into the install root, then rebuilds
/// the lockfile. `expected_hash` is the hash the archive must match before it is read (from
/// `--sha256` or a tome index entry); `None` skips the check, which only happens for a local
/// archive installed without `--sha256`.
fn install_archive(
    archive_path: &Path,
    expected_hash: Option<String>,
    expected_store_hash: Option<&str>,
) -> Result<InstalledArchive> {
    let installed = install_store_only(archive_path, expected_hash, expected_store_hash)?;
    let mut tx = Transaction::new();
    rebuild_lock(&mut tx)?;
    tx.commit();
    Ok(installed)
}

/// Like [`install_archive`], but does **not** rebuild the lockfile or activate a generation.
/// Used by `grm tome build` to make built packages available as build dependencies for subsequent
/// builds without polluting the user's active profile.
pub(crate) fn install_store_only(
    archive_path: &Path,
    expected_hash: Option<String>,
    expected_store_hash: Option<&str>,
) -> Result<InstalledArchive> {
    if !archive_path.exists() {
        bail!(
            "package archive `{}` does not exist",
            archive_path.display()
        );
    }
    if let Some(expected) = &expected_hash {
        validate_sha256(expected, "expected archive hash")?;
    }

    // Verify integrity before the archive is read or extracted. A mismatch is fatal.
    status("hashing archive");
    let archive_hash = archive::archive_hash(archive_path)?;
    if let Some(expected) = &expected_hash {
        archive::verify_hash(&archive_hash, expected)
            .with_context(|| format!("verify archive {}", archive_path.display()))?;
        success("archive hash verified");
    }

    status(&format!(
        "validating archive paths ({})",
        archive_path.display()
    ));
    validate_archive_paths(archive_path)?;

    status("reading package metadata");
    let metadata = inspect_archive(archive_path)?;
    validate_target(&metadata, &paths::target_triple())?;

    let root = paths::install_root()?;
    let (package_dir, store_hash) = resolve_store_dir(&metadata, expected_store_hash)?;

    // Stage on the same filesystem as the store so `promote_package` can use an atomic rename.
    let store_root = paths::store_root()?;
    fs::create_dir_all(&store_root)?;
    let transaction = tempfile::Builder::new()
        .prefix("grimoire-")
        .tempdir_in(&store_root)?;
    let staging_dir = transaction.path().join("package");
    fs::create_dir_all(&staging_dir)?;

    status(&format!(
        "extracting into transaction ({})",
        transaction.path().display()
    ));
    extract_archive(archive_path, &staging_dir)?;

    status("validating extracted files");
    validate_bins(&metadata, &staging_dir)?;

    // Everything from here mutates shared install state. Stage each step against a
    // transaction so that a failure restores the previously installed version.
    let mut tx = Transaction::new();

    status(&format!("promoting package to ({})", package_dir.display()));
    let replaced = promote_package(&mut tx, &staging_dir, &package_dir)?;

    status("writing package state");
    write_state(
        &mut tx,
        &root,
        &metadata,
        &archive_hash,
        &store_hash,
        &package_dir.to_string_lossy(),
    )?;

    tx.commit();
    if let Some(replaced) = replaced {
        let _ = fs::remove_dir_all(replaced);
    }

    report(&format!(
        "installed {} {} into {}",
        metadata.name,
        metadata.version,
        root.display()
    ));
    let version = parse_version_relaxed(&metadata.version)
        .with_context(|| format!("package version `{}` is not valid semver", metadata.version))?;
    let target = paths::target_triple();
    let runtime_deps: Vec<Dependency> = metadata
        .deps
        .runtime
        .into_iter()
        .filter(|d| d.matches_platform(&target))
        .collect();
    Ok(InstalledArchive {
        name: metadata.name,
        version,
        runtime_deps,
    })
}

/// Ensures every build dependency in `deps` is installed store-only (no lockfile, no generation).
/// Missing deps are resolved through the solver and installed from substitutes or built from source.
/// Already-installed packages are reused.
pub(crate) fn ensure_build_deps_installed(deps: &[Dependency]) -> Result<()> {
    if deps.is_empty() {
        return Ok(());
    }

    let mut installed = installed_versions()?;
    let missing: Vec<Dependency> = deps
        .iter()
        .filter(|dep| find_dep_state(&installed_states().unwrap_or_default(), &dep.name).is_none())
        .cloned()
        .collect();

    if missing.is_empty() {
        return Ok(());
    }

    let mut plan = solve::resolve(&missing, &installed, None)?;
    plan.compute_store_hashes()
        .with_context(|| "compute store hashes for build dependencies")?;

    for step in plan.steps {
        if installed.contains_key(&step.name) {
            continue;
        }

        let result = if let Some(sub) = step.substitutes.first() {
            let archive = fetch::fetch_verified(
                &sub.entry.archive,
                &sub.root,
                &sub.entry.archive_hash,
                &paths::archive_cache_dir()?,
                &format!("archive `{}` {}", sub.entry.name, sub.entry.version),
            )?;
            install_store_only(
                &archive,
                Some(sub.entry.archive_hash.clone()),
                Some(&sub.store_hash),
            )
        } else if let Some(rune) = &step.rune {
            let store_hash = crate::closure::store_hash_for_rune(rune)
                .with_context(|| format!("cannot compute store hash for `{}`", step.name))?;
            let mut metadata = EmbeddedNuRuntime
                .package_metadata(rune)
                .with_context(|| format!("read rune metadata {}", rune.display()))?;
            addendum::patched_package_metadata(
                &mut metadata,
                build::tome_name_for_rune(rune)?.as_deref(),
                rune,
            )
            .with_context(|| format!("apply addendums to {}", rune.display()))?;
            let build_deps = metadata.deps.build_for(&paths::target_triple());
            ensure_build_deps_installed(&build_deps)
                .with_context(|| format!("install build dependencies for `{}`", step.name))?;
            let env = build::build_env_for_target(
                build_dep_bin_dirs(&build_deps)?,
                build_dep_env_vars(&build_deps)?,
                &paths::target_triple(),
            )?;
            let result = build::build_package_with_env(
                &rune.to_string_lossy(),
                &paths::build_output_dir()?,
                &env,
                &store_hash,
            )?;
            install_store_only(&result.archive, None, Some(&result.store_hash))
        } else {
            bail!(
                "no installable prebuilt or source for `{}` {}",
                step.name,
                step.version
            )
        };

        let installed_archive = result
            .with_context(|| format!("store-only install `{}` {}", step.name, step.version))?;
        installed.insert(installed_archive.name, installed_archive.version);
    }

    Ok(())
}

impl Installer {
    /// Installs `name` and its transitive runtime dependencies. The solver picks a concrete
    /// version for every package in the graph and orders the plan so dependencies install first.
    fn install_named(&mut self, name: &str) -> Result<()> {
        let mut plan = solve::resolve(
            &[Dependency::any(name)],
            &self.installed,
            self.pins.as_ref(),
        )?;
        plan.compute_store_hashes()
            .with_context(|| format!("compute store hashes for `{name}`"))?;
        if self.dry_run {
            print_plan(&plan);
            return Ok(());
        }
        self.execute_plan(plan)
    }

    /// Builds `package` (a rune path or known name) from source as the root, then resolves and
    /// installs its runtime dependencies through the solver.
    fn install_source_root(&mut self, package: &str) -> Result<()> {
        let rune = build::resolve_rune(package)?;
        if self.dry_run {
            return self.dry_run_source_root(&rune);
        }
        let store_hash = crate::closure::store_hash_for_rune(&rune)
            .with_context(|| format!("compute store hash for source root `{package}`"))?;
        let installed = self.build_and_install(&rune, &store_hash)?;
        let runtime = installed.runtime_deps.clone();
        self.record(installed);
        self.install_deps(&runtime)
    }

    /// Installs a local pre-built archive as the root, verifying it against `sha256` when given,
    /// then resolves and installs the runtime dependencies its embedded metadata declares.
    fn install_local_root(&mut self, package: &str, sha256: Option<String>) -> Result<()> {
        if self.dry_run {
            return self.dry_run_local_root(package);
        }
        let installed = install_archive(&PathBuf::from(package), sha256, None)?;
        let runtime = installed.runtime_deps.clone();
        self.record(installed);
        self.install_deps(&runtime)
    }

    /// Prints the plan for a source-rune root install: the rune itself, plus the solver plan
    /// for its build and runtime dependencies (everything that would land in the install root).
    fn dry_run_source_root(&self, rune: &Path) -> Result<()> {
        let mut metadata = EmbeddedNuRuntime
            .package_metadata(rune)
            .with_context(|| format!("read rune metadata {}", rune.display()))?;
        addendum::patched_package_metadata(
            &mut metadata,
            build::tome_name_for_rune(rune)?.as_deref(),
            rune,
        )
        .with_context(|| format!("apply addendums to {}", rune.display()))?;
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
                (None, Some(rune)) => {
                    let hash = step.store_hash.as_deref().with_context(|| {
                        format!("cannot compute store hash for `{}`", step.name)
                    })?;
                    self.build_and_install(rune, hash)
                }
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
                let hash = step
                    .store_hash
                    .as_deref()
                    .with_context(|| format!("cannot compute store hash for `{}`", step.name))?;
                self.build_and_install(rune, hash)
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
        tome::verify_rune(rune)
            .with_context(|| format!("verify rune signature {}", rune.display()))?;
        let mut metadata = EmbeddedNuRuntime
            .package_metadata(rune)
            .with_context(|| format!("read rune metadata {}", rune.display()))?;
        addendum::patched_package_metadata(
            &mut metadata,
            build::tome_name_for_rune(rune)?.as_deref(),
            rune,
        )
        .with_context(|| format!("apply addendums to {}", rune.display()))?;
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
        self.installed.insert(installed.name, installed.version);
    }
}

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

fn set_hold(name: &str, held: bool) -> Result<()> {
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

/// Reinstalls each package in `names` at the newest available version, for `upgrade`. The named
/// packages are dropped from the known-installed set so the solver re-resolves them to the newest
/// candidate instead of reusing the currently installed (older) version; every other installed
/// package is still reused to satisfy dependencies.
pub fn upgrade_packages(names: &[String]) -> Result<()> {
    if let Some(msg) = paths::fixed_store_setup_instructions() {
        bail!("{msg}");
    }
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
    installer.finalize()?;
    Ok(())
}

/// Installed package names mapped to their concrete versions, for the solver. Recorded state
/// versions were validated as semver when written, so an unparsable one is skipped defensively.
fn installed_versions() -> Result<BTreeMap<String, Version>> {
    let mut versions = BTreeMap::new();
    for state in installed_states()? {
        if let Ok(version) = parse_version_relaxed(&state.version) {
            versions.insert(state.name, version);
        }
    }
    Ok(versions)
}

/// Finds an installed package that satisfies the dependency `name`.
/// First tries an exact package name match, then falls back to capability resolution
/// (any installed package whose `bins` map contains `name` as a key).
pub(crate) fn find_dep_state<'a>(
    states: &'a [PackageState],
    name: &str,
) -> Option<&'a PackageState> {
    states
        .iter()
        .find(|state| state.name == name)
        .or_else(|| states.iter().find(|state| state.bins.contains_key(name)))
}

pub(crate) fn build_dep_bin_dirs(deps: &[Dependency]) -> Result<Vec<PathBuf>> {
    let states = installed_states()?;
    let mut dirs = Vec::new();
    for dep in deps {
        let Some(state) = find_dep_state(&states, &dep.name) else {
            continue;
        };
        for path in state.bins.values() {
            let bin = PathBuf::from(&state.store_path).join(path);
            let Some(parent) = bin.parent() else {
                continue;
            };
            let dir = parent.to_path_buf();
            if !dirs.contains(&dir) {
                dirs.push(dir);
            }
        }
    }
    Ok(dirs)
}

/// Computes additional environment variables (PKG_CONFIG_PATH, CPATH, LIBRARY_PATH, and
/// `<DEP>_PREFIX` for each build dep) from the installed build dependencies so that compilers
/// and pkg-config can find headers and libraries.
pub(crate) fn build_dep_env_vars(deps: &[Dependency]) -> Result<Vec<(String, String)>> {
    let states = installed_states()?;
    let mut pkg_config_paths = Vec::new();
    let mut cpaths = Vec::new();
    let mut library_paths = Vec::new();
    let mut prefix_vars = Vec::new();

    for dep in deps {
        let Some(state) = find_dep_state(&states, &dep.name) else {
            continue;
        };
        let store = PathBuf::from(&state.store_path);
        let pkgconfig = store.join("lib/pkgconfig");
        if pkgconfig.is_dir() && !pkg_config_paths.contains(&pkgconfig) {
            pkg_config_paths.push(pkgconfig);
        }
        let include = store.join("include");
        if include.is_dir() && !cpaths.contains(&include) {
            cpaths.push(include);
        }
        let lib = store.join("lib");
        if lib.is_dir() && !library_paths.contains(&lib) {
            library_paths.push(lib);
        }
        let env_name = format!("{}_PREFIX", dep.name.to_ascii_uppercase().replace('-', "_"));
        prefix_vars.push((env_name, state.store_path.clone()));
    }

    let mut env = Vec::new();
    if !pkg_config_paths.is_empty() {
        env.push((
            "PKG_CONFIG_PATH".to_string(),
            std::env::join_paths(&pkg_config_paths)
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_default(),
        ));
    }
    if !cpaths.is_empty() {
        env.push((
            "CPATH".to_string(),
            std::env::join_paths(&cpaths)
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_default(),
        ));
    }
    if !library_paths.is_empty() {
        env.push((
            "LIBRARY_PATH".to_string(),
            std::env::join_paths(&library_paths)
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_default(),
        ));
    }
    env.extend(prefix_vars);
    Ok(env)
}

pub fn remove(args: PackageArg) -> Result<()> {
    if let Some(msg) = paths::fixed_store_setup_instructions() {
        bail!("{msg}");
    }
    if args.packages.is_empty() {
        bail!("specify at least one package to remove");
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
fn remove_one(name: &str) -> Result<PackageState> {
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
    let package_dir = PathBuf::from(&state.store_path);
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

/// Removes runtime dependencies left orphaned by a previous removal — packages no other
/// installed package still lists in its `runtime_deps`. Cascades transitively: a dep that
/// becomes orphaned mid-pass is itself a candidate. Build dependencies are not considered;
/// once a package is installed they are no longer load-bearing for it.
fn autoremove_orphans(initial: Vec<String>) -> Result<()> {
    let mut queue: VecDeque<String> = initial.into();
    let mut seen: HashSet<String> = HashSet::new();
    while let Some(name) = queue.pop_front() {
        if !seen.insert(name.clone()) {
            continue;
        }
        let states = installed_states()?;
        if !states.iter().any(|state| state.name == name) {
            continue;
        }
        let still_needed = states
            .iter()
            .any(|other| other.name != name && other.runtime_deps.iter().any(|dep| dep == &name));
        if still_needed {
            continue;
        }
        let removed =
            remove_one(&name).with_context(|| format!("autoremove unused dependency `{name}`"))?;
        report(&format!("autoremoved unused dependency {name}"));
        for dep in removed.runtime_deps {
            queue.push_back(dep);
        }
    }
    Ok(())
}

fn inspect_archive(path: &Path) -> Result<PackageMetadata> {
    let file = File::open(path)?;
    let decoder = zstd::stream::read::Decoder::new(file)?;
    let mut archive = tar::Archive::new(decoder);

    for entry in archive.entries()? {
        let mut entry = entry?;
        if normalize_archive_path(&entry.path()?) == ".grimoire/package.nuon" {
            let mut text = String::new();
            entry.read_to_string(&mut text)?;
            return PackageMetadata::from_value(nuon_io::parse_nuon(&text)?, true);
        }
    }

    bail!("package archive is missing .grimoire/package.nuon");
}

/// Resolves the absolute store directory an archive installs into and its store hash.
///
/// Every archive records its content-addressed store basename (`<hash>-<name>-<version>`) in its
/// embedded `store_path`; the absolute location is `store_root()/<basename>`, derived locally so the
/// same archive lands in the same place on every host regardless of who built it. The hash is the
/// basename's leading component. When the caller knows the expected store hash — a tome index entry,
/// or a fresh source build — it is cross-checked so a tampered or mislabeled archive is refused.
fn resolve_store_dir(
    metadata: &PackageMetadata,
    expected_hash: Option<&str>,
) -> Result<(PathBuf, String)> {
    let Some(basename) = metadata.store_path.as_deref() else {
        bail!(
            "package `{}` metadata is missing its store_path basename",
            metadata.name
        );
    };
    validate_relative_package_path(basename, "metadata store_path")?;
    let suffix = format!("-{}-{}", metadata.name, metadata.version);
    let Some(hash) = basename.strip_suffix(&suffix) else {
        bail!(
            "package `{}` metadata store_path `{basename}` is not `<hash>-{}-{}`",
            metadata.name,
            metadata.name,
            metadata.version
        );
    };
    if hash.is_empty() || hash.contains('/') {
        bail!(
            "package `{}` metadata store_path `{basename}` has an invalid hash component",
            metadata.name
        );
    }
    if let Some(expected) = expected_hash {
        if hash != expected {
            bail!(
                "package `{}` embeds store hash `{hash}` but its inputs hash to `{expected}`",
                metadata.name
            );
        }
    }
    Ok((paths::store_root()?.join(basename), hash.to_string()))
}

/// Validates every archive member *before* extraction (AGENTS.md §5.2–§5.3): member paths must
/// stay inside the extraction root; symlinks are allowed only when their target also resolves
/// within the package (so a link can never point outside the install prefix), and no member may
/// be nested *under* a symlink (which would let extraction write through the link). Hard links
/// are still rejected outright. With these guarantees the subsequent `unpack` into a fresh
/// staging directory cannot be lured outside the destination.
fn validate_archive_paths(path: &Path) -> Result<()> {
    let file = File::open(path)?;
    let decoder = zstd::stream::read::Decoder::new(file)?;
    let mut archive = tar::Archive::new(decoder);
    let mut bad = Vec::new();
    let mut members: Vec<PathBuf> = Vec::new();
    let mut symlinks: BTreeSet<PathBuf> = BTreeSet::new();

    for entry in archive.entries()? {
        let entry = entry?;
        let member_path = entry.path()?.into_owned();
        let member = member_path.display().to_string();
        if !archive::validate_archive_member_path(&member_path) {
            bad.push(member);
            continue;
        }

        let entry_type = entry.header().entry_type();
        if entry_type.is_hard_link() {
            bail!("archive contains a hard link, which is not accepted yet: {member}");
        }
        if entry_type.is_symlink() {
            let target = entry
                .link_name()?
                .ok_or_else(|| anyhow!("archive symlink `{member}` is missing a target"))?;
            if !archive::validate_symlink_target(&member_path, &target) {
                bail!(
                    "archive symlink `{member}` has a target that escapes the package: {}",
                    target.display()
                );
            }
            symlinks.insert(member_path.clone());
        }
        members.push(member_path);
    }

    if !bad.is_empty() {
        bail!("archive contains unsafe paths: {}", bad.join(", "));
    }

    // A member nested under a symlink would be extracted *through* that link; reject it so the
    // validated targets are the only paths `unpack` can ever follow.
    if !symlinks.is_empty() {
        for member in &members {
            if let Some(ancestor) = member
                .ancestors()
                .skip(1)
                .find(|ancestor| symlinks.contains(*ancestor))
            {
                bail!(
                    "archive member `{}` is nested under symlink `{}`",
                    member.display(),
                    ancestor.display()
                );
            }
        }
    }

    Ok(())
}

fn extract_archive(path: &Path, destination: &Path) -> Result<()> {
    let file = File::open(path)?;
    let decoder = zstd::stream::read::Decoder::new(file)?;
    let mut archive = tar::Archive::new(decoder);
    archive.unpack(destination)?;
    Ok(())
}

fn validate_bins(metadata: &PackageMetadata, package_dir: &Path) -> Result<()> {
    for (name, path) in &metadata.bins {
        validate_relative_package_path(path, &format!("bin `{name}`"))?;
        let bin_path = package_dir.join(path);
        if !bin_path.exists() {
            bail!("declared bin `{name}` points to missing file `{path}`");
        }
        make_executable(&bin_path)?;
    }
    Ok(())
}

/// Moves the staged package into its final location. If a previous version exists it is
/// renamed aside first; the backup path is returned so the caller can drop it once the
/// whole install commits. A registered rollback restores the previous version on failure.
fn promote_package(
    tx: &mut Transaction,
    staging_dir: &Path,
    package_dir: &Path,
) -> Result<Option<PathBuf>> {
    let parent = package_dir
        .parent()
        .context("package directory should have parent")?;
    fs::create_dir_all(parent)?;

    let backup = backup_path(package_dir)?;
    let had_previous = package_dir.exists();
    if had_previous {
        if backup.exists() {
            fs::remove_dir_all(&backup)?;
        }
        fs::rename(package_dir, &backup)
            .with_context(|| format!("back up existing package {}", package_dir.display()))?;
    }

    // Register the restore before promoting so a failed rename also rolls back.
    {
        let package_dir = package_dir.to_path_buf();
        let backup = backup.clone();
        tx.on_rollback(move || {
            let _ = fs::remove_dir_all(&package_dir);
            if had_previous {
                let _ = fs::rename(&backup, &package_dir);
            }
        });
    }

    fs::rename(staging_dir, package_dir)
        .with_context(|| format!("promote package to {}", package_dir.display()))?;

    Ok(had_previous.then_some(backup))
}

fn backup_path(package_dir: &Path) -> Result<PathBuf> {
    let name = package_dir
        .file_name()
        .and_then(|name| name.to_str())
        .context("package directory should have a name")?;
    Ok(package_dir.with_file_name(format!("{name}.grimoire-old")))
}

fn write_state(
    tx: &mut Transaction,
    root: &Path,
    metadata: &PackageMetadata,
    archive_hash: &str,
    store_hash: &str,
    store_path: &str,
) -> Result<()> {
    let state_dir = root.join("state").join("packages");
    fs::create_dir_all(&state_dir)?;
    let state_path = state_dir.join(format!("{}.nuon", metadata.name));

    // A hold is user intent that survives reinstalls and upgrades — preserve it when we
    // rewrite the state file. (Nothing else in the prior state is worth carrying forward;
    // the install we just performed is the authoritative source for everything else.)
    let previous_held = if state_path.exists() {
        PackageState::from_value(nuon_io::read_nuon(&state_path)?)
            .map(|prior| prior.held)
            .unwrap_or(false)
    } else {
        false
    };

    let state = PackageState {
        name: metadata.name.clone(),
        version: metadata.version.clone(),
        target: metadata.target.clone(),
        archive_hash: archive_hash.to_owned(),
        store_hash: store_hash.to_owned(),
        store_path: store_path.to_owned(),
        bins: metadata.bins.clone(),
        runtime_deps: metadata
            .deps
            .runtime
            .iter()
            .filter(|d| d.matches_platform(&paths::target_triple()))
            .map(|dep| dep.name.clone())
            .collect(),
        build_deps: metadata
            .deps
            .build_for(&paths::target_triple())
            .iter()
            .map(|dep| dep.name.clone())
            .collect(),
        source_hashes: metadata
            .sources
            .iter()
            .map(|(name, source)| (name.clone(), source.sha256.clone()))
            .collect(),
        held: previous_held,
    };

    // Capture the prior state so a later failure can restore it.
    let previous = if state_path.exists() {
        Some(fs::read(&state_path)?)
    } else {
        None
    };
    {
        let state_path = state_path.clone();
        tx.on_rollback(move || match &previous {
            Some(bytes) => {
                let _ = fs::write(&state_path, bytes);
            }
            None => {
                let _ = fs::remove_file(&state_path);
            }
        });
    }

    nuon_io::write_nuon(&state_path, &state.to_value())
}

fn rebuild_lock(tx: &mut Transaction) -> Result<()> {
    let lock_path = lock::lock_path()?;
    let previous = if lock_path.exists() {
        Some(fs::read(&lock_path)?)
    } else {
        None
    };
    {
        let lock_path = lock_path.clone();
        tx.on_rollback(move || match &previous {
            Some(bytes) => {
                let _ = fs::write(&lock_path, bytes);
            }
            None => {
                let _ = fs::remove_file(&lock_path);
            }
        });
    }
    lock::rebuild()
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

/// Best-effort, RAII-style rollback for a multi-step install. Rollback actions run in
/// reverse registration order when the transaction is dropped without [`commit`](Transaction::commit)ting,
/// e.g. when an install step returns an error via `?`.
#[derive(Default)]
struct Transaction {
    rollbacks: Vec<Box<dyn FnOnce()>>,
    committed: bool,
}

impl Transaction {
    fn new() -> Self {
        Self::default()
    }

    fn on_rollback(&mut self, action: impl FnOnce() + 'static) {
        self.rollbacks.push(Box::new(action));
    }

    fn commit(mut self) {
        self.committed = true;
    }
}

impl Drop for Transaction {
    fn drop(&mut self) {
        if self.committed {
            return;
        }
        while let Some(rollback) = self.rollbacks.pop() {
            rollback();
        }
    }
}

fn normalize_archive_path(path: &Path) -> String {
    let text = path.to_string_lossy();
    text.strip_prefix("./").unwrap_or(&text).to_owned()
}

fn make_executable(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let mut permissions = fs::metadata(path)?.permissions();
    permissions.set_mode(permissions.mode() | 0o111);
    fs::set_permissions(path, permissions)?;
    Ok(())
}

#[cfg(test)]
mod tests {}
