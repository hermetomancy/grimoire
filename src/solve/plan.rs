//! The solver output: ordered install steps with their realization routes and eagerly
//! computed store hashes.

use anyhow::{Context, Result, bail};
use semver::Version;
use std::{collections::BTreeMap, path::PathBuf};

use crate::{build::toolchain, model::IndexEntry, store::closure, util::paths};

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
        let build_env = toolchain::build_env_id().unwrap_or_default();
        let mut computed: BTreeMap<String, String> = BTreeMap::new();

        // Pre-populate with already-installed packages so reused dependencies
        // do not cause "missing computed hash" errors.
        if let Ok(states) = crate::install::installed_states() {
            for state in states {
                computed.insert(state.name.clone(), state.store_hash.clone());
            }
        }

        for step in &mut self.steps {
            let hash = if let Some(rune) = &step.rune {
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
                closure::store_hash_for_rune_with_deps(rune, &dep_hashes, &target, &build_env)
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
            step.store_hash = Some(hash);
        }
        Ok(())
    }
}
