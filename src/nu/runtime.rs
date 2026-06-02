use anyhow::{Context, Result, anyhow};
use nu_protocol::{
    PipelineData, Record, Span, Value,
    debugger::WithoutDebug,
    engine::{Stack, StateWorkingSet},
};
use std::{collections::BTreeMap, fs, path::Path};

use crate::{
    fetch::FetchedSource,
    model::{PackageMetadata, TomeManifest},
    nu::nuon_io,
};

pub trait RuneRuntime {
    fn package_metadata(&self, rune: &Path) -> Result<PackageMetadata>;
    fn tome_manifest(&self, tome: &Path) -> Result<TomeManifest>;
    fn build(
        &self,
        rune: &Path,
        package_dir: &Path,
        work_dir: &Path,
        sources: &BTreeMap<String, FetchedSource>,
    ) -> Result<()>;
}

#[derive(Debug, Default)]
pub struct EmbeddedNuRuntime;

impl RuneRuntime for EmbeddedNuRuntime {
    fn package_metadata(&self, rune: &Path) -> Result<PackageMetadata> {
        PackageMetadata::from_value(exported_const(rune, "package")?, false)
    }

    fn tome_manifest(&self, tome: &Path) -> Result<TomeManifest> {
        TomeManifest::from_value(exported_const(tome, "tome")?)
    }

    fn build(
        &self,
        rune: &Path,
        package_dir: &Path,
        work_dir: &Path,
        sources: &BTreeMap<String, FetchedSource>,
    ) -> Result<()> {
        let rune = rune
            .canonicalize()
            .with_context(|| format!("resolve rune {}", rune.display()))?;
        let package_dir = package_dir
            .canonicalize()
            .with_context(|| format!("resolve package dir {}", package_dir.display()))?;
        let work_dir = work_dir
            .canonicalize()
            .with_context(|| format!("resolve work dir {}", work_dir.display()))?;

        let context = build_context(&package_dir, &work_dir, sources);
        let source = format!(
            "use {} build\nbuild {}\n",
            nuon_string(&rune.display().to_string())?,
            nuon_io::to_nuon_string(&context)?,
        );

        eval_nu_source(
            &source,
            Some(&format!("grimoire-build-{}", rune.display())),
            package_dir.parent().unwrap_or(&package_dir),
        )
    }
}

/// Builds the inert `ctx` record passed to a rune's `build` function. Source paths are the
/// already-fetched, checksum-verified cache locations (AGENTS.md §5.1).
fn build_context(
    package_dir: &Path,
    work_dir: &Path,
    sources: &BTreeMap<String, FetchedSource>,
) -> Value {
    let span = Span::unknown();
    let mut ctx = Record::new();
    ctx.push("package_dir", path_value(package_dir));
    ctx.push("work_dir", path_value(work_dir));
    // Grimoire installs are relocatable and user-local: the install prefix a rune should
    // configure against is the package's own staging dir, not a system path like `/usr`.
    ctx.push("prefix", path_value(package_dir));

    let mut sources_record = Record::new();
    for (name, source) in sources {
        let mut entry = Record::new();
        entry.push("path", path_value(&source.path));
        entry.push("url", Value::string(&source.url, span));
        entry.push("sha256", Value::string(&source.sha256, span));
        sources_record.push(name, Value::record(entry, span));
    }
    ctx.push("sources", Value::record(sources_record, span));

    Value::record(ctx, span)
}

fn path_value(path: &Path) -> Value {
    Value::string(path.display().to_string(), Span::unknown())
}

fn exported_const(path: &Path, name: &str) -> Result<Value> {
    let source = fs::read(path).with_context(|| format!("read {}", path.display()))?;
    let mut engine_state = nu_cmd_lang::create_default_context();
    engine_state.add_env_var("PWD".to_string(), Value::test_string("."));
    let mut working_set = StateWorkingSet::new(&engine_state);
    nu_parser::parse(&mut working_set, path.to_str(), &source, false);

    if let Some(err) = working_set.parse_errors.first() {
        return Err(anyhow!("could not parse {}: {err}", path.display()));
    }

    let var_id = working_set
        .find_variable(name.as_bytes())
        .ok_or_else(|| anyhow!("{} does not export const `{name}`", path.display()))?;
    let value = working_set.get_constant(var_id).map_err(|err| {
        anyhow!(
            "could not read exported const `{name}` from {}: {err}",
            path.display()
        )
    })?;

    Ok(value.clone())
}

fn eval_nu_source(source: &str, source_name: Option<&str>, cwd: &Path) -> Result<()> {
    let mut engine_state =
        nu_command::add_shell_command_context(nu_cmd_lang::create_default_context());
    engine_state.add_env_var(
        "PWD".to_string(),
        Value::string(cwd.display().to_string(), nu_protocol::Span::unknown()),
    );
    let mut stack = Stack::new();

    let (block, delta) = {
        let mut working_set = StateWorkingSet::new(&engine_state);
        let block = nu_parser::parse(&mut working_set, source_name, source.as_bytes(), false);

        if let Some(err) = working_set.parse_errors.first() {
            return Err(anyhow!(
                "could not parse embedded Nushell build runner: {err}"
            ));
        }
        if let Some(err) = working_set.compile_errors.first() {
            return Err(anyhow!(
                "could not compile embedded Nushell build runner: {err}"
            ));
        }

        (block, working_set.render())
    };

    engine_state
        .merge_delta(delta)
        .context("merge embedded Nushell build runner")?;
    let output = nu_engine::eval_block::<WithoutDebug>(
        &engine_state,
        &mut stack,
        &block,
        PipelineData::empty(),
    )
    .context("evaluate embedded Nushell build runner")?;

    if let PipelineData::Value(Value::Error { error, .. }, ..) = output.body {
        return Err(anyhow!("embedded Nushell build failed: {error}"));
    }

    Ok(())
}

/// Renders a string as a NUON string literal so it can be safely interpolated into the
/// generated Nushell build runner. Routed through `nuon_io` per the single-NUON-layer rule.
fn nuon_string(value: &str) -> Result<String> {
    nuon_io::to_nuon_string(&Value::string(value, nu_protocol::Span::unknown()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use nu_protocol::engine::StateWorkingSet;
    use std::path::Path;

    #[test]
    fn probe_export_const_parse() {
        let mut engine_state = nu_cmd_lang::create_default_context();
        engine_state.add_env_var("PWD".to_string(), nu_protocol::Value::test_string("."));
        let mut working_set = StateWorkingSet::new(&engine_state);
        let block = nu_parser::parse(
            &mut working_set,
            None,
            b"export const package = { name: hello, version: \"0.1.0\", bins: { hello: \"bin/hello\" } }\n",
            false,
        );

        assert!(
            working_set.parse_errors.is_empty(),
            "{:?}",
            working_set.parse_errors
        );
        assert_eq!(block.pipelines.len(), 1);
        let var_id = working_set
            .find_variable(b"package")
            .expect("package variable");
        let value = working_set.get_constant(var_id).expect("package const");
        eprintln!("{value:#?}");
    }

    #[test]
    fn reads_package_metadata_from_rune() {
        let runtime = EmbeddedNuRuntime;
        let metadata = runtime
            .package_metadata(Path::new("example/runes/hello.rn"))
            .expect("package metadata");

        assert_eq!(metadata.name, "hello");
        assert_eq!(metadata.version, "0.1.0");
        assert_eq!(
            metadata.bins.get("hello").map(String::as_str),
            Some("bin/hello")
        );
    }
}
