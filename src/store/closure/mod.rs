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
use semver::VersionReq;

use crate::{build, build::toolchain, store, util::paths};

mod capability;
mod stale;

use capability::CapabilityContext;
// Re-exported for callers that name the return type; not used inside this module.
#[allow(unused_imports)]
pub use stale::{StaleInstall, stale_installed};

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
    Walker::new()?.of_name(name, &VersionReq::STAR)
}

/// Like [`store_hash`], but for a specific rune file (e.g. a `grm build <path>`), so the exact rune
/// given is hashed rather than whatever `find_rune` would resolve for its name. `resolved` is the
/// known dependency closure (see [`installed_resolved`]); it only affects split-member addressing
/// and is ignored for ordinary packages, so callers without one pass an empty map.
pub fn store_hash_for_rune(rune: &Path, resolved: &BTreeMap<String, String>) -> Result<String> {
    let mut walker = Walker::new()?;
    walker.resolved = resolved.clone();
    let metadata = walker.metadata(rune)?;
    walker.of_rune(&metadata.name, rune)
}

/// Like [`store_hash_for_rune`], but for an explicit target triple instead of the host default.
pub fn store_hash_for_rune_with_target(
    rune: &Path,
    target: &str,
    resolved: &BTreeMap<String, String>,
) -> Result<String> {
    let mut walker = Walker::with_target(target)?;
    walker.resolved = resolved.clone();
    let metadata = walker.metadata(rune)?;
    walker.of_rune(&metadata.name, rune)
}

/// Computes the store hash from already-read rune bytes and metadata for an explicit target
/// triple, avoiding a temporary file when the bytes are already in memory. The re-index path
/// (`grm tome build --index`) addresses an archive against the target it was built for — read from
/// the archive's own metadata — not the indexing host's, so a cross-target build keeps the address
/// a consumer on that target reproduces (AGENTS §9.8). Split group members cannot be addressed
/// from a single rune; use [`split_member_hashes_with_target`], which takes the whole group.
pub fn store_hash_for_rune_bytes_with_target(
    rune_bytes: &[u8],
    metadata: &crate::model::PackageMetadata,
    target: &str,
) -> Result<String> {
    if metadata.is_split_member() {
        bail!(
            "`{}` is a split member; its store hash is derived from the whole group",
            metadata.name
        );
    }
    let mut walker = Walker::with_target(target)?;
    walker.of_rune_with_bytes(&metadata.name, metadata, rune_bytes)
}

/// The store hashes of every member of a split group, keyed by package name, for an explicit
/// target triple. `group` carries each member's metadata and raw rune bytes (parent included), as
/// read from disk or from a member archive's embedded `.grimoire/group/` copies. External deps
/// address against `resolved` (see [`installed_resolved`]) so the build reproduces the resolver's
/// expected address; pass an empty map to re-derive externals from runes (the re-index path).
pub fn split_member_hashes_with_target(
    group: &[(crate::model::PackageMetadata, Vec<u8>)],
    target: &str,
    resolved: &BTreeMap<String, String>,
) -> Result<BTreeMap<String, String>> {
    let mut walker = Walker::with_target(target)?;
    walker.resolved = resolved.clone();
    walker.group_hashes(&group_parts(group))
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
    resolved: &BTreeMap<String, String>,
) -> Result<String> {
    let metadata = build::read_rune_metadata(rune, build::tome_name_for_rune(rune)?.as_deref())?;

    // A split group member's address folds the *whole group's* external deps, which the
    // caller's positional list (the member's own deps) cannot supply. Address the group through
    // the closure walk, but hand it the resolver's chosen closure (`resolved`) so external deps
    // fold in at the versions the resolver actually picked — the same closure the builder
    // addresses against, so the resolver's predicted address equals the build's produced one.
    if let Some(group) = build::split::group_for(rune)? {
        let mut walker = Walker::with_target(target)?;
        walker.build_env = build_env.to_string();
        walker.resolved = resolved.clone();
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

struct Walker {
    target: String,
    build_env: String,
    cache: HashMap<String, String>,
    stack: Vec<String>,
    /// Resolver-chosen (or installed) store hashes keyed by package name. When a split group's
    /// external dependency appears here, its recorded address is folded in verbatim instead of
    /// being re-derived from the rune — collapsing split-member addressing onto the same closure
    /// the resolver/installer chose, so the address the resolver predicts equals the one the
    /// build produces (AGENTS §9.8). Empty for bare prediction/stale walks, where the closure
    /// walk is itself the canonical address.
    resolved: BTreeMap<String, String>,
    /// Capability-resolution context (provider index, preferences, installed names), built
    /// lazily the first time a dependency name has no literal rune. Most walks never need it.
    caps: Option<CapabilityContext>,
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
            resolved: BTreeMap::new(),
            caps: None,
        })
    }

    fn of_name(&mut self, name: &str, req: &VersionReq) -> Result<String> {
        if let Some(hash) = self.cache.get(name) {
            return Ok(hash.clone());
        }
        if let Some(rune) = build::find_rune(name)? {
            return self.of_rune(name, &rune);
        }
        // No rune, but a resolved package of its own: this is a binary-index-only package (an
        // x-bin published as a prebuilt with no source rune), not a capability. The resolver
        // treats any name with candidates as literal, so address it by its recorded hash here too
        // — otherwise a name that is *both* a binary-only package and a capability provided by
        // others would fold a provider's hash on this side and the package's on the resolver's,
        // diverging the dependent's address (AGENTS §9.8).
        if let Some(hash) = self.resolved.get(name) {
            let hash = hash.clone();
            self.cache.insert(name.to_string(), hash.clone());
            return Ok(hash);
        }
        // No literal rune: resolve the name as a capability to a concrete provider, mirroring
        // the solver — including the version requirement, so the provider chosen here is the one
        // the resolver picked. The chosen provider folds into the hash, which is the point — a
        // package built against `gawk`'s awk is different content from one built against `mawk`'s,
        // so it gets a different address (and a prebuilt only substitutes for users whose
        // resolution matches the builder's).
        let Some(provider) = self.resolve_capability(name, req)? else {
            bail!(
                "no rune found for `{name}` and no package provides it; every runtime \
                 dependency must resolve to a rune or a capability provider"
            );
        };
        // The provider is a literal package; resolving it needs no further req (it has a rune).
        let hash = self.of_name(&provider, &VersionReq::STAR)?;
        self.cache.insert(name.to_string(), hash.clone());
        Ok(hash)
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

    /// Resolves a split group's external dependency to a store hash, preferring the
    /// resolver/installer's chosen address (`self.resolved`) over re-deriving it from the rune.
    /// This is the single point where split-member addressing consumes resolver choices, so the
    /// member's hash folds external deps at the versions the resolver actually picked rather than
    /// an independent re-pick — the source of accidental address divergence (AGENTS §9.8). A
    /// capability is keyed in `resolved` by its concrete provider, so it is resolved to that
    /// provider the same deterministic way `of_name` does before the map is consulted. Anything
    /// absent from `resolved` (no plan in scope, or a dep outside the resolved set) falls back to
    /// the full closure walk, which is the canonical address when no resolved closure exists.
    fn external_hash(&mut self, name: &str, req: &VersionReq) -> Result<String> {
        if let Some(hash) = self.resolved.get(name) {
            return Ok(hash.clone());
        }
        if build::find_rune(name)?.is_none()
            && let Some(provider) = self.resolve_capability(name, req)?
            && let Some(hash) = self.resolved.get(&provider)
        {
            return Ok(hash.clone());
        }
        self.of_name(name, req)
    }

    /// Computes the derived store hash of every member of a split group: external runtime
    /// deps (the union across all members, group-internal references excluded) resolve via
    /// [`Self::external_hash`], fold into the shared group hash, and each member's address
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
                    let hash = self.external_hash(&dep.name, &dep.req)?;
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
                dep_hashes.push(self.of_name(&dep.name, &dep.req)?);
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Dependency, Deps, PackageMetadata};

    fn meta(name: &str, split_from: Option<&str>, runtime: Vec<Dependency>) -> PackageMetadata {
        PackageMetadata {
            name: name.to_string(),
            version: "1.0.0".to_string(),
            target: None,
            store_path: None,
            targets: Vec::new(),
            fixed_output: false,
            summary: None,
            bins: BTreeMap::new(),
            sources: BTreeMap::new(),
            deps: Deps {
                runtime,
                ..Deps::default()
            },
            build_flags: BTreeMap::new(),
            provides: Vec::new(),
            libs: Vec::new(),
            notes: Vec::new(),
            upstream_version: None,
            conflicts: Vec::new(),
            replaces: Vec::new(),
            split_from: split_from.map(str::to_string),
            files: split_from
                .map(|_| vec!["bin/extra*".to_string()])
                .unwrap_or_default(),
        }
    }

    fn part(metadata: PackageMetadata, rune_hash: &str) -> GroupPart {
        GroupPart {
            name: metadata.name.clone(),
            metadata,
            rune_hash: rune_hash.to_string(),
        }
    }

    /// A walker with an explicit resolved closure and no host probing, so split addressing is a
    /// pure function of its inputs when every external dep is covered by `resolved`.
    fn walker(resolved: BTreeMap<String, String>) -> Walker {
        Walker {
            target: "linux-x86_64-gnu".to_string(),
            build_env: "env".to_string(),
            cache: HashMap::new(),
            stack: Vec::new(),
            resolved,
            caps: None,
        }
    }

    /// A split group whose parent has a real *external* runtime dep (`libdep`); the member's only
    /// runtime dep is the parent (group-internal, excluded from the external union).
    fn group() -> Vec<GroupPart> {
        vec![
            part(
                meta("core", None, vec![Dependency::any("libdep")]),
                "PARENT_RUNE",
            ),
            part(
                meta("extra", Some("core"), vec![Dependency::any("core")]),
                "MEMBER_RUNE",
            ),
        ]
    }

    #[test]
    fn split_member_address_tracks_the_resolved_external_version() {
        // The member (and parent) address must fold whatever version the resolver chose for the
        // external dep — supplied via `resolved` — not an independent re-pick. Two different
        // resolved closures must yield two different addresses; the same closure, the same address.
        let a = walker(BTreeMap::from([(
            "libdep".to_string(),
            "DEPHASH_A".to_string(),
        )]))
        .group_hashes(&group())
        .expect("a resolved external dep needs no rune lookup");
        let b = walker(BTreeMap::from([(
            "libdep".to_string(),
            "DEPHASH_B".to_string(),
        )]))
        .group_hashes(&group())
        .expect("a resolved external dep needs no rune lookup");
        let a2 = walker(BTreeMap::from([(
            "libdep".to_string(),
            "DEPHASH_A".to_string(),
        )]))
        .group_hashes(&group())
        .unwrap();
        assert_eq!(
            a, a2,
            "the same resolved closure derives the same addresses"
        );
        assert_ne!(
            a["core"], b["core"],
            "the parent's address must track the resolved external version"
        );
        assert_ne!(
            a["extra"], b["extra"],
            "the member's address must track the resolved external version"
        );
    }

    #[test]
    fn split_member_address_is_the_canonical_derivation_over_resolved_deps() {
        // Pin that `group_hashes` folds *exactly* the resolved external hash through the public
        // split primitives — there is one canonical derivation, and the resolved closure is its
        // only variable input. A second code path that derived the address differently would fail
        // this equality.
        let parent = meta("core", None, vec![Dependency::any("libdep")]);
        let hashes = walker(BTreeMap::from([(
            "libdep".to_string(),
            "DEPHASH".to_string(),
        )]))
        .group_hashes(&group())
        .unwrap();

        let rune_hashes = BTreeMap::from([
            ("core".to_string(), "PARENT_RUNE".to_string()),
            ("extra".to_string(), "MEMBER_RUNE".to_string()),
        ]);
        let group_hash = store::split_group_hash(
            &parent,
            &rune_hashes,
            &["DEPHASH".to_string()],
            "linux-x86_64-gnu",
            "env",
        );
        assert_eq!(
            hashes["core"],
            store::split_member_hash(&group_hash, "core")
        );
        assert_eq!(
            hashes["extra"],
            store::split_member_hash(&group_hash, "extra")
        );
    }
}
