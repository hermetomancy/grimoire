//! Capability resolution for the closure walker.
//!
//! A dependency name that does not match any literal rune is treated as a capability (e.g. `awk`).
//! This module resolves that capability to a concrete provider package, mirroring the solver so the
//! provider folded into a dependent's content address is exactly the one the resolver picked
//! (AGENTS §9.8).

use std::collections::BTreeMap;

use anyhow::{Result, bail};
use semver::{Version, VersionReq};

use crate::install;
use crate::model::preferences::Preferences;
use crate::solve::{CapabilityIndex, provider_satisfies_req, select_provider};

pub(super) struct CapabilityContext {
    index: CapabilityIndex,
    preferences: BTreeMap<String, String>,
    /// Installed provider names mapped to their version, so capability resolution can apply the
    /// dependency's `VersionReq` exactly as the resolver's `expand_capability` does (AGENTS §9.8).
    /// A provider whose recorded version does not parse is omitted — it cannot req-match anyway.
    installed: BTreeMap<String, Version>,
}

impl super::Walker {
    /// Resolves a capability name to one provider package, in the solver's order and against the
    /// same version requirement: the `grm prefer` choice when it still provides the capability,
    /// else the first installed provider *that satisfies `req`*, else the first provider by name.
    /// This mirrors `solve::resolver::expand_capability` step for step — including the req filter
    /// on installed providers — so the provider folded into a dependent's content address is the
    /// same one the resolver picked (AGENTS §9.8). Every step is deterministic (providers sorted
    /// by name). Returns `None` when nothing provides the capability.
    pub(super) fn resolve_capability(
        &mut self,
        name: &str,
        req: &VersionReq,
    ) -> Result<Option<String>> {
        if self.caps.is_none() {
            let installed = install::InstalledWorld::load_default()
                .map(|world| world.installed_versions())
                .unwrap_or_default();
            self.caps = Some(CapabilityContext {
                index: CapabilityIndex::build()?,
                preferences: Preferences::load().unwrap_or_default().providers,
                installed,
            });
        }
        let Some(caps) = self.caps.as_ref() else {
            bail!("capability context was not initialized");
        };
        let mut providers = caps.index.providers(name);
        if providers.is_empty() {
            return Ok(None);
        }
        providers.sort();
        Ok(select_provider(
            &providers,
            caps.preferences.get(name),
            &caps.installed,
            req,
            |provider| provider_satisfies_req(provider, req, &caps.installed),
        ))
    }
}
