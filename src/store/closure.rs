//! Content-addressing a package over its dependency closure.
//!
//! A compiled package's store hash folds in the store hashes of its runtime dependencies, so the
//! whole closure is captured transitively (Nix-style). Computing that address requires resolving
//! each dependency to its rune and recursing — a pure walk over the rune graph, with no building or
//! installing. This is what `grm tome build` records in the index and what tests predict via the
//! `store-hash` seam. The installer derives the same address incrementally from the store hashes of
//! the dependencies it has already installed.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;

use anyhow::{Context, Result, bail};

use crate::{build, build::toolchain, store, util::paths};

/// One member's hash inputs when addressing a split group: its metadata and the sha256 of
/// its rune bytes. Assembled from disk runes or from a member archive's embedded group.
struct GroupPart {
    name: String,
    metadata: crate::model::PackageMetadata,
    rune_hash: String,
}

fn disk_group_parts(group: &build::split::SplitGroup) -> Result<Vec<GroupPart>> {
    let bytes = group.rune_bytes()?;
    group
        .members()
        .map(|member| {
            let rune_bytes = bytes.get(&member.name).with_context(|| {
                format!("missing rune bytes for group member `{}`", member.name)
            })?;
            Ok(GroupPart {
                name: member.name.clone(),
                metadata: member.metadata.clone(),
                rune_hash: store::hash_bytes(rune_bytes),
            })
        })
        .collect()
}

/// Computes the content address (store hash) of the package named `name`, resolving its runtime
/// dependency closure to store hashes.
pub fn store_hash(name: &str) -> Result<String> {
    Walker::new()?.of_name(name)
}

/// Like [`store_hash`], but for a specific rune file (e.g. a `grm build <path>`), so the exact rune
/// given is hashed rather than whatever `find_rune` would resolve for its name.
pub fn store_hash_for_rune(rune: &Path) -> Result<String> {
    let mut walker = Walker::new()?;
    let metadata = walker.metadata(rune)?;
    walker.of_rune(&metadata.name, rune)
}

/// Like [`store_hash_for_rune`], but for an explicit target triple instead of the host default.
pub fn store_hash_for_rune_with_target(rune: &Path, target: &str) -> Result<String> {
    let mut walker = Walker::with_target(target)?;
    let metadata = walker.metadata(rune)?;
    walker.of_rune(&metadata.name, rune)
}

/// Computes the store hash from already-read rune bytes and metadata.
/// Avoids writing a temporary file when the bytes are already in memory.
///
/// Split group members cannot be addressed from a single rune: use
/// [`store_hash_for_split_archive`], which takes the whole group.
pub fn store_hash_for_rune_bytes(
    rune_bytes: &[u8],
    metadata: &crate::model::PackageMetadata,
) -> Result<String> {
    if metadata.is_split_member() {
        bail!(
            "`{}` is a split member; its store hash is derived from the whole group",
            metadata.name
        );
    }
    let mut walker = Walker::new()?;
    walker.of_rune_with_bytes(&metadata.name, metadata, rune_bytes)
}

/// The store hashes of every member of a split group, keyed by package name, for the host
/// target. `group` carries each member's metadata and raw rune bytes (parent included), as
/// read from disk or from a member archive's embedded `.grimoire/group/` copies.
pub fn split_member_hashes(
    group: &[(crate::model::PackageMetadata, Vec<u8>)],
) -> Result<BTreeMap<String, String>> {
    Walker::new()?.group_hashes(&group_parts(group))
}

/// Like [`split_member_hashes`], for an explicit target triple.
pub fn split_member_hashes_with_target(
    group: &[(crate::model::PackageMetadata, Vec<u8>)],
    target: &str,
) -> Result<BTreeMap<String, String>> {
    Walker::with_target(target)?.group_hashes(&group_parts(group))
}

fn group_parts(group: &[(crate::model::PackageMetadata, Vec<u8>)]) -> Vec<GroupPart> {
    group
        .iter()
        .map(|(metadata, bytes)| GroupPart {
            name: metadata.name.clone(),
            metadata: metadata.clone(),
            rune_hash: store::hash_bytes(bytes),
        })
        .collect()
}

/// Computes the store hash for a rune whose dependency closure has already been resolved.
/// `dep_hashes` are the store hashes of the rune's platform-filtered runtime deps **in
/// declaration order** — the resolver may have expanded capability names to concrete
/// providers, so a by-name lookup against the rune's raw dep names is impossible here.
/// This is used by the solver after version resolution to compute hashes eagerly.
pub fn store_hash_for_rune_with_deps(
    rune: &Path,
    dep_hashes: &[String],
    target: &str,
    build_env: &str,
) -> Result<String> {
    let metadata = build::read_rune_metadata(rune, build::tome_name_for_rune(rune)?.as_deref())?;

    // A split group member's address folds the *whole group's* external deps, which the
    // caller's positional list (the member's own deps) cannot supply. Fall back to the
    // closure walk, which resolves the union deterministically — the same path the builder
    // uses, so the addresses agree by construction.
    if let Some(group) = build::split::group_for(rune)? {
        let mut walker = Walker::with_target(target)?;
        walker.build_env = build_env.to_string();
        let hashes = walker.group_hashes(&disk_group_parts(&group)?)?;
        return hashes.get(&metadata.name).cloned().with_context(|| {
            format!("split group for `{}` did not yield its hash", metadata.name)
        });
    }

    let declared = metadata
        .deps
        .runtime
        .iter()
        .filter(|dep| dep.matches_platform(target))
        .count();
    if declared != dep_hashes.len() {
        bail!(
            "rune `{}` declares {declared} runtime dep(s) for {target} but {} hash(es) were \
             supplied",
            metadata.name,
            dep_hashes.len()
        );
    }

    let rune_bytes =
        std::fs::read(rune).with_context(|| format!("read rune {}", rune.display()))?;
    Ok(store::store_hash_for_metadata(
        &metadata,
        &rune_bytes,
        dep_hashes,
        target,
        build_env,
    ))
}

/// Installed packages whose recorded address has drifted from the catalog: the rune currently
/// resolvable for the same name *and version* produces a different store hash — its content,
/// declared sources, build flags, dependency closure, or the host build environment changed
/// since the package was realized. Resolution must not reuse these by version; re-realizing
/// them is how a rune edit propagates to installs at all.
///
/// Conservative on both sides: a package whose rune cannot be resolved or hashed (installed
/// from a local archive, or whose deps only resolve as capabilities) is never reported — there
/// is nothing to rebuild it from; and a rune that moved to a *different* version is `grm
/// upgrade`'s business, not drift. One walker memoizes the closure walk across the whole set.
/// Returns `(name, expected store hash)` pairs so callers can report what the package
/// would re-address to, not just that it drifted.
pub fn stale_installed(states: &[crate::model::PackageState]) -> Vec<(String, String)> {
    let Ok(mut walker) = Walker::new() else {
        return Vec::new();
    };
    let mut stale = Vec::new();
    for state in states {
        // A hold pins the installed bits, not just the version: a held package is never
        // re-realized for drift. `grm unhold` lets the pending drift apply.
        if state.held {
            continue;
        }
        let Ok(Some(rune)) = build::find_rune(&state.name) else {
            continue;
        };
        let Ok(metadata) = walker.metadata(&rune) else {
            continue;
        };
        if metadata.version != state.version {
            continue;
        }
        let Ok(expected) = walker.of_rune(&state.name, &rune) else {
            continue;
        };
        if expected != state.store_hash {
            stale.push((state.name.clone(), expected));
        }
    }
    stale
}

struct Walker {
    target: String,
    build_env: String,
    cache: HashMap<String, String>,
    stack: Vec<String>,
    /// Capability-resolution context (provider index, preferences, installed names), built
    /// lazily the first time a dependency name has no literal rune. Most walks never need it.
    caps: Option<CapabilityContext>,
}

struct CapabilityContext {
    index: crate::solve::CapabilityIndex,
    preferences: std::collections::BTreeMap<String, String>,
    installed: std::collections::HashSet<String>,
}

impl Walker {
    fn new() -> Result<Self> {
        Self::with_target(&paths::target_triple())
    }

    fn with_target(target: &str) -> Result<Self> {
        Ok(Self {
            target: target.to_string(),
            // Compiled packages fold the host toolchain identity into their hash; fixed-output
            // packages ignore it. An absent toolchain hashes as empty (only fixed-output packages
            // can be addressed without one).
            build_env: toolchain::build_env_id().unwrap_or_default(),
            cache: HashMap::new(),
            stack: Vec::new(),
            caps: None,
        })
    }

    fn of_name(&mut self, name: &str) -> Result<String> {
        if let Some(hash) = self.cache.get(name) {
            return Ok(hash.clone());
        }
        if let Some(rune) = build::find_rune(name)? {
            return self.of_rune(name, &rune);
        }
        // No literal rune: resolve the name as a capability to a concrete provider, mirroring
        // the solver. The chosen provider folds into the hash, which is the point — a package
        // built against `gawk`'s awk is different content from one built against `mawk`'s, so
        // it gets a different address (and a prebuilt only substitutes for users whose
        // resolution matches the builder's).
        let Some(provider) = self.resolve_capability(name)? else {
            bail!(
                "no rune found for `{name}` and no package provides it; every runtime \
                 dependency must resolve to a rune or a capability provider"
            );
        };
        let hash = self.of_name(&provider)?;
        self.cache.insert(name.to_string(), hash.clone());
        Ok(hash)
    }

    /// Resolves a capability name to one provider package, in the solver's order — the
    /// `grm prefer` choice when it still provides the capability, else an installed provider,
    /// else the first provider — except every step here is deterministic (providers sorted by
    /// name) because the result feeds a content address. Returns `None` when nothing provides
    /// the capability.
    fn resolve_capability(&mut self, name: &str) -> Result<Option<String>> {
        if self.caps.is_none() {
            let installed = crate::install::installed_states()
                .unwrap_or_default()
                .into_iter()
                .map(|state| state.name)
                .collect();
            self.caps = Some(CapabilityContext {
                index: crate::solve::CapabilityIndex::build()?,
                preferences: crate::model::preferences::Preferences::load()
                    .unwrap_or_default()
                    .providers,
                installed,
            });
        }
        let caps = self.caps.as_ref().expect("capability context built above");
        let mut providers = caps.index.providers(name);
        if providers.is_empty() {
            return Ok(None);
        }
        providers.sort();
        if let Some(preferred) = caps.preferences.get(name)
            && providers.contains(preferred)
        {
            return Ok(Some(preferred.clone()));
        }
        if let Some(installed) = providers
            .iter()
            .find(|provider| caps.installed.contains(*provider))
        {
            return Ok(Some(installed.clone()));
        }
        Ok(Some(providers[0].clone()))
    }

    fn of_rune(&mut self, name: &str, rune: &Path) -> Result<String> {
        if let Some(hash) = self.cache.get(name) {
            return Ok(hash.clone());
        }
        if self.stack.iter().any(|entry| entry == name) {
            bail!("dependency cycle computing store hash for `{name}`");
        }
        // A rune may be a split group member (parent or split): its address derives from the
        // group's shared build, not from the rune alone. Computing the group caches every
        // member's hash, so each group is walked once. The group keys by *package* name —
        // `name` may be a rune path (`grm store-hash <file.rn>`), so look up by metadata.
        if let Some(group) = build::split::group_for(rune)? {
            let package_name = self.metadata(rune)?.name;
            let hashes = self.group_hashes(&disk_group_parts(&group)?)?;
            let hash = hashes.get(&package_name).cloned().with_context(|| {
                format!("split group for `{package_name}` did not yield its hash")
            })?;
            self.cache.insert(name.to_string(), hash.clone());
            return Ok(hash);
        }
        let metadata = self.metadata(rune)?;
        let rune_bytes =
            std::fs::read(rune).with_context(|| format!("read rune {}", rune.display()))?;
        self.of_rune_with_bytes(name, &metadata, &rune_bytes)
    }

    /// Computes the derived store hash of every member of a split group: external runtime
    /// deps (the union across all members, group-internal references excluded) resolve via
    /// the normal closure walk, fold into the shared group hash, and each member's address
    /// derives from that. All member hashes are cached before returning.
    fn group_hashes(&mut self, parts: &[GroupPart]) -> Result<BTreeMap<String, String>> {
        let parent = parts
            .iter()
            .find(|part| !part.metadata.is_split_member())
            .context("split group has no parent metadata")?;
        for part in parts {
            if self.stack.contains(&part.name) {
                bail!(
                    "dependency cycle computing store hash for split group member `{}`",
                    part.name
                );
            }
        }
        for part in parts {
            self.stack.push(part.name.clone());
        }
        let dep_hashes_result: Result<Vec<String>> = (|| {
            let mut dep_hashes = Vec::new();
            for part in parts {
                for dep in &part.metadata.deps.runtime {
                    if !dep.matches_platform(&self.target) {
                        continue;
                    }
                    if parts.iter().any(|member| member.name == dep.name) {
                        continue;
                    }
                    let hash = self.of_name(&dep.name)?;
                    if !dep_hashes.contains(&hash) {
                        dep_hashes.push(hash);
                    }
                }
            }
            Ok(dep_hashes)
        })();
        for _ in parts {
            self.stack.pop();
        }
        let dep_hashes = dep_hashes_result?;

        let rune_hashes: BTreeMap<String, String> = parts
            .iter()
            .map(|part| (part.name.clone(), part.rune_hash.clone()))
            .collect();
        let group_hash = store::split_group_hash(
            &parent.metadata,
            &rune_hashes,
            &dep_hashes,
            &self.target,
            &self.build_env,
        );
        let hashes: BTreeMap<String, String> = parts
            .iter()
            .map(|part| {
                (
                    part.name.clone(),
                    store::split_member_hash(&group_hash, &part.name),
                )
            })
            .collect();
        for (name, hash) in &hashes {
            self.cache.insert(name.clone(), hash.clone());
        }
        Ok(hashes)
    }

    fn of_rune_with_bytes(
        &mut self,
        name: &str,
        metadata: &crate::model::PackageMetadata,
        rune_bytes: &[u8],
    ) -> Result<String> {
        if self.stack.iter().any(|entry| entry == name) {
            bail!("dependency cycle computing store hash for `{name}`");
        }
        self.stack.push(name.to_string());
        // Platform-filtered, declaration order — exactly the dep list the resolver hands to
        // `store_hash_for_rune_with_deps`, so both paths address identically.
        let dep_hashes_result: Result<Vec<String>> = (|| {
            let mut dep_hashes = Vec::with_capacity(metadata.deps.runtime.len());
            for dep in &metadata.deps.runtime {
                if !dep.matches_platform(&self.target) {
                    continue;
                }
                dep_hashes.push(self.of_name(&dep.name)?);
            }
            Ok(dep_hashes)
        })();
        self.stack.pop();
        let dep_hashes = dep_hashes_result?;

        let hash = store::store_hash_for_metadata(
            metadata,
            rune_bytes,
            &dep_hashes,
            &self.target,
            &self.build_env,
        );
        self.cache.insert(name.to_string(), hash.clone());
        Ok(hash)
    }

    fn metadata(&self, rune: &Path) -> Result<crate::model::PackageMetadata> {
        let tome_name = build::tome_name_for_rune(rune)?;
        build::read_rune_metadata(rune, tome_name.as_deref())
    }
}
