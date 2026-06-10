//! Candidate gathering: the installable versions for a package name, merged per version
//! from tome index entries and source runes, plus lockfile pin filtering.

use anyhow::{Context, Result, bail};
use semver::Version;
use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
};

use crate::{
    archive, build,
    model::{Dependency, parse_version_relaxed},
    tome,
    util::paths,
};

use super::*;

/// Supplies the installable candidate versions for a package name, highest version first.
pub(crate) trait CandidateSource {
    fn candidates(&self, name: &str) -> Result<Vec<Candidate>>;
}

/// Production candidate source: gathers binary index entries and source runes from every
/// configured tome for the current target.
pub(crate) struct TomeCandidates {
    pub(crate) target: String,
}

impl CandidateSource for TomeCandidates {
    fn candidates(&self, name: &str) -> Result<Vec<Candidate>> {
        gather_candidates(name, &self.target)
    }
}

#[derive(Clone)]
pub(crate) struct Candidate {
    pub(crate) version: Version,
    /// Runtime dependencies for this version. Authoritative from the source rune when one defines
    /// the version; otherwise taken from the index entry.
    pub(crate) runtime: Vec<Dependency>,
    pub(crate) rune: Option<PathBuf>,
    pub(crate) substitutes: Vec<Substitute>,
}

/// Prebuilt substitutes grouped by version, paired with the runtime deps the index entry declares.
pub(crate) type VersionCandidates = BTreeMap<Version, (Vec<Dependency>, Vec<Substitute>)>;

/// Filters `candidates` down to those matching `name`'s lockfile pin: the exact version, and the
/// exact archive hash for any prebuilt substitute. A package with no pin is rejected, because a
/// locked install must not pull in anything the lockfile did not record. A source rune is retained
/// so a package the lockfile recorded as source-built reproduces by rebuilding.
pub(crate) fn pin_candidates(
    name: &str,
    candidates: Vec<Candidate>,
    pins: &Pins,
) -> Result<Vec<Candidate>> {
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
pub(crate) fn gather_candidates(name: &str, target: &str) -> Result<Vec<Candidate>> {
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

pub(crate) fn gather_index_candidates(name: &str, target: &str) -> Result<VersionCandidates> {
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

pub(crate) fn gather_rune_candidate(
    name: &str,
    target: &str,
) -> Result<Option<(Version, Vec<Dependency>, PathBuf)>> {
    let Some(rune) = build::find_rune(name)? else {
        return Ok(None);
    };
    let metadata = build::read_rune_metadata(&rune, build::tome_name_for_rune(&rune)?.as_deref())?;
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
