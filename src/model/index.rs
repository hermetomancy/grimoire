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

        Ok(Self {
            name,
            version,
            target,
            archive,
            archive_hash,
            runtime_deps,
            provides,
            libs,
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
        Value::record(record, Span::unknown())
    }
}
