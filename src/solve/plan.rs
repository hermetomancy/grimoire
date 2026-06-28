//! The solver output: ordered install steps with their realization routes and eagerly
//! computed store hashes.

use anyhow::{Context, Result, bail};
use semver::Version;
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use crate::{
    build::{self, toolchain},
    model::IndexEntry,
    store::closure,
    util::paths,
};

/// A package pinned by the lockfile: the exact version and archive hash last installed. Used by
/// `install --locked` to constrain resolution to the recorded reproducible set.
#[derive(Clone)]
pub struct Pin {
    pub version: Version,
    pub archive_hash: String,
    /// The locked content address, when the lockfile recorded one. Checked against the
    /// computed store hash before realization so recipe/source/build-env drift fails loudly
    /// *before* anything is fetched or built.
    pub store_hash: Option<String>,
}

/// Lockfile pins keyed by package name. When supplied to [`resolve`], every package in the graph
/// must match its pin (and any package absent from the map is rejected), reproducing a prior install.
pub type Pins = BTreeMap<String, Pin>;

/// A prebuilt archive published for a resolved package version: a candidate *substitute* for a
/// source build. The installer selects one by matching its `store_hash` against the store hash
/// recomputed from the local rune, so the binhost is queried by content address.
#[derive(Clone)]
pub struct Substitute {
    pub root: PathBuf,
    pub store_hash: String,
    pub entry: IndexEntry,
    pub tome_name: String,
}

/// One package to install at a resolved version, plus the two ways to realize it: the source `rune`
/// that can build it (when one is available) and the prebuilt `substitutes` published for that
/// version. The installer prefers a substitute whose store hash matches the inputs, otherwise builds
/// from the rune. At least one of `rune`/`substitutes` is always present.
pub struct PlanStep {
    pub name: String,
    pub version: Version,
    pub rune: Option<PathBuf>,
    pub substitutes: Vec<Substitute>,
    /// The content-addressed store hash computed eagerly after version resolution.
    pub store_hash: Option<String>,
    /// Runtime dependency names, used for hash computation.
    pub runtime_deps: Vec<String>,
    /// `conflicts`/`replaces` metadata for the resolved version, so linked-coexistence
    /// decisions resolve at plan time (and surface in `--dry-run`) instead of mid-install.
    pub conflicts: Vec<String>,
    pub replaces: Vec<String>,
}

/// An ordered set of install steps: dependencies appear before the packages that need them.
pub struct Plan {
    pub steps: Vec<PlanStep>,
}

impl Plan {
    /// Computes the content-addressed store hash for every step in the plan.
    /// Steps are already in topo-order (dependencies before dependents), so each
    /// dependency's hash is available when its dependents need it.
    pub fn compute_store_hashes(&mut self) -> Result<()> {
        let target = paths::target_triple();
        let mut computed: BTreeMap<String, String> = BTreeMap::new();
        let mut installed_versions: BTreeMap<String, Version> = BTreeMap::new();

        // Pre-populate with already-installed packages so reused dependencies
        // do not cause "missing computed hash" errors.
        if let Ok(world) = crate::install::InstalledWorld::load_default() {
            for state in world.iter() {
                computed.insert(state.name.clone(), state.store_hash.clone());
                if let Ok(version) = crate::model::parse_version_relaxed(&state.version) {
                    installed_versions.insert(state.name.clone(), version);
                }
            }
        }

        self.compute_store_hashes_inner(&target, &mut computed, &mut installed_versions)
    }

    fn compute_store_hashes_inner(
        &mut self,
        target: &str,
        computed: &mut BTreeMap<String, String>,
        installed_versions: &mut BTreeMap<String, Version>,
    ) -> Result<()> {
        for step in &mut self.steps {
            let hash = if let Some(rune) = &step.rune {
                seed_build_dep_hashes(rune, target, computed, installed_versions).with_context(
                    || format!("compute build dependency hashes for `{}`", step.name),
                )?;
                let build_env =
                    toolchain::store_build_env_id_for_target_with_resolved(false, target, computed);
                // `runtime_deps` carries the resolver's *expanded* names (capabilities already
                // replaced by concrete providers) in the rune's declaration order, so the
                // hashes are passed positionally rather than looked up by raw dep name.
                let dep_hashes: Vec<String> = step
                    .runtime_deps
                    .iter()
                    .map(|dep_name| {
                        computed.get(dep_name).cloned().ok_or_else(|| {
                            anyhow::anyhow!(
                                "missing computed hash for `{dep_name}`, a dependency of `{}`",
                                step.name
                            )
                        })
                    })
                    .collect::<Result<Vec<_>>>()?;
                // For a split member, `computed` carries the resolver's chosen hashes for the
                // group's external deps, so the closure walk folds those versions instead of an
                // independent re-pick — the resolver and the build address the member identically.
                closure::store_hash_for_rune_with_deps(
                    rune,
                    &dep_hashes,
                    target,
                    &build_env,
                    computed,
                )
                .with_context(|| format!("compute store hash for `{}`", step.name))?
            } else if let Some(sub) = step.substitutes.first() {
                sub.store_hash.clone()
            } else {
                bail!(
                    "cannot compute store hash for `{}`: no rune and no substitutes",
                    step.name
                );
            };
            computed.insert(step.name.clone(), hash.clone());
            installed_versions.insert(step.name.clone(), step.version.clone());
            step.store_hash = Some(hash);
        }
        Ok(())
    }
}

fn seed_build_dep_hashes(
    rune: &Path,
    target: &str,
    computed: &mut BTreeMap<String, String>,
    installed_versions: &mut BTreeMap<String, Version>,
) -> Result<()> {
    let metadata = build::read_rune_metadata(rune, build::tome_name_for_rune(rune)?.as_deref())?;
    let build_deps = build::effective_build_deps(rune, &metadata, target)?;
    if build_deps.is_empty() {
        return Ok(());
    }

    let mut plan =
        crate::solve::resolve(&build_deps, installed_versions, &Default::default(), None)
            .with_context(|| format!("resolve build dependencies for `{}`", metadata.name))?;
    plan.compute_store_hashes_inner(target, computed, installed_versions)
}
