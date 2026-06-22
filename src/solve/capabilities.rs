//! The capability index: which packages provide which command names, read from published
//! tome indexes first and runes as a fallback.

use anyhow::Result;
use semver::{Version, VersionReq};
use std::collections::{BTreeMap, HashMap};

use crate::{
    model::PackageIndex, model::req_matches, nu::runtime::EmbeddedNuRuntime, tome, util::paths,
};

/// Maps capability names (e.g. "awk", "sh") to the package names that provide them.
/// Built once per resolve by reading tome indexes first, then falling back to runes
/// for packages not yet indexed.
#[derive(Clone)]
pub(crate) struct CapabilityIndex {
    pub(crate) map: HashMap<String, Vec<String>>,
}

impl CapabilityIndex {
    pub(crate) fn build() -> Result<Self> {
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

    pub(crate) fn record_provides(
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

    pub(crate) fn record_capabilities_from_rune(
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
        // A rune can also declare non-binary capabilities via `provides`, exactly like a
        // published index entry; harvest them so source-only packages resolve the same way.
        Self::record_provides(&metadata.name, &metadata.provides, map);
    }

    pub(crate) fn providers(&self, capability: &str) -> Vec<String> {
        self.map.get(capability).cloned().unwrap_or_default()
    }
}

/// The package names that provide `capability`, from published tome indexes and runes. The
/// read-only seam `grm provides` uses; [`CapabilityIndex`] itself stays private to the solver.
pub fn capability_providers(capability: &str) -> Result<Vec<String>> {
    Ok(CapabilityIndex::build()?.providers(capability))
}

/// Picks the provider of a capability from `providers` (which MUST already be sorted by name),
/// honoring, in order: a `grm prefer` choice that still provides the capability; the first
/// installed provider whose version satisfies `req`; the first provider that *can* satisfy `req`
/// (per `can_satisfy`); the first provider by name. Returns `None` only when `providers` is empty.
///
/// The `can_satisfy` step is what keeps a satisfiable graph from being reported unsatisfiable when
/// the lexically-first provider has no version matching `req` but another does. It must be a
/// deterministic function of inputs both the resolver and the walk see identically (the provider's
/// rune-declared version plus installed versions — see [`provider_satisfies_req`]); a check over
/// the prebuilt index, which the walk cannot see, would reintroduce divergence.
///
/// This is the **single** definition of provider selection. Both the resolver
/// ([`super::resolver`]) and the closure walker ([`crate::store::closure`]) call it, so the
/// provider folded into a dependent's content address is identical on both paths — the store
/// address determinism §9.8 depends on.
pub(crate) fn select_provider(
    providers: &[String],
    preference: Option<&String>,
    installed: &BTreeMap<String, Version>,
    req: &VersionReq,
    can_satisfy: impl Fn(&str) -> bool,
) -> Option<String> {
    if let Some(preferred) = preference
        && providers.contains(preferred)
    {
        return Some(preferred.clone());
    }
    if let Some(installed) = providers.iter().find(|provider| {
        installed
            .get(*provider)
            .is_some_and(|v| req_matches(req, v))
    }) {
        return Some(installed.clone());
    }
    if let Some(capable) = providers.iter().find(|provider| can_satisfy(provider)) {
        return Some(capable.clone());
    }
    providers.first().cloned()
}

/// Whether `provider` has a version satisfying `req`, judged only from inputs both the resolver and
/// the closure walk read identically: an installed version, or the version its rune declares. This
/// deliberately excludes the prebuilt index (visible to the resolver, not the walk) so the two
/// paths agree on the chosen provider and the store address stays reproducible (§9.8).
pub(crate) fn provider_satisfies_req(
    provider: &str,
    req: &VersionReq,
    installed: &BTreeMap<String, Version>,
) -> bool {
    if installed.get(provider).is_some_and(|v| req_matches(req, v)) {
        return true;
    }
    let Ok(Some(rune)) = crate::build::find_rune(provider) else {
        return false;
    };
    let Ok(metadata) = EmbeddedNuRuntime.package_metadata(&rune) else {
        return false;
    };
    crate::model::parse_version_relaxed(&metadata.version)
        .is_ok_and(|version| req_matches(req, &version))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::PackageMetadata;
    use std::collections::BTreeMap;

    #[test]
    fn rune_capabilities_include_bins_and_provides() {
        let mut bins = BTreeMap::new();
        bins.insert(
            "default".to_owned(),
            BTreeMap::from([
                ("gawk".to_owned(), "bin/gawk".to_owned()),
                ("awk".to_owned(), "bin/gawk".to_owned()),
            ]),
        );
        let metadata = PackageMetadata {
            name: "gawk".to_owned(),
            version: "5.3.0".to_owned(),
            target: None,
            store_path: None,
            targets: Vec::new(),
            fixed_output: false,
            build_only: false,
            summary: None,
            bins,
            sources: BTreeMap::new(),
            deps: Default::default(),
            build_flags: BTreeMap::new(),
            provides: vec!["text-processor".to_owned()],
            libs: Vec::new(),
            notes: Vec::new(),
            upstream_version: None,
            conflicts: Vec::new(),
            replaces: Vec::new(),
            split_from: None,
            files: Vec::new(),
        };
        let mut map = HashMap::new();
        CapabilityIndex::record_capabilities_from_rune(&metadata, "linux-x86_64-musl", &mut map);
        assert_eq!(map.get("awk"), Some(&vec!["gawk".to_owned()]));
        assert_eq!(map.get("text-processor"), Some(&vec!["gawk".to_owned()]));
        assert!(
            !map.contains_key("gawk"),
            "self-named bin must not be a capability"
        );
    }

    fn providers(names: &[&str]) -> Vec<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    fn installed(pairs: &[(&str, &str)]) -> BTreeMap<String, Version> {
        pairs
            .iter()
            .map(|(n, v)| (n.to_string(), Version::parse(v).unwrap()))
            .collect()
    }

    // The req-aware `can_satisfy` step is only consulted when no preference and no installed
    // provider matches; the `none_capable` default exercises the older steps in isolation.
    fn none_capable(_: &str) -> bool {
        false
    }

    #[test]
    fn installed_provider_must_satisfy_the_requirement() {
        // The exact §9.8 divergence: `afoo` sorts first and is installed but only `zfoo`
        // satisfies `>=2`. A req-blind pick would take `afoo`; the resolver takes `zfoo`. Both
        // paths now run this one function, so they cannot disagree.
        let providers = providers(&["afoo", "zfoo"]);
        let installed = installed(&[("afoo", "1.0.0"), ("zfoo", "2.0.0")]);
        let req = VersionReq::parse(">=2.0.0").unwrap();
        assert_eq!(
            select_provider(&providers, None, &installed, &req, none_capable),
            Some("zfoo".to_string())
        );
    }

    #[test]
    fn first_installed_satisfying_provider_wins_over_later_ones() {
        let providers = providers(&["afoo", "zfoo"]);
        let installed = installed(&[("afoo", "2.1.0"), ("zfoo", "2.0.0")]);
        let req = VersionReq::parse(">=2.0.0").unwrap();
        assert_eq!(
            select_provider(&providers, None, &installed, &req, none_capable),
            Some("afoo".to_string())
        );
    }

    #[test]
    fn preference_wins_regardless_of_requirement() {
        let providers = providers(&["afoo", "zfoo"]);
        let installed = installed(&[("afoo", "2.0.0")]);
        let pref = "zfoo".to_string();
        // Preference is honored even when it cannot satisfy req (resolution fails loudly
        // downstream); the resolver behaves identically, so addresses still agree.
        assert_eq!(
            select_provider(
                &providers,
                Some(&pref),
                &installed,
                &VersionReq::parse(">=9").unwrap(),
                none_capable,
            ),
            Some("zfoo".to_string())
        );
    }

    #[test]
    fn capable_provider_is_preferred_over_first_by_name() {
        // #15: `afoo` sorts first but cannot satisfy `>=2`; `zfoo` can. With nothing installed,
        // the req-aware step picks `zfoo` instead of failing on the unsatisfiable `afoo`.
        let providers = providers(&["afoo", "zfoo"]);
        let req = VersionReq::parse(">=2.0.0").unwrap();
        assert_eq!(
            select_provider(&providers, None, &BTreeMap::new(), &req, |p| p == "zfoo"),
            Some("zfoo".to_string())
        );
    }

    #[test]
    fn falls_back_to_first_by_name_when_no_provider_can_satisfy() {
        // Genuinely unsatisfiable: fall back to the first by name so resolution fails loudly
        // downstream with a clear "no version satisfies" rather than here.
        let providers = providers(&["afoo", "zfoo"]);
        let req = VersionReq::parse(">=2.0.0").unwrap();
        assert_eq!(
            select_provider(&providers, None, &BTreeMap::new(), &req, none_capable),
            Some("afoo".to_string())
        );
    }

    #[test]
    fn empty_providers_yields_none() {
        assert_eq!(
            select_provider(&[], None, &BTreeMap::new(), &VersionReq::STAR, none_capable),
            None
        );
    }
}
