//! Solver unit tests: in-memory candidate sources exercising version selection,
//! backtracking, pins, and preference-aware capability expansion.

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
    cand_with_meta(version, deps, &[], &[])
}

fn cand_with_meta(
    version: &str,
    deps: &[Dependency],
    conflicts: &[&str],
    replaces: &[&str],
) -> Candidate {
    Candidate {
        version: parse_version_relaxed(version).expect("version"),
        runtime: deps.to_vec(),
        rune: Some(PathBuf::from(format!("{version}.rn"))),
        substitutes: Vec::new(),
        conflicts: conflicts.iter().map(|s| s.to_string()).collect(),
        replaces: replaces.iter().map(|s| s.to_string()).collect(),
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
    let linked = HashSet::new();
    let plan = resolve_with(roots, &installed, &linked, None, src, &BTreeMap::new())?;
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
    let linked = HashSet::new();
    let mut resolver = Resolver {
        installed,
        linked: &linked,
        pins: None,
        source: &src,
        candidates: HashMap::new(),
        capabilities,
        preferences,
        steps_remaining: RESOLVE_STEP_BUDGET,
    };
    resolver.expand_capability(&dep(dep_name, ">=1.0")).name
}

#[test]
fn pathological_backtracking_aborts_within_budget() {
    // A chain whose leaf is permanently unsatisfiable (`sink` exists only at 1.0.0 but is
    // required at >=5) forces the resolver to try every combination of the upstream packages'
    // two versions — exponential. With a small budget it must abort with a clear error rather
    // than hang. The real budget is far larger; this drives the same mechanism cheaply.
    let two = |next: &str, req: &str| {
        vec![
            cand("2.0.0", &[dep(next, req)]),
            cand("1.0.0", &[dep(next, req)]),
        ]
    };
    let src = source(&[
        ("b0", two("b1", "*")),
        ("b1", two("b2", "*")),
        ("b2", two("b3", "*")),
        ("b3", two("b4", "*")),
        ("b4", two("b5", "*")),
        ("b5", two("sink", ">=5.0.0")),
        ("sink", vec![cand("1.0.0", &[])]),
    ]);
    let installed = BTreeMap::new();
    let preferences = BTreeMap::new();
    let linked = HashSet::new();
    let mut resolver = Resolver {
        installed: &installed,
        linked: &linked,
        pins: None,
        source: &src,
        candidates: HashMap::new(),
        capabilities: CapabilityIndex {
            map: HashMap::new(),
        },
        preferences: &preferences,
        steps_remaining: 32,
    };
    let mut chosen = BTreeMap::new();
    let err = resolver
        .resolve_list(&[dep("b0", "*")], &mut chosen)
        .expect_err("over-constrained graph must abort");
    assert!(
        format!("{err:#}").contains("did not converge"),
        "expected a budget-exhaustion error, got: {err:#}"
    );
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
    // through to the first provider *by name* (deterministic — the choice folds into
    // dependents' store hashes) rather than failing the resolve.
    let installed = BTreeMap::new();
    let preferences = BTreeMap::from([("awk".to_owned(), "nawk".to_owned())]);
    assert_eq!(
        expand("awk", &["mawk", "gawk"], &installed, &preferences),
        "gawk"
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
    // Reuse applies only when the installed version is the newest candidate.
    let src = source(&[("lib", vec![cand("1.0.0", &[]), cand("1.1.0", &[])])]);
    let mut installed = BTreeMap::new();
    installed.insert("lib".to_owned(), parse_version_relaxed("1.1.0").unwrap());
    let linked = HashSet::new();
    let resolved = resolve_with(
        &[dep("lib", ">=1.0")],
        &installed,
        &linked,
        None,
        &src,
        &BTreeMap::new(),
    )
    .expect("plan");
    assert!(
        resolved.steps.is_empty(),
        "installed newest version should produce no step"
    );
}

#[test]
fn rebuilds_when_installed_is_older_than_newest_candidate() {
    // The closure walker addresses `lib` from its current rune (1.1.0), so a stale install (1.0.0)
    // must be rebuilt to the newest rather than reused — otherwise a dependent's planned address
    // folds 1.0.0 while the build recomputes 1.1.0 (the §9.8 store-hash-mismatch abort).
    let src = source(&[("lib", vec![cand("1.0.0", &[]), cand("1.1.0", &[])])]);
    let mut installed = BTreeMap::new();
    installed.insert("lib".to_owned(), parse_version_relaxed("1.0.0").unwrap());
    let linked = HashSet::new();
    let resolved = resolve_with(
        &[dep("lib", ">=1.0")],
        &installed,
        &linked,
        None,
        &src,
        &BTreeMap::new(),
    )
    .expect("plan");
    let steps: Vec<_> = resolved
        .steps
        .into_iter()
        .map(|step| (step.name, step.version.to_string()))
        .collect();
    assert_eq!(
        steps,
        vec![("lib".to_owned(), "1.1.0".to_owned())],
        "{steps:?}"
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
            store_hash: None,
            version: parse_version_relaxed("1.0.0").unwrap(),
            archive_hash: String::new(),
        },
    );
    let linked = HashSet::new();
    let resolved = resolve_with(
        &[dep("app", ">=1.0")],
        &installed,
        &linked,
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
    let linked = HashSet::new();
    let err = match resolve_with(
        &[dep("app", ">=1.0")],
        &installed,
        &linked,
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

#[test]
fn bare_requirement_resolves_a_prerelease_only_package() {
    // The catalog pins unreleased software as `-dev.` prereleases; a plain dependency
    // (`*`) must accept them even though strict semver excludes prereleases from `*`.
    let src = source(&[("grimoire", vec![cand("0.1.0-dev.20260612", &[])])]);
    let resolved = plan(&[Dependency::any("grimoire")], &src)
        .expect("prerelease-only package resolves under a bare requirement");
    assert_eq!(
        resolved,
        vec![("grimoire".to_owned(), "0.1.0-dev.20260612".to_owned())]
    );
}

#[test]
fn backtracks_on_conflicting_newest_version() {
    // `app` 2.0 conflicts with `lib`, but `app` 1.0 does not. Resolution must backtrack from
    // the higher version to the compatible one instead of producing a conflicting plan.
    let src = source(&[
        (
            "app",
            vec![
                cand_with_meta("2.0.0", &[dep("base", ">=1.0")], &["lib"], &[]),
                cand("1.0.0", &[dep("base", ">=1.0")]),
            ],
        ),
        ("lib", vec![cand("1.0.0", &[])]),
        ("base", vec![cand("1.0.0", &[])]),
    ]);
    let steps = plan(&[dep("app", ">=1.0"), dep("lib", ">=1.0")], &src).expect("plan");
    assert!(
        steps.contains(&("app".to_owned(), "1.0.0".to_owned())),
        "resolver should backtrack to the non-conflicting app 1.0.0: {steps:?}"
    );
    assert!(
        steps.contains(&("lib".to_owned(), "1.0.0".to_owned())),
        "lib should still be selected: {steps:?}"
    );
}

#[test]
fn backtracks_when_dependency_conflicts_with_chosen_package() {
    // `app` 2.0 depends on `helper` >=2.0, but `helper` 2.0 conflicts with `stable` which is
    // already required by another root. `helper` 1.0 is compatible, so app must downgrade.
    let src = source(&[
        (
            "app",
            vec![
                cand("2.0.0", &[dep("helper", ">=2.0")]),
                cand("1.0.0", &[dep("helper", ">=1.0, <2.0")]),
            ],
        ),
        (
            "helper",
            vec![
                cand_with_meta("2.0.0", &[], &["stable"], &[]),
                cand("1.5.0", &[]),
            ],
        ),
        ("stable", vec![cand("1.0.0", &[])]),
    ]);
    let steps = plan(&[dep("app", ">=1.0"), dep("stable", ">=1.0")], &src).expect("plan");
    assert!(
        steps.contains(&("app".to_owned(), "1.0.0".to_owned())),
        "app should backtrack to 1.0.0: {steps:?}"
    );
    assert!(
        steps.contains(&("helper".to_owned(), "1.5.0".to_owned())),
        "helper 1.5.0 should be selected: {steps:?}"
    );
    assert!(
        steps.contains(&("stable".to_owned(), "1.0.0".to_owned())),
        "stable should remain selected: {steps:?}"
    );
}

#[test]
fn replaces_exempts_conflicting_pair() {
    // `app` 2.0 both conflicts with and replaces `legacy`, so they may coexist.
    let src = source(&[
        (
            "app",
            vec![
                cand_with_meta("2.0.0", &[], &["legacy"], &["legacy"]),
                cand("1.0.0", &[]),
            ],
        ),
        ("legacy", vec![cand("1.0.0", &[])]),
    ]);
    let steps = plan(&[dep("app", ">=2.0"), dep("legacy", ">=1.0")], &src).expect("plan");
    assert!(
        steps.contains(&("app".to_owned(), "2.0.0".to_owned())),
        "app 2.0.0 should be selected because it replaces legacy: {steps:?}"
    );
}

#[test]
fn installed_reuse_falls_back_when_conflicts_with_resolution() {
    // `legacy` is installed at 1.0.0 and conflicts with the newest `app` 2.0. A non-conflicting
    // version of `app` exists, so the resolver should pick it instead of reusing the installed
    // `legacy` (which would be refused by the plan-time gate).
    let src = source(&[
        (
            "app",
            vec![
                cand_with_meta("2.0.0", &[], &["legacy"], &[]),
                cand("1.0.0", &[]),
            ],
        ),
        ("legacy", vec![cand("1.0.0", &[])]),
    ]);
    let mut installed = BTreeMap::new();
    installed.insert("legacy".to_owned(), Version::new(1, 0, 0));
    let linked: HashSet<String> = HashSet::from(["legacy".to_owned()]);
    let resolved = resolve_with(
        &[dep("app", ">=1.0"), dep("legacy", ">=1.0")],
        &installed,
        &linked,
        None,
        &src,
        &BTreeMap::new(),
    )
    .expect("plan");
    let steps: Vec<_> = resolved
        .steps
        .into_iter()
        .map(|s| (s.name, s.version.to_string()))
        .collect();
    assert!(
        steps.contains(&("app".to_owned(), "1.0.0".to_owned())),
        "app should backtrack to the version that does not conflict with installed legacy: {steps:?}"
    );
}

#[test]
fn unresolved_conflict_reports_conflict_message() {
    // Every version of `app` conflicts with `stable`, and both are required. The error should
    // mention the conflict rather than a generic "no version satisfies".
    let src = source(&[
        (
            "app",
            vec![
                cand_with_meta("2.0.0", &[], &["stable"], &[]),
                cand_with_meta("1.0.0", &[], &["stable"], &[]),
            ],
        ),
        ("stable", vec![cand("1.0.0", &[])]),
    ]);
    let err = plan(&[dep("app", ">=1.0"), dep("stable", ">=1.0")], &src).expect_err("should fail");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("conflicts with chosen"),
        "expected a conflict-specific error, got: {msg}"
    );
}
