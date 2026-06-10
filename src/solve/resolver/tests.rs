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
