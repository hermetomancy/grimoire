//! Realizing a resolved plan: per-step substitute-vs-source decisions, root installs,
//! dry-run plan printing, and post-commit note reporting for the [`Installer`](super::Installer).

use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};

use crate::{
    build, fetch,
    model::{Dependency, validate_targets},
    solve::{Plan, PlanStep, Substitute},
    tome,
    util::paths,
    util::progress::{report, status},
};

use super::*;

impl Installer {
    /// Installs `name` and its transitive runtime dependencies. The solver picks a concrete
    /// version for every package in the graph and orders the plan so dependencies install first.
    pub(super) fn install_named(&mut self, name: &str) -> Result<String> {
        let mut plan = solve::resolve(
            &[Dependency::any(name)],
            &self.installed,
            self.pins.as_ref(),
        )?;
        plan.compute_store_hashes()
            .with_context(|| format!("compute store hashes for `{name}`"))?;
        if self.dry_run {
            // Print the full picture first — steps, migrations, build-dep closure — then
            // surface the same refusals a real run would hit, so a dry run that would fail
            // says so after showing why.
            print_plan(&plan, &self.installed)?;
            refuse_plan_conflicts(&plan)?;
            return Ok(name.to_owned());
        }
        // Linked-coexistence is decided here, before anything is fetched or built; the
        // realize-time gate remains as defense against stale plans.
        refuse_plan_conflicts(&plan)?;
        self.execute_plan(plan)?;
        Ok(name.to_owned())
    }

    /// Builds `package` (a rune path or known name) from source as the root, then resolves and
    /// installs its runtime dependencies through the solver.
    pub(super) fn install_source_root(&mut self, package: &str) -> Result<String> {
        let rune = build::resolve_rune(package)?;
        if self.dry_run {
            self.dry_run_source_root(&rune)?;
            return Ok(package.to_owned());
        }
        let store_hash = crate::store::closure::store_hash_for_rune(
            &rune,
            &crate::store::closure::installed_resolved(),
        )
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
    pub(super) fn install_local_root(
        &mut self,
        package: &str,
        sha256: Option<String>,
    ) -> Result<String> {
        if self.dry_run {
            self.dry_run_local_root(package)?;
            return Ok(package.to_owned());
        }
        let installed = install_archive(
            &PathBuf::from(package),
            sha256,
            None,
            InstallOrigin::LocalArchive,
        )?;
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
        print_plan_consequences(&plan, &self.installed)?;
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
        print_plan_consequences(&plan, &self.installed)?;
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
        // A pinned content address is the lock's recipe identity: it folds in the rune,
        // sources, dependency closure, and build environment. Drift fails here, before any
        // fetch or build, with a message that names what the lock expected.
        if let (Some(pins), Some(computed)) = (&self.pins, step.store_hash.as_deref())
            && let Some(pinned) = pins
                .get(&step.name)
                .and_then(|pin| pin.store_hash.as_deref())
            && pinned != computed
        {
            bail!(
                "store hash for `{}` drifted from the lock (recipe, sources, or build                          environment changed): locked {pinned}, computed {computed}",
                step.name
            );
        }
        if self.reuse_realized_step(&step)? {
            return Ok(());
        }
        let installed = self
            .realize_step(&step)
            .with_context(|| format!("install `{}` {}", step.name, step.version))?;
        self.record(installed);
        Ok(())
    }

    /// Skips a stale plan step whose package already landed — same name, version, and content
    /// address, with the store path still present — because a deeper build-dependency
    /// recursion installed it after this step's plan was resolved. Without this check an
    /// overlapping build-dep graph (two packages sharing a dependency) realizes the shared
    /// package once per plan that listed it, rebuilding it from source each time.
    fn reuse_realized_step(&mut self, step: &PlanStep) -> Result<bool> {
        if !step_already_realized(step)? {
            return Ok(false);
        }
        status(&format!(
            "{} {} is already in the store; reusing",
            step.name, step.version
        ));
        self.installed
            .insert(step.name.clone(), step.version.clone());
        Ok(true)
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
                (Some(sub), _) => {
                    self.verify_pinned_substitute(step, sub)?;
                    self.install_substitute(sub)
                }
                (None, Some(rune)) => self.build_and_install(rune, require_store_hash(step)?),
                (None, None) => bail!("no pinned artifact available for `{}`", step.name),
            };
        }

        if let Some(hash) = &step.store_hash
            && let Some(sub) = step.substitutes.iter().find(|s| s.store_hash == *hash)
        {
            return self.install_substitute(sub);
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

    /// Under `--locked`, independently re-asserts that the chosen substitute is the exact artifact
    /// the lockfile pinned — both its archive hash and, when recorded, its content address — rather
    /// than trusting the solver's candidate filtering alone. Without this the `store_hash` pin goes
    /// unenforced for a prebuilt-only package: its `step.store_hash` is just the substitute's own,
    /// so the drift check in `execute_step` compares the substitute against itself.
    fn verify_pinned_substitute(&self, step: &PlanStep, sub: &Substitute) -> Result<()> {
        let Some(pins) = &self.pins else {
            return Ok(());
        };
        let Some(pin) = pins.get(&step.name) else {
            bail!(
                "`{}` is not recorded in the lockfile; cannot install --locked",
                step.name
            );
        };
        crate::archive::verify_hash(&sub.entry.archive_hash, &pin.archive_hash).with_context(
            || {
                format!(
                    "substitute for `{}` does not match the locked archive hash",
                    step.name
                )
            },
        )?;
        if let Some(pinned) = &pin.store_hash
            && sub.store_hash != *pinned
        {
            bail!(
                "substitute for `{}` has content address {} but the lock pins {pinned}",
                step.name,
                sub.store_hash
            );
        }
        Ok(())
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
            InstallOrigin::Prebuilt,
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
        // A source root that conflicts with the linked environment must refuse *before*
        // the (potentially hour-long) build, not when the finished archive installs.
        refuse_linked_conflicts(
            &installed_states()?,
            &metadata.name,
            &metadata.conflicts,
            &metadata.replaces,
        )?;
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
            // A prior build of these exact inputs may still sit in cache/builds (e.g. the
            // package was removed and is being reinstalled). Its content address is verified
            // like any substitute's, so reusing it skips the whole rebuild safely.
            if let Some(archive) = cached_build_archive(&metadata, store_hash) {
                return install_archive(
                    &archive,
                    expected_hash,
                    Some(store_hash),
                    InstallOrigin::CachedBuild,
                );
            }
            let build_deps = build::effective_build_deps(rune, &metadata, &paths::target_triple())?;
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
                &crate::store::closure::installed_resolved(),
            )?;
            // Group siblings already landed in the build output dir; their own install
            // steps reuse them via `cached_build_archive` instead of rebuilding the group.
            install_archive(
                &result.primary.archive,
                expected_hash,
                Some(&result.primary.store_hash),
                InstallOrigin::Source,
            )
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
    pub(super) fn report_notes(&self) {
        for (name, lines) in &self.notes {
            report(&format!(
                "notes for {}:",
                crate::util::progress::strong(name)
            ));
            for line in lines {
                crate::util::progress::note(&format!("  {line}"));
            }
        }
    }
}
