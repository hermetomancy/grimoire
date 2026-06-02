//! Version-aware dependency resolution.
//!
//! Given one or more root requirements, the resolver picks a concrete version for every package
//! in the runtime dependency graph that satisfies all accumulated semver requirements. Candidate
//! versions for a package are the binary archives a tome's index offers for the current target
//! plus the single version of its source rune; the highest satisfying version is preferred, and
//! a prebuilt binary is preferred over a source build at the same version. Selection backtracks
//! when a choice cannot satisfy a transitive requirement. The result is an install plan ordered
//! so dependencies precede dependents.

use anyhow::{Context, Result, bail};
use semver::Version;
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    path::PathBuf,
};

use crate::{
    archive, build,
    model::{Dependency, IndexEntry},
    nu::runtime::{EmbeddedNuRuntime, RuneRuntime},
    paths, tome,
};

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

/// Where a planned package comes from: a verified binary archive in a tome's package repo, or a
/// source build of a rune.
#[derive(Clone)]
pub enum Origin {
    Binary { root: PathBuf, entry: IndexEntry },
    Source { rune: PathBuf },
}

/// One package to install, with the concrete version chosen and how to obtain it.
pub struct PlanStep {
    pub name: String,
    pub version: Version,
    pub origin: Origin,
}

/// An ordered set of install steps: dependencies appear before the packages that need them.
pub struct Plan {
    pub steps: Vec<PlanStep>,
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
    let mut resolver = Resolver {
        installed,
        pins,
        source,
        candidates: HashMap::new(),
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
    runtime: Vec<Dependency>,
    origin: Origin,
}

#[derive(Clone)]
struct ChosenNode {
    version: Version,
    /// Runtime dependency names selected for this node, used to order the plan.
    deps: Vec<String>,
    /// `None` when the package is already installed and reused (no install step emitted).
    origin: Option<Origin>,
}

struct Resolver<'a> {
    installed: &'a BTreeMap<String, Version>,
    pins: Option<&'a Pins>,
    source: &'a dyn CandidateSource,
    candidates: HashMap<String, Vec<Candidate>>,
}

impl Resolver<'_> {
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
        let (name, req) = (&dep.name, &dep.req);

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
                        origin: None,
                    },
                );
                if self.resolve_list(rest, &mut trial).is_ok() {
                    *chosen = trial;
                    return Ok(());
                }
            }
        }

        let candidates = self.candidates_for(name)?;
        let mut last_err = None;
        for candidate in candidates {
            if !req.matches(&candidate.version) {
                continue;
            }
            // Try this candidate against a clone so a branch that fails deeper rolls back.
            let mut trial = chosen.clone();
            trial.insert(
                name.clone(),
                ChosenNode {
                    version: candidate.version.clone(),
                    deps: candidate.runtime.iter().map(|d| d.name.clone()).collect(),
                    origin: Some(candidate.origin.clone()),
                },
            );
            let mut next: Vec<Dependency> = rest.to_vec();
            next.extend(candidate.runtime.iter().cloned());
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

/// Filters `candidates` down to those matching `name`'s lockfile pin: the exact version, and for a
/// binary archive the exact archive hash too. A package with no pin is rejected, because a locked
/// install must not pull in anything the lockfile did not record.
fn pin_candidates(name: &str, candidates: Vec<Candidate>, pins: &Pins) -> Result<Vec<Candidate>> {
    let Some(pin) = pins.get(name) else {
        bail!("`{name}` is required but is not recorded in the lockfile; cannot install --locked");
    };
    let filtered: Vec<Candidate> = candidates
        .into_iter()
        .filter(|candidate| {
            candidate.version == pin.version
                && match &candidate.origin {
                    Origin::Binary { entry, .. } => {
                        archive::verify_hash(&entry.archive_hash, &pin.archive_hash).is_ok()
                    }
                    Origin::Source { .. } => true,
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

/// All installable candidates for `name`/`target`: the binary archives every tome's index offers
/// plus the single version of its source rune, sorted highest version first (a prebuilt binary
/// beats a source build at the same version). No downloads happen — this reads index metadata.
fn gather_candidates(name: &str, target: &str) -> Result<Vec<Candidate>> {
    let mut candidates: Vec<Candidate> = Vec::new();
    for tome in tome::load_tomes()? {
        let Some((root, index)) = tome::package_index(&tome)? else {
            continue;
        };
        for entry in index.candidates(name, target) {
            let version = Version::parse(&entry.version)
                .with_context(|| format!("index version `{}` for `{name}`", entry.version))?;
            candidates.push(Candidate {
                version,
                runtime: entry.runtime_deps.clone(),
                origin: Origin::Binary {
                    root: root.clone(),
                    entry: entry.clone(),
                },
            });
        }
    }

    if let Some(rune) = build::find_rune(name)? {
        let metadata = EmbeddedNuRuntime
            .package_metadata(&rune)
            .with_context(|| format!("read rune metadata {}", rune.display()))?;
        let version = Version::parse(&metadata.version)
            .with_context(|| format!("rune version `{}` for `{name}`", metadata.version))?;
        candidates.push(Candidate {
            version,
            runtime: metadata.deps.runtime,
            origin: Origin::Source { rune },
        });
    }

    candidates.sort_by(|a, b| {
        b.version
            .cmp(&a.version)
            .then(rank(&a.origin).cmp(&rank(&b.origin)))
    });
    Ok(candidates)
}

/// The newest version of `name` installable from any configured tome (binary or source), or
/// `None` when no tome offers it. Used by `upgrade` to decide whether a newer release exists.
pub fn newest_available(name: &str) -> Result<Option<Version>> {
    Ok(gather_candidates(name, &paths::target_triple())?
        .into_iter()
        .map(|candidate| candidate.version)
        .next())
}

fn rank(origin: &Origin) -> u8 {
    match origin {
        Origin::Binary { .. } => 0,
        Origin::Source { .. } => 1,
    }
}

/// Emits steps in dependency order (post-order DFS): a package's dependencies are listed before
/// it. Already-installed nodes carry no origin and are skipped, but still order their dependents.
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
    if let Some(origin) = &node.origin {
        steps.push(PlanStep {
            name: name.to_owned(),
            version: node.version.clone(),
            origin: origin.clone(),
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
        }
    }

    /// A source candidate at `version` requiring `deps`; sorting still keeps highest first.
    fn cand(version: &str, deps: &[Dependency]) -> Candidate {
        Candidate {
            version: Version::parse(version).expect("version"),
            runtime: deps.to_vec(),
            origin: Origin::Source {
                rune: PathBuf::from(format!("{version}.rn")),
            },
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
        installed.insert("lib".to_owned(), Version::parse("1.0.0").unwrap());
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
                version: Version::parse("1.0.0").unwrap(),
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
}
