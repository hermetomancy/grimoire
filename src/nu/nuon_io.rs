//! Reading and writing NUON documents through the Nushell engine. All of Grimoire's on-disk
//! state — package state, indexes, the lockfile — is NUON, parsed to and serialized from
//! `nu_protocol::Value` here so the data layer stays declarative and inert (AGENTS.md §4).

use anyhow::{Context, Result, anyhow};
use nu_protocol::{Value, engine::EngineState};
use std::{fs, io::Write, path::Path};

pub fn read_nuon(path: &Path) -> Result<Value> {
    let input =
        fs::read_to_string(path).with_context(|| format!("read NUON from {}", path.display()))?;
    parse_nuon(&input)
}

pub fn parse_nuon(input: &str) -> Result<Value> {
    nuon::from_nuon(input, None).map_err(|err| anyhow!("parse NUON: {err}"))
}

/// Writes NUON state atomically: the contents are written to a temporary file in the
/// destination directory and then renamed into place, so a reader never observes a
/// partially written state file.
pub fn write_nuon(path: &Path, value: &Value) -> Result<()> {
    let contents = to_nuon_string(value)?;
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let mut temp = tempfile::Builder::new()
        .prefix(".grimoire-nuon-")
        .tempfile_in(parent)?;
    temp.write_all(contents.as_bytes())?;
    temp.flush()?;
    temp.persist(path).map_err(|err| anyhow!(err.to_string()))?;
    Ok(())
}

pub fn to_nuon_string(value: &Value) -> Result<String> {
    let engine_state = EngineState::new();
    nuon::to_nuon(&engine_state, value, nuon::ToNuonConfig::default())
        .map_err(|err| anyhow!(err.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_grimoire_package_metadata() -> Result<()> {
        let value = parse_nuon(
            r#"{
                format: 1
                name: hello
                version: "0.1.0"
                target: macos-aarch64-darwin
                bins: { hello: "bin/hello" }
            }"#,
        )?;

        let Value::Record { val, .. } = value else {
            panic!("expected record");
        };

        assert_eq!(
            val.get("name").and_then(|value| value.as_str().ok()),
            Some("hello")
        );
        assert!(matches!(val.get("bins"), Some(Value::Record { .. })));
        Ok(())
    }
}
