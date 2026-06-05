//! Content-addressing a package over its dependency closure.
//!
//! A compiled package's store hash folds in the store hashes of its runtime dependencies, so the
//! whole closure is captured transitively (Nix-style). Computing that address requires resolving
//! each dependency to its rune and recursing — a pure walk over the rune graph, with no building or
//! installing. This is what `grm tome build` records in the index and what tests predict via the
//! `store-hash` seam. The installer derives the same address incrementally from the store hashes of
//! the dependencies it has already installed.

use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result, anyhow, bail};

use crate::{
    addendum, build,
    nu::runtime::{EmbeddedNuRuntime, RuneRuntime},
    paths, store, toolchain,
};

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

struct Walker {
    target: String,
    build_env: String,
    cache: HashMap<String, String>,
    stack: Vec<String>,
}

impl Walker {
    fn new() -> Result<Self> {
        Ok(Self {
            target: paths::target_triple(),
            // Compiled packages fold the host toolchain identity into their hash; fixed-output
            // packages ignore it. An absent toolchain hashes as empty (only fixed-output packages
            // can be addressed without one).
            build_env: toolchain::build_env_id().unwrap_or_default(),
            cache: HashMap::new(),
            stack: Vec::new(),
        })
    }

    fn of_name(&mut self, name: &str) -> Result<String> {
        if let Some(hash) = self.cache.get(name) {
            return Ok(hash.clone());
        }
        let rune = build::find_rune(name)?
            .ok_or_else(|| anyhow!("no rune found for `{name}`; every package must have a rune"))?;
        self.of_rune(name, &rune)
    }

    fn of_rune(&mut self, name: &str, rune: &Path) -> Result<String> {
        if self.stack.iter().any(|entry| entry == name) {
            bail!("dependency cycle computing store hash for `{name}`");
        }
        let metadata = self.metadata(rune)?;
        self.stack.push(name.to_string());
        let mut dep_hashes = Vec::with_capacity(metadata.deps.runtime.len());
        for dep in &metadata.deps.runtime {
            dep_hashes.push(self.of_name(&dep.name)?);
        }
        self.stack.pop();

        let rune_bytes =
            std::fs::read(rune).with_context(|| format!("read rune {}", rune.display()))?;
        let hash = store::store_hash_for_metadata(
            &metadata,
            &rune_bytes,
            &dep_hashes,
            &self.target,
            &self.build_env,
        );
        self.cache.insert(name.to_string(), hash.clone());
        Ok(hash)
    }

    fn metadata(&self, rune: &Path) -> Result<crate::model::PackageMetadata> {
        let mut metadata = EmbeddedNuRuntime
            .package_metadata(rune)
            .with_context(|| format!("read rune metadata {}", rune.display()))?;
        addendum::patched_package_metadata(
            &mut metadata,
            build::tome_name_for_rune(rune)?.as_deref(),
            rune,
        )
        .with_context(|| format!("apply addendums to {}", rune.display()))?;
        Ok(metadata)
    }
}
