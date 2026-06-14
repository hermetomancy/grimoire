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

/// Writes NUON state atomically and durably: the contents are written to a temporary file in the
/// destination directory, fsync'd, then renamed into place, so a reader never observes a partial
/// state file and a crash never leaves a present-but-empty one. The destination directory is
/// fsync'd after the rename so the new entry itself is durable (AGENTS.md §9).
pub fn write_nuon(path: &Path, value: &Value) -> Result<()> {
    let contents = to_nuon_string(value)?;
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let mut temp = tempfile::Builder::new()
        .prefix(".grimoire-nuon-")
        .tempfile_in(parent)?;
    temp.write_all(contents.as_bytes())?;
    temp.flush()?;
    // The data must reach disk before the rename, or a crash can leave the new name pointing at
    // an unwritten (zero-length) file.
    temp.as_file()
        .sync_all()
        .with_context(|| format!("fsync staged NUON for {}", path.display()))?;
    temp.persist(path).map_err(|err| anyhow!(err.to_string()))?;
    crate::util::fs_util::fsync_dir(parent)?;
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
