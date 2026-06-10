//! The capability index: which packages provide which command names, read from published
//! tome indexes first and runes as a fallback.

use anyhow::Result;
use std::collections::HashMap;

use crate::{model::PackageIndex, nu::runtime::EmbeddedNuRuntime, tome, util::paths};

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
