//! Installing, removing, and upgrading packages.
//!
//! [`install`] resolves a package and its dependencies through the solver, then realizes each
//! step — fetching and verifying a binary archive or building a rune from source — into the
//! install root. Every install stages into a transaction directory and promotes with atomic
//! renames, rolling back shims and state on failure (AGENTS.md §4). `--locked` constrains
//! resolution to the lockfile's recorded versions and hashes for a reproducible reinstall.

use anyhow::{Context, Result, bail};
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
        Dependency, PackageMetadata, PackageState, validate_relative_package_path, validate_sha256,
        validate_target,
    },
    nu::{
        nuon_io,
        runtime::{BuildEnv, EmbeddedNuRuntime, RuneRuntime},
    },
    paths,
    progress::{report, status, success},
    solve::{self, Origin, Plan, PlanStep},
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
    /// Names that were already installed when this run started. Used to tell a build dep that
    /// just came in for this run apart from one the user (or a previous run) had installed
    /// independently; only the former is a candidate for post-build cleanup.
    initial_installed: BTreeSet<String>,
    /// Build dependencies installed during this run *for* a source build, that were not
    /// already installed when the run started. Removed at the end of a successful install —
    /// once the build is over they are no longer load-bearing. The downloaded archive stays in
    /// `cache/archives/`, so a future install that needs them is a cheap re-extract.
    build_staged: BTreeSet<String>,
    /// When true, every install path stops after planning and prints the plan to stdout —
    /// no fetches, no builds, no state writes. Wired from `--dry-run` / `--explain`.
    dry_run: bool,
}

impl Installer {
    /// Snapshot the installed-name set up front so post-build cleanup can distinguish "this
    /// run pulled in `make`" from "the user already had `make`".
    fn new(installed: BTreeMap<String, Version>, pins: Option<solve::Pins>) -> Self {
        let initial_installed = installed.keys().cloned().collect();
        Self {
            installed,
            pins,
            building: HashSet::new(),
            installed_now: Vec::new(),
            initial_installed,
            build_staged: BTreeSet::new(),
            dry_run: false,
        }
    }

    fn with_dry_run(mut self, dry_run: bool) -> Self {
        self.dry_run = dry_run;
        self
    }
}

pub fn install(args: InstallArgs) -> Result<()> {
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

    if args.from_source || args.package.ends_with(".rn") {
        installer.install_source_root(&args.package)?;
    } else if PathBuf::from(&args.package).exists() || args.package.ends_with(".tar.zst") {
        installer.install_local_root(&args.package, args.sha256.clone())?;
    } else {
        installer.install_named(&args.package)?;
    }

    if args.dry_run {
        return Ok(());
    }

    installer.cleanup_build_staged()?;

    // A resolve that reuses an already-satisfying install produces no steps, so nothing above
    // reported anything. Tell the user the request was a no-op rather than printing silence.
    if installer.installed_now.is_empty() {
        report(&format!(
            "{} is already installed and up to date",
            args.package
        ));
    }
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
struct InstalledArchive {
    name: String,
    version: Version,
    runtime_deps: Vec<Dependency>,
}

/// Verifies, extracts, and promotes the resolved archive into the install root. `expected_hash`
/// is the hash the archive must match before it is read (from `--sha256` or a tome index entry);
/// `None` skips the check, which only happens for a local archive installed without `--sha256`.
fn install_archive(archive_path: &Path, expected_hash: Option<String>) -> Result<InstalledArchive> {
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

    let transactions_dir = root.join("transactions");
    fs::create_dir_all(&transactions_dir)?;
    let transaction = tempfile::Builder::new()
        .prefix("grimoire-")
        .tempdir_in(&transactions_dir)?;
    let staging_dir = transaction.path().join("package");
    fs::create_dir_all(&staging_dir)?;

    status(&format!(
        "extracting into transaction ({})",
        transaction.path().display()
    ));
    extract_archive(archive_path, &staging_dir)?;

    status("validating extracted files");
    validate_bins(&metadata, &staging_dir)?;

    let package_dir = root
        .join("packages")
        .join(&metadata.name)
        .join(&metadata.version);

    // Everything from here mutates shared install state. Stage each step against a
    // transaction so that a failure restores the previously installed version.
    let mut tx = Transaction::new();

    status(&format!("promoting package to ({})", package_dir.display()));
    let replaced = promote_package(&mut tx, &staging_dir, &package_dir)?;

    status("linking shims");
    link_bins(&mut tx, &metadata, &root, &package_dir)?;

    status("writing package state");
    write_state(&mut tx, &root, &metadata, &archive_hash)?;

    rebuild_lock(&mut tx)?;

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
    let version = Version::parse(&metadata.version)
        .with_context(|| format!("package version `{}` is not valid semver", metadata.version))?;
    Ok(InstalledArchive {
        name: metadata.name,
        version,
        runtime_deps: metadata.deps.runtime,
    })
}

impl Installer {
    /// Installs `name` and its transitive runtime dependencies. The solver picks a concrete
    /// version for every package in the graph and orders the plan so dependencies install first.
    fn install_named(&mut self, name: &str) -> Result<()> {
        let plan = solve::resolve(
            &[Dependency::any(name)],
            &self.installed,
            self.pins.as_ref(),
        )?;
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
        let installed = self.build_and_install(&rune)?;
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
        let installed = install_archive(&PathBuf::from(package), sha256)?;
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
        let mut combined = metadata.deps.build_for(&paths::target_triple());
        combined.extend(metadata.deps.runtime.clone());
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
        if metadata.deps.runtime.is_empty() {
            return Ok(());
        }
        let plan = solve::resolve(&metadata.deps.runtime, &self.installed, self.pins.as_ref())?;
        print_plan_body(&plan);
        Ok(())
    }

    /// Resolves `deps` into a plan and executes it. Already-installed satisfying packages are
    /// reused by the solver and produce no step.
    fn install_deps(&mut self, deps: &[Dependency]) -> Result<()> {
        if deps.is_empty() {
            return Ok(());
        }
        let plan = solve::resolve(deps, &self.installed, self.pins.as_ref())?;
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
        let installed = match step.origin {
            Origin::Binary { root, entry } => {
                let archive = fetch::fetch_verified(
                    &entry.archive,
                    &root,
                    &entry.archive_hash,
                    &paths::archive_cache_dir()?,
                    &format!("archive `{}` {}", entry.name, entry.version),
                )?;
                install_archive(&archive, Some(entry.archive_hash))
                    .with_context(|| format!("install `{}` {}", step.name, step.version))?
            }
            Origin::Source { rune } => self
                .build_and_install(&rune)
                .with_context(|| format!("install `{}` {} from source", step.name, step.version))?,
        };
        self.record(installed);
        Ok(())
    }

    /// Builds the rune at `rune` from source and installs the resulting archive. Build
    /// dependencies are resolved and installed first so they are present when the rune runs; the
    /// `building` guard rejects a build dependency that cycles back to the package being built.
    fn build_and_install(&mut self, rune: &Path) -> Result<InstalledArchive> {
        let mut metadata = EmbeddedNuRuntime
            .package_metadata(rune)
            .with_context(|| format!("read rune metadata {}", rune.display()))?;
        addendum::patched_package_metadata(
            &mut metadata,
            build::tome_name_for_rune(rune)?.as_deref(),
            rune,
        )
        .with_context(|| format!("apply addendums to {}", rune.display()))?;
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
            // Whatever wasn't already installed coming into this run is being pulled in
            // *for* this build, so mark it for post-build cleanup. Names already in
            // `initial_installed` belong to the user (or a previous run) and stay.
            let staged: Vec<String> = build_deps
                .iter()
                .map(|dep| dep.name.clone())
                .filter(|name| !self.initial_installed.contains(name))
                .collect();
            self.install_deps(&build_deps)
                .with_context(|| format!("install build dependencies for `{}`", metadata.name))?;
            for name in staged {
                self.build_staged.insert(name);
            }
            let env = BuildEnv {
                path_dirs: build_dep_bin_dirs(&build_deps)?,
            };
            let archive = build::build_package_with_env(
                &rune.to_string_lossy(),
                &paths::build_output_dir()?,
                &env,
            )?;
            install_archive(&archive, expected_hash)
        })();
        self.building.remove(&metadata.name);
        result
    }

    fn record(&mut self, installed: InstalledArchive) {
        self.installed_now.push(installed.name.clone());
        self.installed.insert(installed.name, installed.version);
    }

    /// Removes build dependencies pulled in solely for this run's source builds, now that the
    /// builds have finished. A staged dep is kept if any installed package's `runtime_deps`
    /// references it — the same gating used by `autoremove_orphans` — or if a *later* package
    /// in this run picked it up as a runtime dep. Each removal cascades its own runtime-dep
    /// orphans, so transitive runtime deps of a build dep get reclaimed too.
    fn cleanup_build_staged(&mut self) -> Result<()> {
        let staged = std::mem::take(&mut self.build_staged);
        for name in &staged {
            let states = installed_states()?;
            if !states.iter().any(|state| state.name == *name) {
                continue;
            }
            let still_needed = states.iter().any(|other| {
                other.name != *name && other.runtime_deps.iter().any(|dep| dep == name)
            });
            if still_needed {
                continue;
            }
            let removed =
                remove_one(name).with_context(|| format!("cleanup build dependency `{name}`"))?;
            report(&format!("removed build dependency {name}"));
            self.installed.remove(name);
            autoremove_orphans(removed.runtime_deps)?;
        }
        Ok(())
    }
}

pub fn list() -> Result<()> {
    for state in installed_states()? {
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
    set_hold(&args.package, true)
}

pub fn unhold(args: PackageArg) -> Result<()> {
    set_hold(&args.package, false)
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
    installer.cleanup_build_staged()?;
    Ok(())
}

/// Installed package names mapped to their concrete versions, for the solver. Recorded state
/// versions were validated as semver when written, so an unparsable one is skipped defensively.
fn installed_versions() -> Result<BTreeMap<String, Version>> {
    let mut versions = BTreeMap::new();
    for state in installed_states()? {
        if let Ok(version) = Version::parse(&state.version) {
            versions.insert(state.name, version);
        }
    }
    Ok(versions)
}

fn build_dep_bin_dirs(deps: &[Dependency]) -> Result<Vec<PathBuf>> {
    let root = paths::install_root()?;
    let states = installed_states()?;
    let mut dirs = Vec::new();
    for dep in deps {
        let Some(state) = states.iter().find(|state| state.name == dep.name) else {
            continue;
        };
        for path in state.bins.values() {
            let bin = root
                .join("packages")
                .join(&state.name)
                .join(&state.version)
                .join(path);
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

pub fn remove(args: PackageArg) -> Result<()> {
    let removed = remove_one(&args.package)?;
    report(&format!("removed {}", args.package));
    autoremove_orphans(removed.runtime_deps)
}

/// Removes one installed package and returns its prior state record. Each call is a complete
/// transaction (shims, package directory, state file, lockfile) — callers chaining multiple
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
    // a transaction: a failure partway through restores the shims, package files, and state
    // record rather than leaving the package half-removed.
    let mut tx = Transaction::new();
    let bin_dir = root.join("bin");
    for bin in state.bins.keys() {
        let shim = shim_path(&bin_dir, bin);
        let previous = capture_shim(&shim)?;
        {
            let shim = shim.clone();
            tx.on_rollback(move || restore_shim(&shim, previous.as_ref()));
        }
        remove_if_exists(&shim)?;
    }

    // Move the package dir aside rather than deleting outright, so a later failure can restore
    // it; the backup is dropped only once the whole removal commits.
    let package_dir = root.join("packages").join(&state.name).join(&state.version);
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

/// Validates every archive member *before* extraction (AGENTS.md §5.2): member paths must
/// stay inside the extraction root, and symlinks/hard links are rejected outright (§5.3) so a
/// malicious link can never be followed during `unpack`.
fn validate_archive_paths(path: &Path) -> Result<()> {
    let file = File::open(path)?;
    let decoder = zstd::stream::read::Decoder::new(file)?;
    let mut archive = tar::Archive::new(decoder);
    let mut bad = Vec::new();

    for entry in archive.entries()? {
        let entry = entry?;
        let member = entry.path()?.display().to_string();
        if !archive::validate_archive_member_path(&entry.path()?) {
            bad.push(member);
            continue;
        }

        let entry_type = entry.header().entry_type();
        if entry_type.is_symlink() {
            bail!("archive contains a symlink, which is not accepted yet: {member}");
        }
        if entry_type.is_hard_link() {
            bail!("archive contains a hard link, which is not accepted yet: {member}");
        }
    }

    if !bad.is_empty() {
        bail!("archive contains unsafe paths: {}", bad.join(", "));
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

fn link_bins(
    tx: &mut Transaction,
    metadata: &PackageMetadata,
    root: &Path,
    package_dir: &Path,
) -> Result<()> {
    let bin_dir = root.join("bin");
    fs::create_dir_all(&bin_dir)?;

    for (name, path) in &metadata.bins {
        let source = package_dir.join(path).canonicalize()?;
        let shim = shim_path(&bin_dir, name);

        // Capture and register a restore of the prior shim before replacing it.
        let previous = capture_shim(&shim)?;
        {
            let shim = shim.clone();
            tx.on_rollback(move || restore_shim(&shim, previous.as_ref()));
        }

        remove_if_exists(&shim)?;
        write_shim(&shim, &source)?;
    }

    Ok(())
}

#[cfg(unix)]
fn shim_path(bin_dir: &Path, name: &str) -> PathBuf {
    bin_dir.join(name)
}

#[cfg(windows)]
fn shim_path(bin_dir: &Path, name: &str) -> PathBuf {
    bin_dir.join(format!("{name}.cmd"))
}

#[cfg(unix)]
fn write_shim(shim: &Path, source: &Path) -> Result<()> {
    std::os::unix::fs::symlink(source, shim)
        .with_context(|| format!("link shim {}", shim.display()))
}

#[cfg(windows)]
fn write_shim(shim: &Path, source: &Path) -> Result<()> {
    fs::write(shim, windows_shim_contents(source))
        .with_context(|| format!("write shim {}", shim.display()))
}

/// The body of a Windows `.cmd` shim that forwards all arguments to the real executable.
/// Factored out (and compiled on Windows or under test) so its exact contents can be unit-tested
/// on any host: a batch shim must silence echo, quote the target path, and pass `%*`.
#[cfg(any(windows, test))]
fn windows_shim_contents(source: &Path) -> String {
    format!("@echo off\r\n\"{}\" %*\r\n", source.display())
}

enum PreviousShim {
    Symlink(PathBuf),
    File(Vec<u8>),
}

fn capture_shim(shim: &Path) -> Result<Option<PreviousShim>> {
    let Ok(metadata) = fs::symlink_metadata(shim) else {
        return Ok(None);
    };
    if metadata.file_type().is_symlink() {
        Ok(Some(PreviousShim::Symlink(fs::read_link(shim)?)))
    } else {
        Ok(Some(PreviousShim::File(fs::read(shim)?)))
    }
}

fn restore_shim(shim: &Path, previous: Option<&PreviousShim>) {
    let _ = remove_if_exists(shim);
    match previous {
        None => {}
        Some(PreviousShim::File(bytes)) => {
            let _ = fs::write(shim, bytes);
        }
        Some(PreviousShim::Symlink(target)) => {
            #[cfg(unix)]
            let _ = std::os::unix::fs::symlink(target, shim);
            #[cfg(windows)]
            let _ = std::os::windows::fs::symlink_file(target, shim);
        }
    }
}

fn write_state(
    tx: &mut Transaction,
    root: &Path,
    metadata: &PackageMetadata,
    archive_hash: &str,
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
        bins: metadata.bins.clone(),
        runtime_deps: metadata
            .deps
            .runtime
            .iter()
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
        let origin = match &step.origin {
            Origin::Binary { entry, .. } => format!("binary archive {}", entry.archive),
            Origin::Source { rune } => format!("source rune {}", rune.display()),
        };
        println!("  + {} {} ({})", step.name, step.version, origin);
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

fn remove_if_exists(path: &Path) -> Result<()> {
    if fs::symlink_metadata(path).is_ok() {
        fs::remove_file(path)?;
    }
    Ok(())
}

fn normalize_archive_path(path: &Path) -> String {
    let text = path.to_string_lossy();
    text.strip_prefix("./").unwrap_or(&text).to_owned()
}

#[cfg(unix)]
fn make_executable(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let mut permissions = fs::metadata(path)?.permissions();
    permissions.set_mode(permissions.mode() | 0o111);
    fs::set_permissions(path, permissions)?;
    Ok(())
}

#[cfg(not(unix))]
fn make_executable(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_shim_forwards_arguments_to_target() {
        let shim =
            windows_shim_contents(Path::new(r"C:\grimoire\packages\hello\0.1.0\bin\hello.exe"));
        // CRLF line endings so cmd.exe parses the batch file correctly on Windows.
        assert!(shim.starts_with("@echo off\r\n"), "shim: {shim:?}");
        assert!(shim.ends_with("\r\n"), "shim: {shim:?}");
        // The target path is quoted (so spaces are tolerated) and `%*` forwards every argument.
        assert!(
            shim.contains("\"C:\\grimoire\\packages\\hello\\0.1.0\\bin\\hello.exe\" %*"),
            "shim: {shim:?}"
        );
        // No stray un-quoted invocation that would break on a spaced path.
        assert_eq!(
            shim.matches('"').count(),
            2,
            "exactly one quoted target: {shim:?}"
        );
    }
}
