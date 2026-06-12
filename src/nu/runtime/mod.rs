//! Evaluating `.rn` definitions and running build steps in the embedded Nushell engine.
//!
//! The [`RuneRuntime`] trait exposes reading package/tome metadata and executing a rune's `build`
//! function against a prepared context; [`EmbeddedNuRuntime`] is the in-process implementation.
//! Runes are evaluated, not shelled out to — the engine is embedded (AGENTS.md §1).

use anyhow::{Context, Result};
use nu_protocol::{Record, Span, Value};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use crate::{
    fetch::FetchedSource,
    model::{BuildManifest, PackageMetadata, TomeManifest},
    nu::nuon_io,
    util::progress,
};

mod env;
mod eval;
mod meta_cache;

pub use env::*;
pub(crate) use eval::*;

#[derive(Debug, Default)]
pub struct EmbeddedNuRuntime;

impl EmbeddedNuRuntime {
    pub fn package_metadata(&self, rune: &Path) -> Result<PackageMetadata> {
        PackageMetadata::from_value(meta_cache::cached_package_const(rune)?, false)
            .with_context(|| format!("parse package metadata from {}", rune.display()))
    }

    /// Reads package metadata from rune source already in memory (an archive-embedded group
    /// rune). `label` stands in for the file path in errors.
    pub fn package_metadata_from_bytes(
        &self,
        source: &[u8],
        label: &str,
    ) -> Result<PackageMetadata> {
        PackageMetadata::from_value(exported_const_from_bytes(source, label, "package")?, false)
            .with_context(|| format!("parse package metadata from {label}"))
    }

    pub fn tome_manifest(&self, tome: &Path) -> Result<TomeManifest> {
        TomeManifest::from_value(exported_const(tome, "tome")?)
            .with_context(|| format!("parse tome manifest from {}", tome.display()))
    }

    pub fn build(
        &self,
        rune: &Path,
        dirs: &BuildDirs,
        sources: &BTreeMap<String, FetchedSource>,
        build_flags: &BTreeMap<String, String>,
        env: &BuildEnv,
    ) -> Result<Option<BuildManifest>> {
        let rune = rune
            .canonicalize()
            .with_context(|| format!("resolve rune {}", rune.display()))?;
        let package_dir = dirs
            .package_dir
            .canonicalize()
            .with_context(|| format!("resolve package dir {}", dirs.package_dir.display()))?;
        let final_prefix = normalize_final_prefix(&dirs.final_prefix);
        let work_dir = dirs
            .work_dir
            .canonicalize()
            .with_context(|| format!("resolve work dir {}", dirs.work_dir.display()))?;

        let host_tool_dir = prepare_host_tool_dir(work_dir.as_path(), &env.host_tools)?;
        let path_entries = build_path_entries(env, host_tool_dir.as_deref());
        let path = build_path_string(&path_entries);
        let sandbox_env = sandbox_env_vars(&work_dir, &env.extra_env)?;
        let context = build_context(
            &package_dir,
            &final_prefix,
            &work_dir,
            sources,
            build_flags,
            path.as_deref(),
            &sandbox_env.context,
            &env.target,
        );
        let env_prefix = path_env_assignment(&path_entries)?;
        let source = format!(
            "{env_prefix}use {} build\nbuild {}\n",
            nuon_io::to_nuon_string(&Value::string(rune.display().to_string(), Span::unknown()))?,
            nuon_io::to_nuon_string(&context)?,
        );

        let maybe_value = eval_nu_source(
            &source,
            Some(&format!("grimoire-build-{}", rune.display())),
            package_dir.parent().unwrap_or(&package_dir),
            path.as_deref(),
            &sandbox_env.process,
            dirs.log_file.clone(),
        )?;

        match maybe_value {
            Some(value) => BuildManifest::from_value(value).map(Some),
            None => Ok(None),
        }
    }
}

/// Builds the inert `ctx` record passed to a rune's `build` function. Source paths are the
/// already-fetched, checksum-verified cache locations (AGENTS.md §10.1).
#[allow(clippy::too_many_arguments)]
fn build_context(
    package_dir: &Path,
    final_prefix: &Path,
    work_dir: &Path,
    sources: &BTreeMap<String, FetchedSource>,
    build_flags: &BTreeMap<String, String>,
    path: Option<&str>,
    extra_env: &[(String, String)],
    target: &str,
) -> Value {
    let span = Span::unknown();
    let mut ctx = Record::new();
    ctx.push("package_dir", path_value(package_dir));
    ctx.push("work_dir", path_value(work_dir));
    // `prefix` is the final intended location the package should bake into configure-time
    // metadata. `package_dir` remains the staging root used as DESTDIR.
    ctx.push("prefix", path_value(final_prefix));
    ctx.push("store_path", path_value(final_prefix));
    ctx.push("target", Value::string(target, span));

    let mut sources_record = Record::new();
    for (name, source) in sources {
        let mut entry = Record::new();
        entry.push("path", path_value(&source.path));
        match &source.extracted_dir {
            Some(dir) => entry.push("dir", path_value(dir)),
            None => entry.push("dir", Value::nothing(span)),
        }
        entry.push("url", Value::string(&source.url, span));
        entry.push("sha256", Value::string(&source.sha256, span));
        sources_record.push(name, Value::record(entry, span));
    }
    ctx.push("sources", Value::record(sources_record, span));

    let mut flags = Record::new();
    for (name, value) in build_flags {
        flags.push(name, Value::string(value, span));
    }
    ctx.push("build_flags", Value::record(flags, span));

    ctx.push(
        "nproc",
        Value::int(
            std::thread::available_parallelism()
                .map(|n| n.get() as i64)
                .unwrap_or(4),
            span,
        ),
    );

    let mut env = Record::new();
    if let Some(path) = path {
        env.push("PATH", Value::string(path, span));
    }
    env.push(
        "GRIMOIRE_VERBOSITY",
        Value::string(progress::verbosity_name(), span),
    );
    for (key, value) in extra_env {
        env.push(key, Value::string(value, span));
    }
    ctx.push("env", Value::record(env, span));

    Value::record(ctx, span)
}

fn path_value(path: &Path) -> Value {
    Value::string(path.display().to_string(), Span::unknown())
}

fn normalize_final_prefix(path: &Path) -> PathBuf {
    // Normalize `.` and `..` without resolving symlinks, so the prefix stays
    // consistent even when the install root contains symlinks.
    path.components().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_package_metadata_from_rune() -> Result<()> {
        // Self-contained: a rune written to a temp dir, so the test does not depend on the
        // tome-example submodule being checked out (CI checkouts may omit submodules).
        let dir = tempfile::tempdir()?;
        let rune = dir.path().join("hello.rn");
        std::fs::write(
            &rune,
            "export const package = {\n  name: \"hello\"\n  version: \"0.1.0\"\n}\n\nexport def build [ctx] {\n  null\n}\n",
        )?;
        let runtime = EmbeddedNuRuntime;
        let metadata = runtime.package_metadata(&rune)?;

        assert_eq!(metadata.name, "hello");
        assert_eq!(metadata.version, "0.1.0");
        // bins are discovered at build time, not declared statically
        assert!(metadata.bins_for("linux-x86_64-musl").is_empty());
        Ok(())
    }
}
