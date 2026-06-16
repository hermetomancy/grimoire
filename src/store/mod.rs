//! Content-addressed store path computation.
//!
//! Every package lives in an immutable store directory keyed by a deterministic hash of its
//! inputs. Two kinds of package are addressed differently, mirroring Nix:
//!
//! - **Compiled packages** are *input-addressed*: the hash covers the sources, rune bytes, target,
//!   host build environment, build flags, and — transitively — the **store hashes of the runtime
//!   dependency closure**. Because each dependency contributes its own content address, a builder
//!   and an installer compute identical hashes as long as they resolve the same closure.
//! - **Fixed-output packages** (`fixed_output`, the x-bin / fetch-only case) are *output-addressed*:
//!   the hash covers only the declared source checksums (plus name/version/target). It deliberately
//!   excludes the build environment and the dependency closure, so the same fetched artifact lands
//!   at the same store path regardless of which host produced it — Nix's fixed-output derivation.

pub(crate) mod closure;

use crate::model::{PackageMetadata, Source};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

/// Computes a package's content address from its resolved metadata, the raw rune bytes, and the
/// store hashes of its resolved runtime dependency closure.
///
/// This is the single definition of a package's store hash: the builder records it (in the archive
/// and the published index entry) and the installer recomputes it to decide whether a prebuilt
/// substitute matches the inputs it would otherwise build. `dep_store_hashes` are the content
/// addresses of the package's direct runtime dependencies — each already folds in its own closure,
/// so listing the direct ones captures the whole transitive set.
pub fn store_hash_for_metadata(
    metadata: &PackageMetadata,
    rune_bytes: &[u8],
    dep_store_hashes: &[String],
    target: &str,
    build_env: &str,
) -> String {
    if metadata.fixed_output {
        fixed_output_hash(
            &metadata.name,
            &metadata.version,
            &metadata.sources_for(target),
            target,
        )
    } else {
        compiled_hash(
            &metadata.name,
            &metadata.version,
            &metadata.sources_for(target),
            &hash_bytes(rune_bytes),
            dep_store_hashes,
            &metadata.build_flags,
            target,
            build_env,
        )
    }
}

/// The input-addressed hash of a compiled package: sources, rune, target, build environment, build
/// flags, and the content addresses of the runtime dependency closure.
// Each argument is a distinct hash input; grouping them into a struct would only obscure that the
// signature *is* the list of things a compiled store path depends on. Prefer [`store_hash_for_metadata`].
#[allow(clippy::too_many_arguments)]
fn compiled_hash(
    name: &str,
    version: &str,
    sources: &BTreeMap<String, Source>,
    rune_hash: &str,
    dep_store_hashes: &[String],
    build_flags: &BTreeMap<String, String>,
    target: &str,
    build_env: &str,
) -> String {
    let mut hasher = Sha256::new();

    hasher.update(b"grimoire-store-v4\n");
    hasher.update(name.as_bytes());
    hasher.update(b"\0");
    hasher.update(version.as_bytes());
    hasher.update(b"\0");

    hash_sources(&mut hasher, sources);

    hasher.update(b"rune\0");
    hasher.update(rune_hash.as_bytes());
    hasher.update(b"\0");

    hasher.update(b"target\0");
    hasher.update(target.as_bytes());
    hasher.update(b"\0");

    hasher.update(b"build_env\0");
    hasher.update(build_env.as_bytes());
    hasher.update(b"\0");

    hasher.update(b"build_flags\0");
    for (key, value) in build_flags.iter() {
        hasher.update(key.as_bytes());
        hasher.update(b"=");
        hasher.update(value.as_bytes());
        hasher.update(b"\0");
    }

    // Dependencies fold in by content address (sorted for determinism), so the whole closure is
    // captured transitively without depending on resolved version strings.
    hasher.update(b"deps\0");
    let mut deps: Vec<&str> = dep_store_hashes.iter().map(String::as_str).collect();
    deps.sort_unstable();
    for dep in deps {
        hasher.update(dep.as_bytes());
        hasher.update(b"\0");
    }

    truncate(hasher)
}

/// The output-addressed hash of a fixed-output (fetch-only) package: name, version, target, and the
/// declared source checksums. Excludes the build environment and the dependency closure so the same
/// artifact resolves to the same store path on every host.
fn fixed_output_hash(
    name: &str,
    version: &str,
    sources: &BTreeMap<String, Source>,
    target: &str,
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"grimoire-fixed-output-v2\0");
    hasher.update(name.as_bytes());
    hasher.update(b"\0");
    hasher.update(version.as_bytes());
    hasher.update(b"\0");
    hasher.update(b"target\0");
    hasher.update(target.as_bytes());
    hasher.update(b"\0");
    hash_sources(&mut hasher, sources);
    truncate(hasher)
}

/// The shared input-address of a split group's single build: the parent's compiled-hash
/// inputs, with the rune identity widened to every member rune (a glob edit in any member
/// legitimately changes every member's content, including the parent's remainder) and the
/// dependency fold widened to the union of all members' *external* runtime dep hashes
/// (references between group members are the build itself, not inputs to it).
pub fn split_group_hash(
    parent: &PackageMetadata,
    group_rune_hashes: &BTreeMap<String, String>,
    external_dep_store_hashes: &[String],
    target: &str,
    build_env: &str,
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"grimoire-split-group-v1\0");
    for (name, rune_hash) in group_rune_hashes {
        hasher.update(name.as_bytes());
        hasher.update(b"\0");
        hasher.update(rune_hash.as_bytes());
        hasher.update(b"\0");
    }
    let combined_rune_hash = format!("{:x}", hasher.finalize());

    compiled_hash(
        &parent.name,
        &parent.version,
        &parent.sources_for(target),
        &combined_rune_hash,
        external_dep_store_hashes,
        &parent.build_flags,
        target,
        build_env,
    )
}

/// A group member's store hash, derived from the group's shared input-address. Every member
/// (the parent included) is addressed this way, so rebuilding the group moves all members to
/// new addresses together — they are one artifact set.
pub fn split_member_hash(group_hash: &str, member: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"grimoire-split-member-v1\0");
    hasher.update(group_hash.as_bytes());
    hasher.update(b"\0");
    hasher.update(member.as_bytes());
    hasher.update(b"\0");
    truncate(hasher)
}

fn hash_sources(hasher: &mut Sha256, sources: &BTreeMap<String, Source>) {
    hasher.update(b"sources\0");
    for (source_name, source) in sources.iter() {
        hasher.update(source_name.as_bytes());
        hasher.update(b"\0");
        hasher.update(source.url.as_bytes());
        hasher.update(b"\0");
        hasher.update(source.sha256.as_bytes());
        hasher.update(b"\0");
    }
}

/// Truncates the SHA-256 to 64 bits (16 hex). The store path also carries the package
/// `name-version`, so the effective collision domain is two *different* input sets that share the
/// same name **and** version (e.g. a rune that drifted colliding with its pre-drift address) — a
/// 2^32 birthday bound within a single package, which a realistic catalog never approaches. The
/// width is a deliberate path-length/readability trade-off; widen the slice here (and bump the
/// `grimoire-store-v*` tag) if the domain ever warrants it.
fn truncate(hasher: Sha256) -> String {
    let full_hash = format!("{:x}", hasher.finalize());
    full_hash[..16].to_string()
}

/// Formats a store path basename: `<hash>-<name>-<version>`.
pub fn store_path_basename(hash: &str, name: &str, version: &str) -> String {
    format!("{}-{}-{}", hash, name, version)
}

/// Hashes raw bytes and returns the hex string.
pub fn hash_bytes(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Deps;

    fn metadata(name: &str, fixed_output: bool) -> PackageMetadata {
        PackageMetadata {
            name: name.to_string(),
            version: "1.0.0".to_string(),
            target: None,
            store_path: None,
            targets: Vec::new(),
            fixed_output,
            summary: None,
            bins: BTreeMap::new(),
            sources: BTreeMap::from([(
                "main".to_string(),
                Source {
                    url: "https://example.com/src.tar.gz".to_string(),
                    sha256: "sha256:abc123".to_string(),
                    platform: None,
                    host_libc: None,
                },
            )]),
            deps: Deps::default(),
            build_flags: BTreeMap::new(),
            provides: Vec::new(),
            libs: Vec::new(),
            notes: Vec::new(),
            upstream_version: None,
            conflicts: Vec::new(),
            replaces: Vec::new(),
            split_from: None,
            files: Vec::new(),
        }
    }

    fn hash(meta: &PackageMetadata, deps: &[String], build_env: &str) -> String {
        store_hash_for_metadata(meta, b"rune", deps, "linux-x86_64-gnu", build_env)
    }

    #[test]
    fn hash_is_deterministic() {
        let meta = metadata("hello", false);
        let deps = vec!["aaaa".to_string()];
        let h1 = hash(&meta, &deps, "cc: clang 17.0.0");
        let h2 = hash(&meta, &deps, "cc: clang 17.0.0");
        assert_eq!(h1, h2);
        assert_eq!(h1.len(), 16);
    }

    #[test]
    fn build_env_changes_the_compiled_hash() {
        let meta = metadata("p", false);
        let gcc13 = hash(&meta, &[], "cc: gcc 13");
        let gcc14 = hash(&meta, &[], "cc: gcc 14");
        assert_ne!(
            gcc13, gcc14,
            "different host toolchains must resolve to different store paths"
        );
    }

    #[test]
    fn dependency_closure_changes_the_compiled_hash() {
        let meta = metadata("p", false);
        let without = hash(&meta, &[], "e");
        let with = hash(&meta, &["dephash".to_string()], "e");
        assert_ne!(
            without, with,
            "a different dependency closure must change the store hash"
        );
    }

    #[test]
    fn fixed_output_ignores_build_env_and_deps() {
        let meta = metadata("blob", true);
        let base = hash(&meta, &[], "cc: gcc 13");
        assert_eq!(
            base,
            hash(&meta, &[], "cc: gcc 14"),
            "a fixed-output package must not depend on the host toolchain"
        );
        assert_eq!(
            base,
            hash(&meta, &["dephash".to_string()], "cc: gcc 13"),
            "a fixed-output package must not depend on its dependency closure"
        );
    }

    #[test]
    fn different_names_yield_different_hashes() {
        let h1 = hash(&metadata("a", false), &[], "e");
        let h2 = hash(&metadata("b", false), &[], "e");
        assert_ne!(h1, h2);
    }

    #[test]
    fn split_members_derive_distinct_hashes_from_one_group() {
        let parent = metadata("core", false);
        let rune_hashes = BTreeMap::from([
            ("core".to_string(), "aaa".to_string()),
            ("extra".to_string(), "bbb".to_string()),
        ]);
        let group = split_group_hash(&parent, &rune_hashes, &[], "linux-x86_64-gnu", "env");
        let core = split_member_hash(&group, "core");
        let extra = split_member_hash(&group, "extra");
        assert_ne!(core, extra, "members must have distinct addresses");
        assert_eq!(core.len(), 16);
        assert_eq!(
            core,
            split_member_hash(&group, "core"),
            "derivation is deterministic"
        );
    }

    #[test]
    fn any_member_rune_edit_moves_the_whole_group() {
        let parent = metadata("core", false);
        let before = BTreeMap::from([
            ("core".to_string(), "aaa".to_string()),
            ("extra".to_string(), "bbb".to_string()),
        ]);
        let after = BTreeMap::from([
            ("core".to_string(), "aaa".to_string()),
            // e.g. the member's `files` globs changed: the parent's remainder changes too.
            ("extra".to_string(), "ccc".to_string()),
        ]);
        let g1 = split_group_hash(&parent, &before, &[], "linux-x86_64-gnu", "env");
        let g2 = split_group_hash(&parent, &after, &[], "linux-x86_64-gnu", "env");
        assert_ne!(g1, g2);
        assert_ne!(
            split_member_hash(&g1, "core"),
            split_member_hash(&g2, "core"),
            "a member edit must move the parent's address as well"
        );
    }

    #[test]
    fn external_deps_fold_into_the_group_hash() {
        let parent = metadata("core", false);
        let rune_hashes = BTreeMap::from([("core".to_string(), "aaa".to_string())]);
        let without = split_group_hash(&parent, &rune_hashes, &[], "linux-x86_64-gnu", "env");
        let with = split_group_hash(
            &parent,
            &rune_hashes,
            &["dephash".to_string()],
            "linux-x86_64-gnu",
            "env",
        );
        assert_ne!(without, with);
    }
}
