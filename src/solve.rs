//! Version-aware dependency resolution.
//!
//! Given one or more root requirements, the resolver picks a concrete version for every package
//! in the runtime dependency graph that satisfies all accumulated semver requirements. Candidate
//! versions for a package merge, per version, the prebuilt archives a tome's index offers for the
//! current target with the source rune that defines that version (the rune being authoritative for
//! its runtime dependencies); the highest satisfying version is preferred. Selection backtracks when
//! a choice cannot satisfy a transitive requirement. The result is an install plan ordered so
//! dependencies precede dependents — each step carrying its rune and the prebuilt substitutes the
//! installer then chooses between by store hash.

use anyhow::{Context, Result, bail};
use semver::Version;
use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    path::PathBuf,
};

use crate::{
    addendum, archive, build, closure,
    model::{Dependency, IndexEntry, PackageIndex, parse_version_relaxed},
    nu::runtime::{EmbeddedNuRuntime, RuneRuntime},
    paths, tome, toolchain,
};

/// Maps capability names (e.g. "awk", "sh") to the package names that provide them.
/// Built once per resolve by reading tome indexes first, then falling back to runes
/// for packages not yet indexed.
#[derive(Clone)]
struct CapabilityIndex {
    map: HashMap<String, Vec<String>>,
}

impl CapabilityIndex {
    fn build() -> Result<Self> {
        let mut map: HashMap<String, Vec<String>> = HashMap::new();
        let target = paths::target_triple();
        let tomes = tome::load_tomes()?;

        // 1. Read capabilities from published tome indexes (authoritative).
        for tome in &tomes {
            let cache = tome::ensure_tome_cache(tome)?;
            let index_path = cache.join("dist").join("index.nuon");
            if !index_path.exists() {
                continue;
            }
            let index = match crate::nu::nuon_io::read_nuon(&index_path) {
                Ok(v) => match PackageIndex::from_value(v) {
                    Ok(idx) => idx,
                    Err(_) => continue,
                },
                Err(_) => continue,
            };
            for (_, entry) in index.entries {
                if entry.target != target {
                    continue;
                }
                Self::record_provides(&entry.name, &entry.provides, &mut map);
            }
        }

        // 2. Fall back to runes for packages not in any index.
        for tome in &tomes {
            let cache = tome::ensure_tome_cache(tome)?;
            let runes_dir = cache.join("runes");
            if !runes_dir.exists() {
                continue;
            }
            for entry in std::fs::read_dir(&runes_dir)? {
                let path = entry?.path();
                if path.extension().and_then(|e| e.to_str()) != Some("rn") {
                    continue;
                }
                let metadata = match EmbeddedNuRuntime.package_metadata(&path) {
                    Ok(m) => m,
                    Err(_) => continue,
                };
                // Only record from rune if this package wasn't already recorded from an index.
                Self::record_capabilities_from_rune(&metadata, &target, &mut map);
            }
        }
        Ok(Self { map })
    }

    fn record_provides(
        package_name: &str,
        provides: &[String],
        map: &mut HashMap<String, Vec<String>>,
    ) {
        for name in provides {
            let providers = map.entry(name.clone()).or_default();
            if !providers.contains(&package_name.to_owned()) {
                providers.push(package_name.to_owned());
            }
        }
    }

    fn record_capabilities_from_rune(
        metadata: &crate::model::PackageMetadata,
        target: &str,
        map: &mut HashMap<String, Vec<String>>,
    ) {
        for bin_name in metadata.bins_for(target).keys() {
            if *bin_name == metadata.name {
                continue;
            }
            let providers = map.entry(bin_name.clone()).or_default();
            if !providers.contains(&metadata.name) {
                providers.push(metadata.name.clone());
            }
        }
    }

    fn providers(&self, capability: &str) -> Vec<String> {
        self.map.get(capability).cloned().unwrap_or_default()
    }
}

/// A package pinned by the lockfile: the exact version and archive hash last installed. Used by
/// `install --locked` to constrain resolution to the recorded reproducible set.
#[derive(Clone)]
pub struct Pin {
    pub version: Version,
    pub archive_hash: String,
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
                let dep_hashes: BTreeMap<String, String> = step
                    .runtime_deps
                    .iter()
                    .map(|dep_name| {
                        computed
                            .get(dep_name)
                            .cloned()
                            .map(|h| (dep_name.clone(), h))
                    })
                    .collect::<Option<BTreeMap<_, _>>>()
                    .ok_or_else(|| {
                        anyhow::anyhow!("missing computed hash for dependency of `{}`", step.name)
                    })?;
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

/// Resolves `roots` (and their transitive runtime dependencies) into an ordered install plan.
/// `installed` maps already-installed package names to their versions; an installed package that
/// still satisfies a requirement is reused and produces no step.
pub fn resolve(
    roots: &[Dependency],
    installed: &BTreeMap<String, Version>,
    pins: Option<&Pins>,
) -> Result<Plan> {
    let source = TomeCandidates {
        target: paths::target_triple(),
    };
    resolve_with(roots, installed, pins, &source)
}

/// Resolution core, parameterized over where candidate versions come from so it can be exercised
/// in isolation. Production resolution reads candidates from configured tomes; see [`resolve`].
/// When `pins` is `Some`, candidate sets are filtered to the recorded version/hash, reproducing a
/// locked install (and a package missing from the map is an error).
fn resolve_with(
    roots: &[Dependency],
    installed: &BTreeMap<String, Version>,
    pins: Option<&Pins>,
    source: &dyn CandidateSource,
) -> Result<Plan> {
    let capabilities = CapabilityIndex::build().unwrap_or_else(|_| CapabilityIndex {
        map: HashMap::new(),
    });
    let mut resolver = Resolver {
        installed,
        pins,
        source,
        candidates: HashMap::new(),
        capabilities,
    };
    let mut chosen: BTreeMap<String, ChosenNode> = BTreeMap::new();
    // All roots are resolved as one worklist so backtracking can revise an early choice when a
    // later root introduces a conflicting requirement, not just within a single root's subtree.
    resolver
        .resolve_list(roots, &mut chosen)
        .context("resolve dependencies")?;
    Ok(Plan {
        steps: topo_order(&chosen)?,
    })
}

/// Supplies the installable candidate versions for a package name, highest version first.
trait CandidateSource {
    fn candidates(&self, name: &str) -> Result<Vec<Candidate>>;
}

/// Production candidate source: gathers binary index entries and source runes from every
/// configured tome for the current target.
struct TomeCandidates {
    target: String,
}

impl CandidateSource for TomeCandidates {
    fn candidates(&self, name: &str) -> Result<Vec<Candidate>> {
        gather_candidates(name, &self.target)
    }
}

#[derive(Clone)]
struct Candidate {
    version: Version,
    /// Runtime dependencies for this version. Authoritative from the source rune when one defines
    /// the version; otherwise taken from the index entry.
    runtime: Vec<Dependency>,
    rune: Option<PathBuf>,
    substitutes: Vec<Substitute>,
}

/// Prebuilt substitutes grouped by version, paired with the runtime deps the index entry declares.
type VersionCandidates = BTreeMap<Version, (Vec<Dependency>, Vec<Substitute>)>;

/// How a chosen package is realized: the source rune and/or the prebuilt substitutes available for
/// the selected version. `None` route means an already-installed version is reused (no step).
#[derive(Clone)]
struct Route {
    rune: Option<PathBuf>,
    substitutes: Vec<Substitute>,
}

#[derive(Clone)]
struct ChosenNode {
    version: Version,
    /// Runtime dependency names selected for this node, used to order the plan.
    deps: Vec<String>,
    /// `None` when the package is already installed and reused (no install step emitted).
    route: Option<Route>,
}

struct Resolver<'a> {
    installed: &'a BTreeMap<String, Version>,
    pins: Option<&'a Pins>,
    source: &'a dyn CandidateSource,
    candidates: HashMap<String, Vec<Candidate>>,
    capabilities: CapabilityIndex,
}

impl Resolver<'_> {
    /// If `dep.name` is a capability (no literal package by that name, but one or more packages
    /// provide it via their `bins` map), expand it to a concrete provider dependency.
    /// Otherwise return the dep unchanged.
    fn expand_capability(&mut self, dep: &Dependency) -> Dependency {
        // Fast path: literal package exists
        if let Ok(cands) = self.source.candidates(&dep.name) {
            if !cands.is_empty() {
                return dep.clone();
            }
        }
        let providers = self.capabilities.providers(&dep.name);
        if providers.is_empty() {
            return dep.clone();
        }
        // Prefer an already-installed provider that satisfies the version constraint.
        for provider in &providers {
            if let Some(version) = self.installed.get(provider) {
                if dep.req.matches(version) {
                    return Dependency {
                        name: provider.clone(),
                        req: dep.req.clone(),
                        platform: dep.platform.clone(),
                    };
                }
            }
        }
        // No installed provider matches; use the first available one.
        // TODO: prompt user when multiple uninstalled providers exist.
        Dependency {
            name: providers[0].clone(),
            req: dep.req.clone(),
            platform: dep.platform.clone(),
        }
    }

    /// Resolves an ordered worklist of requirements into `chosen`, backtracking across the whole
    /// list. The head requirement is satisfied first; the chosen package's own runtime deps are
    /// appended to the remaining worklist so a conflict they introduce can backtrack *this* choice
    /// rather than only deeper ones. Because the entire remainder is resolved under each trial,
    /// an early choice is revised when a later requirement (including one from a different root)
    /// cannot otherwise be met. Returns the completed selection or an error if none exists.
    fn resolve_list(
        &mut self,
        worklist: &[Dependency],
        chosen: &mut BTreeMap<String, ChosenNode>,
    ) -> Result<()> {
        let Some((dep, rest)) = worklist.split_first() else {
            return Ok(());
        };
        let expanded = self.expand_capability(dep);
        let (name, req) = (&expanded.name, &expanded.req);

        // Already chosen for another requirement: it must satisfy this one too, then carry on.
        if let Some(node) = chosen.get(name) {
            if req.matches(&node.version) {
                return self.resolve_list(rest, chosen);
            }
            bail!(
                "version conflict: `{name}` is selected at {} but another dependency requires `{req}`",
                node.version
            );
        }

        // Prefer reusing an already-installed satisfying version: it emits no step and its deps
        // are already present (so they are not expanded). If reuse leads to a dead end further
        // down the worklist, fall through and try fresh candidates instead.
        if let Some(version) = self.installed.get(name) {
            if req.matches(version) {
                let mut trial = chosen.clone();
                trial.insert(
                    name.clone(),
                    ChosenNode {
                        version: version.clone(),
                        deps: Vec::new(),
                        route: None,
                    },
                );
                if self.resolve_list(rest, &mut trial).is_ok() {
                    *chosen = trial;
                    return Ok(());
                }
            }
        }

        let candidates = self.candidates_for(name)?;
        let target = paths::target_triple();
        let mut last_err = None;
        for candidate in candidates {
            if !req.matches(&candidate.version) {
                continue;
            }
            let runtime: Vec<Dependency> = candidate
                .runtime
                .iter()
                .filter(|d| d.matches_platform(&target))
                .cloned()
                .collect();
            // Try this candidate against a clone so a branch that fails deeper rolls back.
            let mut trial = chosen.clone();
            trial.insert(
                name.clone(),
                ChosenNode {
                    version: candidate.version.clone(),
                    deps: runtime.iter().map(|d| d.name.clone()).collect(),
                    route: Some(Route {
                        rune: candidate.rune.clone(),
                        substitutes: candidate.substitutes.clone(),
                    }),
                },
            );
            let mut next: Vec<Dependency> = rest.to_vec();
            next.extend(runtime);
            match self.resolve_list(&next, &mut trial) {
                Ok(()) => {
                    *chosen = trial;
                    return Ok(());
                }
                Err(err) => last_err = Some(err),
            }
        }

        match last_err {
            Some(err) => Err(err).with_context(|| {
                format!("no version of `{name}` satisfies `{req}` with its dependencies")
            }),
            None => bail!("no version of `{name}` satisfies `{req}`"),
        }
    }

    fn candidates_for(&mut self, name: &str) -> Result<Vec<Candidate>> {
        if let Some(cached) = self.candidates.get(name) {
            return Ok(cached.clone());
        }
        let mut candidates = self.source.candidates(name)?;
        if let Some(pins) = self.pins {
            candidates = pin_candidates(name, candidates, pins)?;
        }
        self.candidates.insert(name.to_owned(), candidates.clone());
        Ok(candidates)
    }
}

/// Filters `candidates` down to those matching `name`'s lockfile pin: the exact version, and the
/// exact archive hash for any prebuilt substitute. A package with no pin is rejected, because a
/// locked install must not pull in anything the lockfile did not record. A source rune is retained
/// so a package the lockfile recorded as source-built reproduces by rebuilding.
fn pin_candidates(name: &str, candidates: Vec<Candidate>, pins: &Pins) -> Result<Vec<Candidate>> {
    let Some(pin) = pins.get(name) else {
        bail!("`{name}` is required but is not recorded in the lockfile; cannot install --locked");
    };
    let filtered: Vec<Candidate> = candidates
        .into_iter()
        .filter_map(|mut candidate| {
            if candidate.version != pin.version {
                return None;
            }
            candidate.substitutes.retain(|sub| {
                archive::verify_hash(&sub.entry.archive_hash, &pin.archive_hash).is_ok()
            });
            // Keep the version only if it can still be realized: a pin-matching prebuilt, or a rune
            // to rebuild a source-pinned package.
            if candidate.substitutes.is_empty() && candidate.rune.is_none() {
                None
            } else {
                Some(candidate)
            }
        })
        .collect();
    if filtered.is_empty() {
        bail!(
            "no candidate for `{name}` matches the locked version {} (hash {})",
            pin.version,
            pin.archive_hash
        );
    }
    Ok(filtered)
}

/// All installable candidates for `name`/`target`, one per version, sorted highest version first.
/// Each version merges the prebuilt substitutes every tome's index offers with the source rune that
/// defines it (when present); the rune is authoritative for that version's runtime dependencies. No
/// downloads happen — this reads index metadata and the rune.
fn gather_candidates(name: &str, target: &str) -> Result<Vec<Candidate>> {
    let by_version = gather_index_candidates(name, target)?;
    let rune = gather_rune_candidate(name, target)?;

    let mut versions: BTreeSet<Version> = by_version.keys().cloned().collect();
    if let Some((version, _, _)) = &rune {
        versions.insert(version.clone());
    }

    let mut candidates: Vec<Candidate> = versions
        .into_iter()
        .map(|version| {
            let substitutes = by_version
                .get(&version)
                .map(|(_, subs)| subs.clone())
                .unwrap_or_default();
            let (rune_path, runtime) = match &rune {
                Some((rune_version, runtime, path)) if *rune_version == version => {
                    (Some(path.clone()), runtime.clone())
                }
                _ => (
                    None,
                    by_version
                        .get(&version)
                        .map(|(deps, _)| deps.clone())
                        .unwrap_or_default(),
                ),
            };
            Candidate {
                version,
                runtime,
                rune: rune_path,
                substitutes,
            }
        })
        .collect();

    candidates.sort_by(|a, b| b.version.cmp(&a.version));
    Ok(candidates)
}

fn gather_index_candidates(name: &str, target: &str) -> Result<VersionCandidates> {
    let mut by_version: VersionCandidates = BTreeMap::new();
    for tome in tome::load_tomes()? {
        let Some((root, index)) = tome::package_index(&tome)? else {
            continue;
        };
        for (store_hash, entry) in index.candidates(name, target) {
            let version = parse_version_relaxed(&entry.version)
                .with_context(|| format!("index version `{}` for `{name}`", entry.version))?;
            let filtered_runtime: Vec<Dependency> = entry
                .runtime_deps
                .iter()
                .filter(|d| d.matches_platform(target))
                .cloned()
                .collect();
            let slot = by_version
                .entry(version)
                .or_insert_with(|| (filtered_runtime.clone(), Vec::new()));
            // Ensure the slot uses the filtered runtime deps even on first insertion.
            slot.0 = filtered_runtime;
            slot.1.push(Substitute {
                root: root.clone(),
                store_hash: store_hash.to_string(),
                entry: entry.clone(),
                tome_name: tome.name.clone(),
            });
        }
    }
    Ok(by_version)
}

fn gather_rune_candidate(
    name: &str,
    target: &str,
) -> Result<Option<(Version, Vec<Dependency>, PathBuf)>> {
    let Some(rune) = build::find_rune(name)? else {
        return Ok(None);
    };
    let mut metadata = EmbeddedNuRuntime
        .package_metadata(&rune)
        .with_context(|| format!("read rune metadata {}", rune.display()))?;
    addendum::patched_package_metadata(
        &mut metadata,
        build::tome_name_for_rune(&rune)?.as_deref(),
        &rune,
    )
    .with_context(|| format!("apply addendums to {}", rune.display()))?;
    let version = parse_version_relaxed(&metadata.version)
        .with_context(|| format!("rune version `{}` for `{name}`", metadata.version))?;
    let runtime: Vec<Dependency> = metadata
        .deps
        .runtime
        .into_iter()
        .filter(|d| d.matches_platform(target))
        .collect();
    Ok(Some((version, runtime, rune)))
}

/// The newest version of `name` installable from any configured tome (binary or source), or
/// `None` when no tome offers it. Used by `upgrade` to decide whether a newer release exists.
pub fn newest_available(name: &str) -> Result<Option<Version>> {
    Ok(gather_candidates(name, &paths::target_triple())?
        .into_iter()
        .map(|candidate| candidate.version)
        .next())
}

/// Emits steps in dependency order (post-order DFS): a package's dependencies are listed before
/// it. Already-installed nodes carry no route and are skipped, but still order their dependents.
fn topo_order(chosen: &BTreeMap<String, ChosenNode>) -> Result<Vec<PlanStep>> {
    let mut visited = HashSet::new();
    let mut on_stack = HashSet::new();
    let mut steps = Vec::new();
    for name in chosen.keys() {
        visit(name, chosen, &mut visited, &mut on_stack, &mut steps)?;
    }
    Ok(steps)
}

fn visit(
    name: &str,
    chosen: &BTreeMap<String, ChosenNode>,
    visited: &mut HashSet<String>,
    on_stack: &mut HashSet<String>,
    steps: &mut Vec<PlanStep>,
) -> Result<()> {
    if visited.contains(name) {
        return Ok(());
    }
    if !on_stack.insert(name.to_owned()) {
        bail!("dependency cycle involving `{name}`");
    }
    let node = &chosen[name];
    for dep in &node.deps {
        if chosen.contains_key(dep) {
            visit(dep, chosen, visited, on_stack, steps)?;
        }
    }
    on_stack.remove(name);
    visited.insert(name.to_owned());
    if let Some(route) = &node.route {
        steps.push(PlanStep {
            name: name.to_owned(),
            version: node.version.clone(),
            rune: route.rune.clone(),
            substitutes: route.substitutes.clone(),
            store_hash: None,
            runtime_deps: node.deps.clone(),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use semver::VersionReq;

    /// In-memory candidate source so the resolver can be exercised without tomes on disk.
    struct FakeCandidates(HashMap<String, Vec<Candidate>>);

    impl CandidateSource for FakeCandidates {
        fn candidates(&self, name: &str) -> Result<Vec<Candidate>> {
            Ok(self.0.get(name).cloned().unwrap_or_default())
        }
    }

    fn dep(name: &str, req: &str) -> Dependency {
        Dependency {
            name: name.to_owned(),
            req: VersionReq::parse(req).expect("req"),
            platform: None,
        }
    }

    /// A source candidate at `version` requiring `deps`; sorting still keeps highest first.
    fn cand(version: &str, deps: &[Dependency]) -> Candidate {
        Candidate {
            version: parse_version_relaxed(version).expect("version"),
            runtime: deps.to_vec(),
            rune: Some(PathBuf::from(format!("{version}.rn"))),
            substitutes: Vec::new(),
        }
    }

    fn source(entries: &[(&str, Vec<Candidate>)]) -> FakeCandidates {
        let mut map = HashMap::new();
        for (name, mut cands) in entries.iter().cloned() {
            cands.sort_by(|a, b| b.version.cmp(&a.version));
            map.insert(name.to_owned(), cands);
        }
        FakeCandidates(map)
    }

    fn plan(roots: &[Dependency], src: &FakeCandidates) -> Result<Vec<(String, String)>> {
        let installed = BTreeMap::new();
        let plan = resolve_with(roots, &installed, None, src)?;
        Ok(plan
            .steps
            .into_iter()
            .map(|step| (step.name, step.version.to_string()))
            .collect())
    }

    #[test]
    fn picks_highest_version_satisfying_requirement() {
        let src = source(&[(
            "app",
            vec![cand("1.0.0", &[]), cand("1.2.0", &[]), cand("2.0.0", &[])],
        )]);
        let steps = plan(&[dep("app", ">=1.0, <2.0")], &src).expect("plan");
        assert_eq!(steps, vec![("app".to_owned(), "1.2.0".to_owned())]);
    }

    #[test]
    fn orders_dependencies_before_dependents() {
        let src = source(&[
            ("app", vec![cand("1.0.0", &[dep("lib", ">=1.0")])]),
            ("lib", vec![cand("1.0.0", &[])]),
        ]);
        let steps = plan(&[dep("app", ">=1.0")], &src).expect("plan");
        assert_eq!(
            steps,
            vec![
                ("lib".to_owned(), "1.0.0".to_owned()),
                ("app".to_owned(), "1.0.0".to_owned()),
            ]
        );
    }

    #[test]
    fn backtracks_to_older_version_when_newest_conflicts() {
        // `app` 2.0 needs both `lib` >=2.0 and `tool`, but `tool` pins `lib` to 1.x. That
        // combination is unsatisfiable, so resolution backtracks to `app` 1.0, whose own
        // requirements (`lib` 1.x + `tool`) are mutually consistent.
        let src = source(&[
            (
                "app",
                vec![
                    cand("2.0.0", &[dep("lib", ">=2.0"), dep("tool", ">=1.0")]),
                    cand("1.0.0", &[dep("lib", ">=1.0, <2.0"), dep("tool", ">=1.0")]),
                ],
            ),
            ("tool", vec![cand("1.0.0", &[dep("lib", ">=1.0, <2.0")])]),
            ("lib", vec![cand("2.0.0", &[]), cand("1.5.0", &[])]),
        ]);
        let steps = plan(&[dep("app", ">=1.0")], &src).expect("plan");
        assert!(
            steps.contains(&("app".to_owned(), "1.0.0".to_owned())),
            "{steps:?}"
        );
        assert!(
            steps.contains(&("lib".to_owned(), "1.5.0".to_owned())),
            "{steps:?}"
        );
        assert!(
            steps.contains(&("tool".to_owned(), "1.0.0".to_owned())),
            "{steps:?}"
        );
    }

    #[test]
    fn backtracks_across_independent_roots() {
        // Two separate roots share `lib`. `app`'s newest (2.0) wants `lib` >=2.0, but `tool`
        // (a different root) pins `lib` to 1.x. A resolver that committed `app` 2.0 before even
        // looking at `tool` would deadlock; unified backtracking must revise `app` down to 1.0.
        let src = source(&[
            (
                "app",
                vec![
                    cand("2.0.0", &[dep("lib", ">=2.0")]),
                    cand("1.0.0", &[dep("lib", ">=1.0, <2.0")]),
                ],
            ),
            ("tool", vec![cand("1.0.0", &[dep("lib", ">=1.0, <2.0")])]),
            ("lib", vec![cand("2.0.0", &[]), cand("1.5.0", &[])]),
        ]);
        let steps = plan(&[dep("app", ">=1.0"), dep("tool", ">=1.0")], &src).expect("plan");
        assert!(
            steps.contains(&("app".to_owned(), "1.0.0".to_owned())),
            "{steps:?}"
        );
        assert!(
            steps.contains(&("lib".to_owned(), "1.5.0".to_owned())),
            "{steps:?}"
        );
        assert!(
            steps.contains(&("tool".to_owned(), "1.0.0".to_owned())),
            "{steps:?}"
        );
    }

    #[test]
    fn reuses_installed_version_without_emitting_step() {
        let src = source(&[("lib", vec![cand("1.0.0", &[]), cand("1.1.0", &[])])]);
        let mut installed = BTreeMap::new();
        installed.insert("lib".to_owned(), parse_version_relaxed("1.0.0").unwrap());
        let resolved = resolve_with(&[dep("lib", ">=1.0")], &installed, None, &src).expect("plan");
        assert!(
            resolved.steps.is_empty(),
            "installed satisfying version should produce no step"
        );
    }

    #[test]
    fn fails_when_no_candidate_satisfies() {
        let src = source(&[("app", vec![cand("1.0.0", &[])])]);
        let err = plan(&[dep("app", ">=2.0")], &src).expect_err("should fail");
        assert!(
            format!("{err:#}").contains("no version of `app`"),
            "unexpected error: {err:#}"
        );
    }

    #[test]
    fn pins_constrain_resolution_to_locked_version() {
        // Newest is 2.0, but the pin records 1.0 — resolution must reproduce the pinned version.
        let src = source(&[("app", vec![cand("1.0.0", &[]), cand("2.0.0", &[])])]);
        let installed = BTreeMap::new();
        let mut pins = Pins::new();
        pins.insert(
            "app".to_owned(),
            Pin {
                version: parse_version_relaxed("1.0.0").unwrap(),
                archive_hash: String::new(),
            },
        );
        let resolved =
            resolve_with(&[dep("app", ">=1.0")], &installed, Some(&pins), &src).expect("plan");
        assert_eq!(
            resolved
                .steps
                .iter()
                .map(|s| (s.name.clone(), s.version.to_string()))
                .collect::<Vec<_>>(),
            vec![("app".to_owned(), "1.0.0".to_owned())]
        );
    }

    #[test]
    fn locked_install_rejects_unpinned_package() {
        let src = source(&[("app", vec![cand("1.0.0", &[])])]);
        let installed = BTreeMap::new();
        let pins = Pins::new();
        let err = match resolve_with(&[dep("app", ">=1.0")], &installed, Some(&pins), &src) {
            Ok(_) => panic!("expected unpinned package to be rejected"),
            Err(err) => err,
        };
        assert!(
            format!("{err:#}").contains("not recorded in the lockfile"),
            "unexpected error: {err:#}"
        );
    }

    #[test]
    fn fails_on_unsatisfiable_shared_dependency() {
        // Two roots demand disjoint ranges of the same dependency: no single version works.
        let src = source(&[
            ("a", vec![cand("1.0.0", &[dep("shared", ">=2.0")])]),
            ("b", vec![cand("1.0.0", &[dep("shared", "<2.0")])]),
            ("shared", vec![cand("2.0.0", &[]), cand("1.0.0", &[])]),
        ]);
        let err = plan(&[dep("a", ">=1.0"), dep("b", ">=1.0")], &src).expect_err("should fail");
        assert!(
            format!("{err:#}").contains("shared") || format!("{err:#}").contains("conflict"),
            "unexpected error: {err:#}"
        );
    }

    fn platform_dep(name: &str, req: &str, platform: &str) -> Dependency {
        Dependency {
            name: name.to_owned(),
            req: VersionReq::parse(req).expect("req"),
            platform: Some(platform.to_owned()),
        }
    }

    #[test]
    fn filters_platform_conditional_deps_that_do_not_match() {
        let target = paths::target_triple();
        let current_os = target.split('-').next().unwrap();
        let other_os = if current_os == "linux" {
            "macos"
        } else {
            "linux"
        };
        let src = source(&[
            (
                "app",
                vec![cand(
                    "1.0.0",
                    &[
                        platform_dep("current-os-dep", "*", current_os),
                        platform_dep("other-os-dep", "*", other_os),
                    ],
                )],
            ),
            ("current-os-dep", vec![cand("1.0.0", &[])]),
            ("other-os-dep", vec![cand("1.0.0", &[])]),
        ]);
        let steps = plan(&[dep("app", ">=1.0")], &src).expect("plan");
        assert!(
            steps.iter().any(|(n, _)| n == "current-os-dep"),
            "current-os-dep should be kept: {steps:?}"
        );
        assert!(
            !steps.iter().any(|(n, _)| n == "other-os-dep"),
            "other-os-dep should be filtered out: {steps:?}"
        );
    }

    #[test]
    fn keeps_platform_conditional_deps_that_match_glob() {
        let target = paths::target_triple();
        // Build a glob that matches the current target: "*-*-*" always matches.
        let pattern = format!(
            "{}-*-{}",
            target.split('-').next().unwrap(),
            target.split('-').nth(2).unwrap()
        );
        let src = source(&[
            (
                "app",
                vec![cand("1.0.0", &[platform_dep("matched-dep", "*", &pattern)])],
            ),
            ("matched-dep", vec![cand("1.0.0", &[])]),
        ]);
        let steps = plan(&[dep("app", ">=1.0")], &src).expect("plan");
        assert!(steps.iter().any(|(n, _)| n == "matched-dep"), "{steps:?}");
    }

    #[test]
    fn filters_platform_conditional_deps_that_do_not_match_glob() {
        let target = paths::target_triple();
        // A glob that can never match any real target triple: swap OS and ABI.
        let pattern = format!(
            "{}-*-{}",
            target.split('-').nth(2).unwrap(),
            target.split('-').next().unwrap()
        );
        let src = source(&[
            (
                "app",
                vec![cand(
                    "1.0.0",
                    &[platform_dep("unmatched-dep", "*", &pattern)],
                )],
            ),
            ("unmatched-dep", vec![cand("1.0.0", &[])]),
        ]);
        let steps = plan(&[dep("app", ">=1.0")], &src).expect("plan");
        assert!(
            !steps.iter().any(|(n, _)| n == "unmatched-dep"),
            "unmatched-dep should be filtered out: {steps:?}"
        );
    }
}
