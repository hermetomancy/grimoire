//! The resolution core: capability expansion, version selection, and backtracking over
//! the whole requirement worklist.

use anyhow::{Context, Result, bail};
use semver::Version;
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    path::PathBuf,
};

use crate::{model::Dependency, model::preferences::Preferences, util::output, util::paths};

use super::*;

/// Maximum number of backtracking steps before resolution gives up. Each step clones the full
/// selection and recurses, so a pathologically over-constrained graph can blow up exponentially;
/// this bound turns an unbounded hang into a clear error. Real resolutions use orders of
/// magnitude fewer steps, so the ceiling is generous enough never to reject a realistic graph.
const RESOLVE_STEP_BUDGET: usize = 500_000;

/// Resolves `roots` (and their transitive runtime dependencies) into an ordered install plan.
/// `installed` maps already-installed package names to their versions; an installed package that
/// still satisfies a requirement is reused and produces no step. `linked` names the installed
/// packages that are currently linked into the active profile — only linked packages block a new
/// selection through `conflicts`; store-only installs do not.
pub fn resolve(
    roots: &[Dependency],
    installed: &BTreeMap<String, Version>,
    linked: &HashSet<String>,
    pins: Option<&Pins>,
) -> Result<Plan> {
    let source = TomeCandidates {
        target: paths::target_triple(),
    };
    let preferences = Preferences::load().unwrap_or_default();
    resolve_with(
        roots,
        installed,
        linked,
        pins,
        &source,
        &preferences.providers,
    )
}

/// Resolution core, parameterized over where candidate versions come from so it can be exercised
/// in isolation. Production resolution reads candidates from configured tomes; see [`resolve`].
/// When `pins` is `Some`, candidate sets are filtered to the recorded version/hash, reproducing a
/// locked install (and a package missing from the map is an error). `linked` names the installed
/// packages currently linked into the active profile.
pub(crate) fn resolve_with(
    roots: &[Dependency],
    installed: &BTreeMap<String, Version>,
    linked: &HashSet<String>,
    pins: Option<&Pins>,
    source: &dyn CandidateSource,
    preferences: &BTreeMap<String, String>,
) -> Result<Plan> {
    // Propagate a genuine build failure (corrupt tome cache, unreadable index) instead of
    // swallowing it into an empty map: the closure walker does the same (`build()?`), and an
    // empty map would make every capability dep fail to expand and surface as a misleading
    // "no version of X satisfies …" rather than the real cause.
    let capabilities = CapabilityIndex::build().context("build capability index")?;
    let mut resolver = Resolver {
        installed,
        linked,
        pins,
        source,
        candidates: HashMap::new(),
        capabilities,
        preferences,
        steps_remaining: RESOLVE_STEP_BUDGET,
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
    conflicts: Vec<String>,
    replaces: Vec<String>,
}

#[derive(Clone)]
pub(crate) struct ChosenNode {
    version: Version,
    /// Runtime dependency names selected for this node, used to order the plan.
    deps: Vec<String>,
    /// `None` when the package is already installed and reused (no install step emitted).
    route: Option<Route>,
    /// Names this package must not coexist with, unless the relationship is superseded.
    conflicts: Vec<String>,
    /// Names exempted from the package's conflicts (this package supersedes them).
    replaces: Vec<String>,
}

pub(crate) struct Resolver<'a> {
    installed: &'a BTreeMap<String, Version>,
    /// Installed packages currently linked into the active profile. Only these participate in
    /// conflict detection; store-only packages may coexist with anything.
    linked: &'a HashSet<String>,
    pins: Option<&'a Pins>,
    source: &'a dyn CandidateSource,
    candidates: HashMap<String, Vec<Candidate>>,
    capabilities: CapabilityIndex,
    /// `grm prefer` choices: capability name -> preferred provider package.
    preferences: &'a BTreeMap<String, String>,
    /// Remaining backtracking steps before resolution aborts (see [`RESOLVE_STEP_BUDGET`]).
    steps_remaining: usize,
}

impl Resolver<'_> {
    /// If `dep.name` is a capability (no literal package by that name, but one or more packages
    /// provide it via their `bins` map), expand it to a concrete provider dependency.
    /// Otherwise return the dep unchanged.
    fn expand_capability(&mut self, dep: &Dependency) -> Dependency {
        // Fast path: literal package exists
        if let Ok(cands) = self.source.candidates(&dep.name)
            && !cands.is_empty()
        {
            return dep.clone();
        }
        let mut providers = self.capabilities.providers(&dep.name);
        if providers.is_empty() {
            return dep.clone();
        }
        // Provider order must be deterministic: the choice folds into the chosen package's
        // dependents' store hashes, and the closure walker (`store::closure`) shares the very
        // same `select_provider` when hashing straight from a rune — both sides must agree.
        providers.sort();
        // A stale preference — the named package no longer provides the capability at all —
        // warns; a preference that still provides it wins inside `select_provider` even when it
        // cannot satisfy `req` (resolution then fails loudly downstream rather than silently
        // substituting a different provider).
        if let Some(preferred) = self.preferences.get(&dep.name)
            && !providers.contains(preferred)
        {
            output::warn(&format!(
                "preference for `{}` names `{preferred}`, which no longer provides it; ignoring",
                dep.name
            ));
        }
        let name = select_provider(
            &providers,
            self.preferences.get(&dep.name),
            self.installed,
            &dep.req,
            |provider| provider_satisfies_req(provider, &dep.req, self.installed),
        )
        .unwrap_or_else(|| dep.name.clone()); // providers is non-empty, so always Some
        Dependency {
            name,
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
        // Bound the backtracking search so an over-constrained graph fails loudly instead of
        // hanging (each recursion clones the selection, so the worst case is exponential).
        if self.steps_remaining == 0 {
            bail!(
                "dependency resolution did not converge within {RESOLVE_STEP_BUDGET} backtracking \
                 steps; the requirement graph is over-constrained or pathologically large"
            );
        }
        self.steps_remaining -= 1;
        let Some((dep, rest)) = worklist.split_first() else {
            return Ok(());
        };
        let expanded = self.expand_capability(dep);
        let (name, req) = (&expanded.name, &expanded.req);

        // Already chosen for another requirement: it must satisfy this one too, then carry on.
        if let Some(node) = chosen.get(name) {
            if crate::model::req_matches(req, &node.version) {
                return self.resolve_list(rest, chosen);
            }
            bail!(
                "version conflict: `{name}` is selected at {} but another dependency requires `{req}`",
                node.version
            );
        }

        let candidates = self.candidates_for(name)?;
        let target = paths::target_triple();

        // Prefer reusing an already-installed satisfying version: it emits no step and its deps are
        // already present (so they are not expanded). But the closure walker addresses every
        // dependency from its current rune — i.e. the newest candidate — so reusing an *older*
        // installed version makes the dependent's planned address fold a different dep hash than the
        // build recomputes: the §9.8 divergence behind a "computed store hash does not match the
        // planned" abort. Reuse only when the installed version is the newest satisfying candidate
        // (or nothing is published for it — a local-only install the walker folds by recorded hash).
        // If reuse leads to a dead end further down the worklist, fall through to fresh candidates.
        let newest_satisfying = candidates
            .iter()
            .filter(|c| crate::model::req_matches(req, &c.version))
            .map(|c| c.version.clone())
            .max();
        if let Some(version) = self.installed.get(name)
            && crate::model::req_matches(req, version)
            && newest_satisfying
                .as_ref()
                .is_none_or(|newest| version == newest)
        {
            let (conflicts, replaces) = self.installed_metadata(name, version);
            if self
                .conflicts_with_selection(name, &conflicts, &replaces, chosen)
                .is_none()
            {
                let mut trial = chosen.clone();
                trial.insert(
                    name.clone(),
                    ChosenNode {
                        version: version.clone(),
                        deps: Vec::new(),
                        route: None,
                        conflicts,
                        replaces,
                    },
                );
                if self.resolve_list(rest, &mut trial).is_ok() {
                    *chosen = trial;
                    return Ok(());
                }
            }
        }
        let mut last_err = None;
        for candidate in candidates {
            if !crate::model::req_matches(req, &candidate.version) {
                continue;
            }
            if let Some(other) = self.conflicts_with_selection(
                name,
                &candidate.conflicts,
                &candidate.replaces,
                chosen,
            ) {
                last_err = Some(
                    anyhow::anyhow!("`{name}` conflicts with chosen `{other}`").context(format!(
                        "no version of `{name}` satisfies `{req}` with its dependencies"
                    )),
                );
                continue;
            }
            // Expand capability names to concrete providers *here*, so the chosen node's dep
            // list (and therefore the plan step's `runtime_deps`) names real packages: the
            // topo order needs the dependent→provider edge, and store-hash computation needs
            // the provider's hash under a name it can find. Worklist re-expansion downstream
            // is idempotent — a literal package name passes through unchanged.
            let runtime: Vec<Dependency> = candidate
                .runtime
                .iter()
                .filter(|d| d.matches_platform(&target))
                .map(|d| self.expand_capability(d))
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
                        conflicts: candidate.conflicts.clone(),
                        replaces: candidate.replaces.clone(),
                    }),
                    conflicts: candidate.conflicts.clone(),
                    replaces: candidate.replaces.clone(),
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

    /// Returns the conflicts/replaces metadata for an installed package by matching it against the
    /// candidate list. If the installed version has no candidate (local archive, orphaned metadata),
    /// the empty lists are returned and the plan-time conflict gate remains the safety net.
    fn installed_metadata(&mut self, name: &str, version: &Version) -> (Vec<String>, Vec<String>) {
        if let Ok(candidates) = self.candidates_for(name)
            && let Some(candidate) = candidates.iter().find(|c| &c.version == version)
        {
            return (candidate.conflicts.clone(), candidate.replaces.clone());
        }
        (Vec::new(), Vec::new())
    }

    /// Returns the name of an already-chosen or installed package that `name` conflicts with, or
    /// `None` if the candidate can coexist with the current selection. A `replaces` declaration in
    /// either direction exempts the pair from conflict, matching the install-time gates.
    fn conflicts_with_selection(
        &mut self,
        name: &str,
        conflicts: &[String],
        replaces: &[String],
        chosen: &BTreeMap<String, ChosenNode>,
    ) -> Option<String> {
        let replaces_set: HashSet<&str> = replaces.iter().map(String::as_str).collect();
        // Candidate declares a conflict with a chosen package (unless candidate replaces it).
        for other in conflicts {
            if chosen.contains_key(other) && !replaces_set.contains(other.as_str()) {
                return Some(other.clone());
            }
        }
        // A chosen package declares a conflict with the candidate (unless it replaces candidate).
        for (other_name, other) in chosen {
            if other_name == name {
                continue;
            }
            if other.conflicts.iter().any(|c| c == name)
                && !other.replaces.iter().any(|r| r == name)
            {
                return Some(other_name.clone());
            }
        }
        // Linked installed packages that are not part of this resolution still block a
        // conflicting candidate; store-only installs never enter the environment, so they do not.
        // The plan-time gate would refuse an unresolvable conflict anyway, so catching it here
        // lets the resolver backtrack to a compatible alternative when one exists.
        for other_name in self.linked {
            if other_name == name {
                continue;
            }
            if conflicts.iter().any(|c| c == other_name)
                && !replaces_set.contains(other_name.as_str())
            {
                return Some(other_name.clone());
            }
            let Some(other_version) = self.installed.get(other_name) else {
                continue;
            };
            let (other_conflicts, other_replaces) =
                self.installed_metadata(other_name, other_version);
            if other_conflicts.iter().any(|c| c == name)
                && !other_replaces.iter().any(|r| r == name)
            {
                return Some(other_name.clone());
            }
        }
        None
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
            conflicts: route.conflicts.clone(),
            replaces: route.replaces.clone(),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests;
