//! Binary package repository indexes (`dist/index.nuon`): the prebuilt archives a tome
//! publishes, keyed by store hash.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

use anyhow::{Context, Result, bail};
use nu_protocol::{Record, Span, Value};

use super::*;

/// A binary package repository index (`index.nuon`): the set of pre-built archives a tome's
/// package repository offers. Read-only data — Grimoire reads it, never executes it (§4).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PackageIndex {
    pub entries: BTreeMap<String, IndexEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexEntry {
    pub name: String,
    pub version: String,
    pub target: String,
    /// Location of the `.tar.zst` archive: either a path relative to the package
    /// repository or an `http(s)` URL.
    pub archive: String,
    pub archive_hash: String,
    #[serde(default)]
    pub runtime_deps: Vec<Dependency>,
    /// Command names this package provides, discovered at build time and cached in the index
    /// so consumers can resolve capabilities without reading the rune.
    #[serde(default)]
    pub provides: Vec<String>,
    /// Library base names (e.g. "foo" for libfoo.so) discovered at build time.
    #[serde(default)]
    pub libs: Vec<String>,
    /// Installed packages this one cannot coexist with (mirrors the rune's `conflicts`).
    #[serde(default)]
    pub conflicts: Vec<String>,
    /// Package names this one supersedes (mirrors the rune's `replaces`).
    #[serde(default)]
    pub replaces: Vec<String>,
}

impl PackageIndex {
    pub fn from_value(value: Value) -> Result<Self> {
        let record = expect_record(value, "package index")?;
        let format = match record.get("format") {
            Some(Value::Int { val, .. }) => *val,
            Some(_) => bail!("package index field `format` must be an integer"),
            None => 1,
        };
        if format != 2 {
            bail!("unsupported package index format {format}; expected 2");
        }
        let entries_record = match record.get("entries") {
            Some(Value::Record { val, .. }) => val,
            Some(_) => bail!("package index field `entries` must be a record"),
            None => bail!("package index is missing required field `entries`"),
        };
        let mut entries = BTreeMap::new();
        for (hash, entry_value) in entries_record.iter() {
            let entry = IndexEntry::from_value(entry_value)
                .with_context(|| format!("parse index entry for hash `{hash}`"))?;
            entries.insert(hash.clone(), entry);
        }
        Ok(Self { entries })
    }

    /// Every entry for `name`/`target`, newest version first. The index may list several
    /// versions of a package so the resolver can pick one satisfying a requirement.
    /// Returns `(store_hash, entry)` pairs so the caller can associate the hash with the entry.
    pub fn candidates(&self, name: &str, target: &str) -> Vec<(&str, &IndexEntry)> {
        let mut entries: Vec<_> = self
            .entries
            .iter()
            .filter(|(_, entry)| entry.name == name && entry.target == target)
            .map(|(hash, entry)| (hash.as_str(), entry))
            .collect();
        entries.sort_by(|a, b| compare_versions(&b.1.version, &a.1.version));
        entries
    }

    /// Inserts `entry` at `hash`, replacing any existing entry with the same hash so
    /// rebuilding the same inputs updates in place.
    pub fn upsert(&mut self, hash: String, entry: IndexEntry) {
        self.entries.insert(hash, entry);
    }

    pub fn to_value(&self) -> Value {
        let mut record = Record::new();
        record.push("format", Value::int(2, Span::unknown()));
        let mut entries = Record::new();
        for (hash, entry) in &self.entries {
            entries.push(hash, entry.to_value());
        }
        record.push("entries", Value::record(entries, Span::unknown()));
        Value::record(record, Span::unknown())
    }
}

impl IndexEntry {
    fn from_value(value: &Value) -> Result<Self> {
        let Value::Record { val, .. } = value else {
            bail!("package index entry must be a record");
        };

        let name = required_field_string(val, "index entry", "name")?;
        let version = required_field_string(val, "index entry", "version")?;
        validate_package_name(&name)?;
        validate_package_version(&version)?;

        let target = required_field_string(val, "index entry", "target")?;
        if target.trim().is_empty() {
            bail!("index entry `{name}` has an empty target");
        }

        let archive = required_field_string(val, "index entry", "archive")?;
        validate_archive_location(&archive)?;
        let archive_hash = required_field_string(val, "index entry", "archive_hash")?;
        validate_sha256(&archive_hash, "index entry archive_hash")?;

        let runtime_deps = match val.get("runtime_deps") {
            Some(value) => parse_dependency_list(value, "index entry runtime_deps")?,
            None => Vec::new(),
        };

        let provides = optional_string_list(val, "provides")?;
        let libs = optional_string_list(val, "libs")?;
        let conflicts = optional_string_list(val, "conflicts")?;
        let replaces = optional_string_list(val, "replaces")?;

        Ok(Self {
            name,
            version,
            target,
            archive,
            archive_hash,
            runtime_deps,
            provides,
            libs,
            conflicts,
            replaces,
        })
    }

    pub fn to_value(&self) -> Value {
        let mut record = Record::new();
        record.push("name", Value::string(&self.name, Span::unknown()));
        record.push("version", Value::string(&self.version, Span::unknown()));
        record.push("target", Value::string(&self.target, Span::unknown()));
        record.push("archive", Value::string(&self.archive, Span::unknown()));
        record.push(
            "archive_hash",
            Value::string(&self.archive_hash, Span::unknown()),
        );
        record.push("runtime_deps", dependency_list_value(&self.runtime_deps));
        record.push("provides", string_list_value(&self.provides));
        record.push("libs", string_list_value(&self.libs));
        record.push("conflicts", string_list_value(&self.conflicts));
        record.push("replaces", string_list_value(&self.replaces));
        Value::record(record, Span::unknown())
    }
}

#[cfg(test)]
mod tests {
    use crate::{model::PackageIndex, nu::nuon_io};

    fn parse_index(contents: &str) -> anyhow::Result<PackageIndex> {
        PackageIndex::from_value(nuon_io::parse_nuon(contents)?)
    }

    #[test]
    fn reads_and_finds_entries() {
        let index = parse_index(
            "{\n  format: 2,\n  entries: {\n    \"deadbeefdeadbeef\": { name: \"hello\", version: \"1.0.0\", target: \"linux-x86_64-gnu\", archive: \"hello-1.0.0-linux-x86_64-gnu.tar.zst\", archive_hash: \"sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\", runtime_deps: [\"libc\"] }\n  }\n}\n",
        )
        .expect("parse index");
        assert_eq!(index.entries.len(), 1);

        let candidates = index.candidates("hello", "linux-x86_64-gnu");
        let (hash, entry) = candidates.first().expect("entry for current target");
        assert_eq!(*hash, "deadbeefdeadbeef");
        assert_eq!(entry.version, "1.0.0");
        assert_eq!(
            entry
                .runtime_deps
                .iter()
                .map(|dep| dep.name.as_str())
                .collect::<Vec<_>>(),
            vec!["libc"]
        );
        assert!(index.candidates("hello", "macos-aarch64-darwin").is_empty());
        assert!(index.candidates("missing", "linux-x86_64-gnu").is_empty());
    }

    #[test]
    fn rejects_unsafe_archive_path() {
        let err = parse_index(
            "{\n  format: 2,\n  entries: {\n    \"deadbeefdeadbeef\": { name: \"evil\", version: \"1.0.0\", target: \"linux-x86_64-gnu\", archive: \"../escape.tar.zst\", archive_hash: \"sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\" }\n  }\n}\n",
        )
        .unwrap_err();
        assert!(
            format!("{err:#}").contains("parent components"),
            "unexpected error: {err:#}"
        );
    }

    #[test]
    fn rejects_missing_entries_field() {
        assert!(parse_index("{ format: 2 }\n").is_err());
    }

    #[test]
    fn lookup_by_hash() {
        let index = parse_index(
            "{\n  format: 2,\n  entries: {\n    \"aaa\": { name: \"a\", version: \"1.0.0\", target: \"t\", archive: \"a.tar.zst\", archive_hash: \"sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\" }\n    \"bbb\": { name: \"b\", version: \"1.0.0\", target: \"t\", archive: \"b.tar.zst\", archive_hash: \"sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb\" }\n  }\n}\n",
        )
        .expect("parse index");
        assert!(index.entries.contains_key("aaa"));
        assert!(index.entries.contains_key("bbb"));
        assert!(!index.entries.contains_key("ccc"));
    }
}
