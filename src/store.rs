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
        fixed_output_hash(&metadata.name, &metadata.version, &metadata.sources, target)
    } else {
        compiled_hash(
            &metadata.name,
            &metadata.version,
            &metadata.sources,
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

    hasher.update(b"grimoire-store-v3\n");
    hasher.update(name.as_bytes());
    hasher.update(b"\n");
    hasher.update(version.as_bytes());
    hasher.update(b"\n");

    hash_sources(&mut hasher, sources);

    hasher.update(b"rune\n");
    hasher.update(rune_hash.as_bytes());
    hasher.update(b"\n");

    hasher.update(b"target\n");
    hasher.update(target.as_bytes());
    hasher.update(b"\n");

    hasher.update(b"build_env\n");
    hasher.update(build_env.as_bytes());
    hasher.update(b"\n");

    hasher.update(b"build_flags\n");
    for (key, value) in build_flags.iter() {
        hasher.update(key.as_bytes());
        hasher.update(b"=");
        hasher.update(value.as_bytes());
        hasher.update(b"\n");
    }

    // Dependencies fold in by content address (sorted for determinism), so the whole closure is
    // captured transitively without depending on resolved version strings.
    hasher.update(b"deps\n");
    let mut deps: Vec<&str> = dep_store_hashes.iter().map(String::as_str).collect();
    deps.sort_unstable();
    for dep in deps {
        hasher.update(dep.as_bytes());
        hasher.update(b"\n");
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
    hasher.update(b"grimoire-fixed-output-v1\n");
    hasher.update(name.as_bytes());
    hasher.update(b"\n");
    hasher.update(version.as_bytes());
    hasher.update(b"\n");
    hasher.update(b"target\n");
    hasher.update(target.as_bytes());
    hasher.update(b"\n");
    hash_sources(&mut hasher, sources);
    truncate(hasher)
}

fn hash_sources(hasher: &mut Sha256, sources: &BTreeMap<String, Source>) {
    hasher.update(b"sources\n");
    for (source_name, source) in sources.iter() {
        hasher.update(source_name.as_bytes());
        hasher.update(b"\n");
        hasher.update(source.url.as_bytes());
        hasher.update(b"\n");
        hasher.update(source.sha256.as_bytes());
        hasher.update(b"\n");
    }
}

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
                },
            )]),
            deps: Deps::default(),
            build_flags: BTreeMap::new(),
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
}
