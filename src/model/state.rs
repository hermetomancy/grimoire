//! Installed-package state (`state/packages/<name>.nuon`) and the lockfile snapshot
//! (`grimoire.lock.nuon`) built from it.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

use anyhow::Result;
use nu_protocol::{Record, Span, Value};

use super::*;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PackageState {
    pub name: String,
    pub version: String,
    pub target: Option<String>,
    pub archive_hash: String,
    /// The content address (store hash) of this installed package. Folded into the store hash of
    /// any package that depends on it, so the dependency closure is captured transitively.
    pub store_hash: String,
    /// The content-addressed store path this package was installed into, e.g.
    /// `/grm/store/<hash>-<name>-<version>`.
    pub store_path: String,
    #[serde(default)]
    pub bins: BTreeMap<String, String>,
    #[serde(default)]
    pub runtime_deps: Vec<String>,
    #[serde(default)]
    pub build_deps: Vec<String>,
    /// Verified source artifacts that produced this package, keyed by the source name the rune
    /// declared, mapped to the `sha256` each was checked against (empty for binary installs).
    #[serde(default)]
    pub source_hashes: BTreeMap<String, String>,
    /// `true` when the user has held this package back from upgrades via `grm hold`. The
    /// version in `state/packages/<name>.nuon` is what `grm upgrade` will skip; clear with
    /// `grm unhold`.
    #[serde(default)]
    pub held: bool,
    /// `true` when the user asked for this package by name; `false` when the solver pulled it
    /// in as a dependency. Non-requested packages are candidates for `grm autoremove` once
    /// nothing references them.
    #[serde(default)]
    pub requested: bool,
    /// Command names this package provides (discovered at build time). Used for capability
    /// resolution when the solver reads from indexes or installed state.
    #[serde(default)]
    pub provides: Vec<String>,
    /// Library base names (e.g. "foo" for libfoo.so) discovered at build time.
    #[serde(default)]
    pub libs: Vec<String>,
    /// Post-install notes carried over from the package metadata, replayed by `grm info`.
    #[serde(default)]
    pub notes: Vec<String>,
}

impl PackageState {
    pub fn from_value(value: Value) -> Result<Self> {
        let record = expect_record(value, "package state")?;
        let name = required_field_string(&record, "package state", "name")?;
        let version = required_field_string(&record, "package state", "version")?;
        let target = optional_string(&record, "target")?;
        let archive_hash = required_field_string(&record, "package state", "archive_hash")?;
        validate_sha256(&archive_hash, "package state archive_hash")?;
        let store_hash = required_field_string(&record, "package state", "store_hash")?;
        let store_path = required_field_string(&record, "package state", "store_path")?;
        let bins = match record.get("bins") {
            Some(value) => expect_string_map(value, "package field `bins`")?,
            None => BTreeMap::new(),
        };
        let runtime_deps = optional_string_list(&record, "runtime_deps")?;
        let build_deps = optional_string_list(&record, "build_deps")?;
        let source_hashes = match record.get("source_hashes") {
            Some(Value::Nothing { .. }) | None => BTreeMap::new(),
            Some(value) => expect_string_map(value, "field `source_hashes`")?,
        };
        let held = optional_bool(&record, "held")?.unwrap_or(false);
        let requested = optional_bool(&record, "requested")?.unwrap_or(false);
        let provides = optional_string_list(&record, "provides")?;
        let libs = optional_string_list(&record, "libs")?;
        let notes = optional_string_list(&record, "notes")?;

        Ok(Self {
            name,
            version,
            target,
            archive_hash,
            store_hash,
            store_path,
            bins,
            runtime_deps,
            build_deps,
            source_hashes,
            held,
            requested,
            provides,
            libs,
            notes,
        })
    }

    pub fn to_value(&self) -> Value {
        let mut record = Record::new();
        record.push("name", Value::string(&self.name, Span::unknown()));
        record.push("version", Value::string(&self.version, Span::unknown()));
        match &self.target {
            Some(target) => record.push("target", Value::string(target, Span::unknown())),
            None => record.push("target", Value::nothing(Span::unknown())),
        }
        record.push(
            "archive_hash",
            Value::string(&self.archive_hash, Span::unknown()),
        );
        record.push(
            "store_hash",
            Value::string(&self.store_hash, Span::unknown()),
        );
        record.push(
            "store_path",
            Value::string(&self.store_path, Span::unknown()),
        );

        let mut bins = Record::new();
        for (name, path) in &self.bins {
            bins.push(name, Value::string(path, Span::unknown()));
        }
        record.push("bins", Value::record(bins, Span::unknown()));
        record.push("runtime_deps", string_list_value(&self.runtime_deps));
        record.push("build_deps", string_list_value(&self.build_deps));
        record.push("source_hashes", string_map_value(&self.source_hashes));
        if self.held {
            record.push("held", Value::bool(true, Span::unknown()));
        }
        record.push("requested", Value::bool(self.requested, Span::unknown()));
        record.push("provides", string_list_value(&self.provides));
        record.push("libs", string_list_value(&self.libs));
        record.push("notes", string_list_value(&self.notes));
        Value::record(record, Span::unknown())
    }
}

/// The reproducible install snapshot written to `grimoire.lock.nuon`. Built from the recorded
/// installed package state and configured tome state, so it can be regenerated deterministically
/// after any install or removal. Write-only for now (Grimoire emits it; nothing reads it back).
pub struct LockFile {
    pub target: String,
    pub tomes: Vec<TomeState>,
    pub addendums: Vec<AddendumState>,
    pub packages: Vec<PackageState>,
}

impl LockFile {
    pub fn new(
        target: String,
        tomes: Vec<TomeState>,
        addendums: Vec<AddendumState>,
        packages: Vec<PackageState>,
    ) -> Self {
        Self {
            target,
            tomes,
            addendums,
            packages,
        }
    }

    pub fn to_value(&self) -> Value {
        let mut record = Record::new();
        record.push("version", Value::int(1, Span::unknown()));
        record.push("target", self.target_value());

        let tomes = self
            .tomes
            .iter()
            .map(LockFile::tome_value)
            .collect::<Vec<_>>();
        record.push("tomes", Value::list(tomes, Span::unknown()));

        let addendums = self
            .addendums
            .iter()
            .map(LockFile::addendum_value)
            .collect::<Vec<_>>();
        record.push("addendums", Value::list(addendums, Span::unknown()));

        let packages = self
            .packages
            .iter()
            .map(LockFile::package_value)
            .collect::<Vec<_>>();
        record.push("packages", Value::list(packages, Span::unknown()));

        Value::record(record, Span::unknown())
    }

    fn target_value(&self) -> Value {
        let mut parts = self.target.splitn(3, '-');
        let os = parts.next().unwrap_or("");
        let arch = parts.next().unwrap_or("");
        let abi = parts.next().unwrap_or("");
        let mut record = Record::new();
        record.push("os", Value::string(os, Span::unknown()));
        record.push("arch", Value::string(arch, Span::unknown()));
        record.push("abi", Value::string(abi, Span::unknown()));
        Value::record(record, Span::unknown())
    }

    fn tome_value(tome: &TomeState) -> Value {
        let mut record = Record::new();
        record.push("name", Value::string(&tome.name, Span::unknown()));
        record.push("source_url", Value::string(&tome.url, Span::unknown()));
        record.push(
            "source_commit",
            Value::string(
                tome.checked_commit.as_deref().unwrap_or(""),
                Span::unknown(),
            ),
        );
        Value::record(record, Span::unknown())
    }

    fn addendum_value(addendum: &AddendumState) -> Value {
        let mut record = Record::new();
        record.push("name", Value::string(&addendum.name, Span::unknown()));
        record.push("source_url", Value::string(&addendum.url, Span::unknown()));
        record.push(
            "source_commit",
            Value::string(
                addendum.checked_commit.as_deref().unwrap_or(""),
                Span::unknown(),
            ),
        );
        Value::record(record, Span::unknown())
    }

    fn package_value(package: &PackageState) -> Value {
        let mut record = Record::new();
        record.push("name", Value::string(&package.name, Span::unknown()));
        record.push("version", Value::string(&package.version, Span::unknown()));
        record.push(
            "target",
            Value::string(package.target.as_deref().unwrap_or(""), Span::unknown()),
        );
        record.push(
            "archive_hash",
            Value::string(&package.archive_hash, Span::unknown()),
        );
        record.push("source_hashes", string_map_value(&package.source_hashes));
        record.push("runtime_deps", string_list_value(&package.runtime_deps));
        record.push("build_deps", string_list_value(&package.build_deps));
        Value::record(record, Span::unknown())
    }
}
