//! The resolution core: capability expansion, version selection, and backtracking over
//! the whole requirement worklist.

use anyhow::{Context, Result, bail};
use semver::Version;
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    path::PathBuf,
};

use crate::{model::Dependency, model::preferences::Preferences, util::paths, util::progress};

use super::*;

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
    let preferences = Preferences::load().unwrap_or_default();
    resolve_with(roots, installed, pins, &source, &preferences.providers)
}

/// Resolution core, parameterized over where candidate versions come from so it can be exercised
/// in isolation. Production resolution reads candidates from configured tomes; see [`resolve`].
/// When `pins` is `Some`, candidate sets are filtered to the recorded version/hash, reproducing a
/// locked install (and a package missing from the map is an error).
pub(crate) fn resolve_with(
    roots: &[Dependency],
    installed: &BTreeMap<String, Version>,
    pins: Option<&Pins>,
    source: &dyn CandidateSource,
    preferences: &BTreeMap<String, String>,
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
        preferences,
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

/// How a chosen package is realized: the source rune and/or the prebuilt substitutes available for
/// the selected version. `None` route means an already-installed version is reused (no step).
#[derive(Clone)]
pub(crate) struct Route {
    rune: Option<PathBuf>,
    substitutes: Vec<Substitute>,
}

#[derive(Clone)]
pub(crate) struct ChosenNode {
    version: Version,
    /// Runtime dependency names selected for this node, used to order the plan.
    deps: Vec<String>,
    /// `None` when the package is already installed and reused (no install step emitted).
    route: Option<Route>,
}

pub(crate) struct Resolver<'a> {
    installed: &'a BTreeMap<String, Version>,
    pins: Option<&'a Pins>,
    source: &'a dyn CandidateSource,
    candidates: HashMap<String, Vec<Candidate>>,
    capabilities: CapabilityIndex,
    /// `grm prefer` choices: capability name -> preferred provider package.
    preferences: &'a BTreeMap<String, String>,
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
        // A `grm prefer` choice wins over everything: it is explicit user intent. If the
        // preferred package cannot satisfy the version requirement, resolution fails loudly
        // downstream instead of silently substituting a different provider. Only a *stale*
        // preference — the named package no longer provides the capability at all — warns and
        // falls through to the default selection.
        if let Some(preferred) = self.preferences.get(&dep.name) {
            if providers.contains(preferred) {
                return Dependency {
                    name: preferred.clone(),
                    req: dep.req.clone(),
                    platform: dep.platform.clone(),
                };
            }
            progress::report(&format!(
                "preference for `{}` names `{preferred}`, which no longer provides it; ignoring",
                dep.name
            ));
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
        // No installed provider matches; use the first available one. With multiple uninstalled
        // providers the pick is arbitrary — `grm prefer <capability> <package>` decides it.
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

/// Emits steps in dependency order (post-order DFS): a package's dependencies are listed before
/// it. Already-installed nodes carry no route and are skipped, but still order their dependents.
pub(crate) fn topo_order(chosen: &BTreeMap<String, ChosenNode>) -> Result<Vec<PlanStep>> {
    let mut visited = HashSet::new();
    let mut on_stack = HashSet::new();
    let mut steps = Vec::new();
    for name in chosen.keys() {
        visit(name, chosen, &mut visited, &mut on_stack, &mut steps)?;
    }
    Ok(steps)
}

pub(crate) fn visit(
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
    use crate::model::parse_version_relaxed;
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
        let plan = resolve_with(roots, &installed, None, src, &BTreeMap::new())?;
        Ok(plan
            .steps
            .into_iter()
            .map(|step| (step.name, step.version.to_string()))
            .collect())
    }

    /// Expands `dep_name` against a synthetic capability index (capability -> providers),
    /// an installed set, and `grm prefer` choices, bypassing the on-disk tome index.
    fn expand(
        dep_name: &str,
        providers: &[&str],
        installed: &BTreeMap<String, Version>,
        preferences: &BTreeMap<String, String>,
    ) -> String {
        let src = source(&[]);
        let capabilities = CapabilityIndex {
            map: HashMap::from([(
                dep_name.to_owned(),
                providers.iter().map(|p| p.to_string()).collect(),
            )]),
        };
        let mut resolver = Resolver {
            installed,
            pins: None,
            source: &src,
            candidates: HashMap::new(),
            capabilities,
            preferences,
        };
        resolver.expand_capability(&dep(dep_name, ">=1.0")).name
    }

    #[test]
    fn preference_overrides_default_capability_provider() {
        let installed = BTreeMap::new();
        let preferences = BTreeMap::from([("awk".to_owned(), "gawk".to_owned())]);
        assert_eq!(
            expand("awk", &["mawk", "gawk"], &installed, &preferences),
            "gawk"
        );
    }

    #[test]
    fn preference_overrides_installed_provider() {
        // mawk is installed and satisfies the req, but the user prefers gawk — explicit
        // intent wins over the reuse-what-is-installed default.
        let installed = BTreeMap::from([("mawk".to_owned(), Version::new(1, 0, 0))]);
        let preferences = BTreeMap::from([("awk".to_owned(), "gawk".to_owned())]);
        assert_eq!(
            expand("awk", &["mawk", "gawk"], &installed, &preferences),
            "gawk"
        );
    }

    #[test]
    fn stale_preference_falls_back_to_default_provider() {
        // The preferred package no longer provides the capability at all: warn and fall
        // through to the first provider rather than failing the resolve.
        let installed = BTreeMap::new();
        let preferences = BTreeMap::from([("awk".to_owned(), "nawk".to_owned())]);
        assert_eq!(
            expand("awk", &["mawk", "gawk"], &installed, &preferences),
            "mawk"
        );
    }

    #[test]
    fn no_preference_keeps_installed_provider_first() {
        let installed = BTreeMap::from([("gawk".to_owned(), Version::new(1, 0, 0))]);
        assert_eq!(
            expand("awk", &["mawk", "gawk"], &installed, &BTreeMap::new()),
            "gawk"
        );
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
        let resolved = resolve_with(
            &[dep("lib", ">=1.0")],
            &installed,
            None,
            &src,
            &BTreeMap::new(),
        )
        .expect("plan");
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
        let resolved = resolve_with(
            &[dep("app", ">=1.0")],
            &installed,
            Some(&pins),
            &src,
            &BTreeMap::new(),
        )
        .expect("plan");
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
        let err = match resolve_with(
            &[dep("app", ">=1.0")],
            &installed,
            Some(&pins),
            &src,
            &BTreeMap::new(),
        ) {
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
