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
        if let Ok(cands) = self.source.candidates(&dep.name)
            && !cands.is_empty()
        {
            return dep.clone();
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
            if let Some(version) = self.installed.get(provider)
                && dep.req.matches(version)
            {
                return Dependency {
                    name: provider.clone(),
                    req: dep.req.clone(),
                    platform: dep.platform.clone(),
                };
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
        if let Some(version) = self.installed.get(name)
            && req.matches(version)
        {
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
mod tests;
