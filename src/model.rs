use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

use anyhow::{Result, bail};
use nu_protocol::{Record, Span, Value};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PackageMetadata {
    pub name: String,
    pub version: String,
    pub target: Option<String>,
    #[serde(default)]
    pub summary: Option<String>,
    #[serde(default)]
    pub bins: BTreeMap<String, String>,
    #[serde(default)]
    pub sources: BTreeMap<String, Source>,
    #[serde(default)]
    pub deps: Deps,
}

/// A declared source artifact for a source build. Every source must carry a checksum so
/// it can be verified before the build consumes it (AGENTS.md §5.1).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Source {
    pub url: String,
    pub sha256: String,
}

/// Build and runtime dependencies declared by a rune. Runtime deps are installed with the
/// package; build deps are required only for a source build.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Deps {
    /// Build dependencies keyed by target triple, plus an optional `default` set that
    /// applies to every target. Use [`Deps::build_for`] to resolve the set for a target.
    #[serde(default)]
    pub build: BTreeMap<String, Vec<String>>,
    #[serde(default)]
    pub runtime: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PackageState {
    pub name: String,
    pub version: String,
    pub target: Option<String>,
    pub archive_hash: String,
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
}

/// A binary package repository index (`index.nuon`): the set of pre-built archives a tome's
/// package repository offers. Read-only data — Grimoire reads it, never executes it (§3).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PackageIndex {
    pub packages: Vec<IndexEntry>,
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
    pub runtime_deps: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TomeState {
    pub name: String,
    pub url: String,
    #[serde(rename = "ref")]
    pub ref_name: String,
    #[serde(default)]
    pub checked_ref: Option<String>,
    #[serde(default)]
    pub checked_commit: Option<String>,
    #[serde(default)]
    pub tome: Option<TomeManifest>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TomeManifest {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub packages: Option<TomePackages>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TomePackages {
    pub repo: String,
    pub format: String,
    pub index: String,
}

impl Deps {
    /// Build dependencies that apply to `target`: the `default` set plus any entry keyed by
    /// the exact target triple, de-duplicated while preserving order.
    pub fn build_for(&self, target: &str) -> Vec<String> {
        let mut names = Vec::new();
        for key in ["default", target] {
            if let Some(entries) = self.build.get(key) {
                for name in entries {
                    if !names.contains(name) {
                        names.push(name.clone());
                    }
                }
            }
        }
        names
    }
}

impl PackageMetadata {
    pub fn from_value(value: Value, require_target: bool) -> Result<Self> {
        let record = expect_record(value, "package metadata")?;
        let name = required_field_string(&record, "package metadata", "name")?;
        let version = required_field_string(&record, "package metadata", "version")?;
        validate_package_name(&name)?;
        validate_package_version(&version)?;

        let target = optional_string(&record, "target")?;
        if require_target && target.is_none() {
            bail!("package metadata is missing required field `target`");
        }

        let summary = optional_string(&record, "summary")?;
        // A package with no executables (e.g. a library) is valid: `bins` defaults to empty.
        let bins = match record.get("bins") {
            Some(value) => expect_string_map(value, "package field `bins`")?,
            None => BTreeMap::new(),
        };

        for (name, path) in &bins {
            validate_bin_name(name)?;
            validate_relative_package_path(path, &format!("bin `{name}`"))?;
        }

        let sources = match record.get("sources") {
            Some(value) => parse_sources(value)?,
            None => BTreeMap::new(),
        };
        let deps = match record.get("deps") {
            Some(value) => parse_deps(value)?,
            None => Deps::default(),
        };

        Ok(Self {
            name,
            version,
            target,
            summary,
            bins,
            sources,
            deps,
        })
    }

    pub fn archive_value(&self, target: &str) -> Value {
        let mut record = Record::new();
        record.push("format", Value::int(1, Span::unknown()));
        record.push("name", Value::string(&self.name, Span::unknown()));
        record.push("version", Value::string(&self.version, Span::unknown()));
        record.push("target", Value::string(target, Span::unknown()));

        let mut bins = Record::new();
        for (name, path) in &self.bins {
            bins.push(name, Value::string(path, Span::unknown()));
        }
        record.push("bins", Value::record(bins, Span::unknown()));

        if let Some(summary) = &self.summary {
            record.push("summary", Value::string(summary, Span::unknown()));
        }

        Value::record(record, Span::unknown())
    }
}

impl PackageIndex {
    pub fn from_value(value: Value) -> Result<Self> {
        let record = expect_record(value, "package index")?;
        let packages = match record.get("packages") {
            Some(Value::List { vals, .. }) => vals
                .iter()
                .map(IndexEntry::from_value)
                .collect::<Result<Vec<_>>>()?,
            Some(_) => bail!("package index field `packages` must be a list"),
            None => bail!("package index is missing required field `packages`"),
        };
        Ok(Self { packages })
    }

    /// The entry matching `name` for `target`, if the index offers one.
    pub fn find(&self, name: &str, target: &str) -> Option<&IndexEntry> {
        self.packages
            .iter()
            .find(|entry| entry.name == name && entry.target == target)
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

        let runtime_deps = match val.get("runtime_deps") {
            Some(value) => expect_string_list(value, "index entry runtime_deps")?,
            None => Vec::new(),
        };

        Ok(Self {
            name,
            version,
            target,
            archive,
            archive_hash,
            runtime_deps,
        })
    }
}

impl PackageState {
    pub fn from_value(value: Value) -> Result<Self> {
        let record = expect_record(value, "package state")?;
        let name = required_field_string(&record, "package state", "name")?;
        let version = required_field_string(&record, "package state", "version")?;
        let target = optional_string(&record, "target")?;
        let archive_hash = required_field_string(&record, "package state", "archive_hash")?;
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

        Ok(Self {
            name,
            version,
            target,
            archive_hash,
            bins,
            runtime_deps,
            build_deps,
            source_hashes,
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

        let mut bins = Record::new();
        for (name, path) in &self.bins {
            bins.push(name, Value::string(path, Span::unknown()));
        }
        record.push("bins", Value::record(bins, Span::unknown()));
        record.push("runtime_deps", string_list_value(&self.runtime_deps));
        record.push("build_deps", string_list_value(&self.build_deps));
        record.push("source_hashes", string_map_value(&self.source_hashes));
        Value::record(record, Span::unknown())
    }
}

/// The reproducible install snapshot written to `grimoire.lock.nuon`. Built from the recorded
/// installed package state and configured tome state, so it can be regenerated deterministically
/// after any install or removal. Write-only for now (Grimoire emits it; nothing reads it back).
pub struct LockFile {
    pub target: String,
    pub tomes: Vec<TomeState>,
    pub packages: Vec<PackageState>,
}

impl LockFile {
    pub fn new(target: String, tomes: Vec<TomeState>, packages: Vec<PackageState>) -> Self {
        Self {
            target,
            tomes,
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

        // Addendums are not wired yet (TODO item 6); the field is present and empty so the
        // lockfile shape is stable once they land.
        record.push("addendums", Value::list(Vec::new(), Span::unknown()));

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

impl TomeState {
    pub fn from_value(value: Value) -> Result<Self> {
        let record = expect_record(value, "tome state")?;
        let name = required_field_string(&record, "tome state", "name")?;
        let url = required_field_string(&record, "tome state", "url")?;
        let ref_name = required_field_string(&record, "tome state", "ref")?;
        let checked_ref = optional_string(&record, "checked_ref")?;
        let checked_commit = optional_string(&record, "checked_commit")?;

        Ok(Self {
            name,
            url,
            ref_name,
            checked_ref,
            checked_commit,
            tome: match record.get("tome") {
                Some(Value::Nothing { .. }) | None => None,
                Some(value) => Some(TomeManifest::from_value(value.clone())?),
            },
        })
    }

    pub fn to_value(&self) -> Value {
        let mut record = Record::new();
        record.push("name", Value::string(&self.name, Span::unknown()));
        record.push("url", Value::string(&self.url, Span::unknown()));
        record.push("ref", Value::string(&self.ref_name, Span::unknown()));
        if let Some(checked_ref) = &self.checked_ref {
            record.push("checked_ref", Value::string(checked_ref, Span::unknown()));
        }
        if let Some(checked_commit) = &self.checked_commit {
            record.push(
                "checked_commit",
                Value::string(checked_commit, Span::unknown()),
            );
        }
        if let Some(tome) = &self.tome {
            record.push("tome", tome.to_value());
        }
        Value::record(record, Span::unknown())
    }
}

impl TomeManifest {
    pub fn from_value(value: Value) -> Result<Self> {
        let record = expect_record(value, "tome manifest")?;
        let name = required_field_string(&record, "tome manifest", "name")?;
        validate_tome_name(&name)?;
        let description = optional_string(&record, "description")?;
        let packages = match record.get("packages") {
            Some(value) => Some(TomePackages::from_value(value)?),
            None => None,
        };

        Ok(Self {
            name,
            description,
            packages,
        })
    }

    pub fn to_value(&self) -> Value {
        let mut record = Record::new();
        record.push("name", Value::string(&self.name, Span::unknown()));
        if let Some(description) = &self.description {
            record.push("description", Value::string(description, Span::unknown()));
        }
        if let Some(packages) = &self.packages {
            record.push("packages", packages.to_value());
        }
        Value::record(record, Span::unknown())
    }
}

impl TomePackages {
    fn from_value(value: &Value) -> Result<Self> {
        let Value::Record { val, .. } = value else {
            bail!("tome manifest field `packages` must be a record");
        };

        Ok(Self {
            repo: required_field_string(val, "tome packages", "repo")?,
            format: required_field_string(val, "tome packages", "format")?,
            index: required_field_string(val, "tome packages", "index")?,
        })
    }

    fn to_value(&self) -> Value {
        let mut record = Record::new();
        record.push("repo", Value::string(&self.repo, Span::unknown()));
        record.push("format", Value::string(&self.format, Span::unknown()));
        record.push("index", Value::string(&self.index, Span::unknown()));
        Value::record(record, Span::unknown())
    }
}

pub fn validate_target(metadata: &PackageMetadata, current: &str) -> Result<()> {
    let Some(target) = &metadata.target else {
        bail!("package metadata is missing target");
    };

    if target != current {
        bail!("package target `{target}` does not match current target `{current}`");
    }

    Ok(())
}

pub fn validate_relative_package_path(path: &str, label: &str) -> Result<()> {
    if path.starts_with('/') || path.starts_with('\\') || looks_windows_absolute(path) {
        bail!("{label} path `{path}` must be relative");
    }

    if path.contains('\\') {
        bail!("{label} path `{path}` must use / separators");
    }

    if path.split('/').any(|part| part == ".." || part.is_empty()) {
        bail!("{label} path `{path}` must not contain empty or parent components");
    }

    Ok(())
}

/// An archive location is either an `http(s)` URL or a path relative to the package
/// repository. Relative paths must stay inside the repo (no `..`, no absolute paths).
pub fn validate_archive_location(location: &str) -> Result<()> {
    if location.starts_with("http://") || location.starts_with("https://") {
        return Ok(());
    }
    validate_relative_package_path(location, "index entry archive")
}

pub fn validate_tome_name(name: &str) -> Result<()> {
    validate_ident(name, "tome name")
}

pub fn validate_tome_url(url: &str) -> Result<()> {
    if url.trim().is_empty() {
        bail!("tome git-url must not be empty");
    }
    Ok(())
}

pub fn validate_tome_ref(ref_name: &str) -> Result<()> {
    if ref_name.trim().is_empty() {
        bail!("tome ref must not be empty");
    }
    Ok(())
}

fn expect_record(value: Value, label: &str) -> Result<Record> {
    match value {
        Value::Record { val, .. } => Ok(val.into_owned()),
        _ => bail!("{label} must be a record"),
    }
}

fn optional_string(record: &Record, field: &str) -> Result<Option<String>> {
    match record.get(field) {
        Some(Value::Nothing { .. }) | None => Ok(None),
        Some(value) => expect_string(value, &format!("package field `{field}`")).map(Some),
    }
}

fn expect_string(value: &Value, label: &str) -> Result<String> {
    match value {
        Value::String { val, .. } => Ok(val.clone()),
        _ => bail!("{label} must be a string"),
    }
}

fn expect_string_map(value: &Value, label: &str) -> Result<BTreeMap<String, String>> {
    let Value::Record { val, .. } = value else {
        bail!("{label} must be a record");
    };

    let mut out = BTreeMap::new();
    for (key, value) in val.iter() {
        out.insert(
            key.clone(),
            expect_string(value, &format!("bin `{key}` path"))?,
        );
    }
    Ok(out)
}

fn parse_sources(value: &Value) -> Result<BTreeMap<String, Source>> {
    let Value::Record { val, .. } = value else {
        bail!("package field `sources` must be a record");
    };

    let mut out = BTreeMap::new();
    for (name, source) in val.iter() {
        let Value::Record { val: source, .. } = source else {
            bail!("source `{name}` must be a record");
        };
        let url = required_field_string(source, &format!("source `{name}`"), "url")?;
        let sha256 = required_field_string(source, &format!("source `{name}`"), "sha256")?;
        out.insert(name.clone(), Source { url, sha256 });
    }
    Ok(out)
}

fn parse_deps(value: &Value) -> Result<Deps> {
    let Value::Record { val, .. } = value else {
        bail!("package field `deps` must be a record");
    };

    let build = match val.get("build") {
        Some(Value::Record { val, .. }) => {
            let mut out = BTreeMap::new();
            for (target, names) in val.iter() {
                out.insert(target.clone(), expect_string_list(names, "build deps")?);
            }
            out
        }
        Some(Value::Nothing { .. }) | None => BTreeMap::new(),
        Some(_) => bail!("package field `deps.build` must be a record keyed by target"),
    };
    let runtime = match val.get("runtime") {
        Some(value) => expect_string_list(value, "runtime deps")?,
        None => Vec::new(),
    };

    Ok(Deps { build, runtime })
}

fn expect_string_list(value: &Value, label: &str) -> Result<Vec<String>> {
    let Value::List { vals, .. } = value else {
        bail!("{label} must be a list");
    };
    vals.iter()
        .map(|value| expect_string(value, label))
        .collect()
}

fn optional_string_list(record: &Record, field: &str) -> Result<Vec<String>> {
    match record.get(field) {
        Some(Value::Nothing { .. }) | None => Ok(Vec::new()),
        Some(value) => expect_string_list(value, &format!("field `{field}`")),
    }
}

fn string_list_value(items: &[String]) -> Value {
    Value::list(
        items
            .iter()
            .map(|item| Value::string(item, Span::unknown()))
            .collect(),
        Span::unknown(),
    )
}

fn string_map_value(items: &BTreeMap<String, String>) -> Value {
    let mut record = Record::new();
    for (key, value) in items {
        record.push(key, Value::string(value, Span::unknown()));
    }
    Value::record(record, Span::unknown())
}

fn required_field_string(record: &Record, label: &str, field: &str) -> Result<String> {
    let value = record
        .get(field)
        .ok_or_else(|| anyhow::anyhow!("{label} is missing required field `{field}`"))?;
    expect_string(value, &format!("{label} field `{field}`"))
}

fn validate_package_name(name: &str) -> Result<()> {
    validate_ident(name, "package name")
}

fn validate_package_version(version: &str) -> Result<()> {
    if !starts_valid(version)
        || !version
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "_.:+~-".contains(c))
    {
        bail!("package version `{version}` contains unsupported characters");
    }
    Ok(())
}

fn validate_bin_name(name: &str) -> Result<()> {
    validate_ident(name, "bin name")
}

fn validate_ident(value: &str, label: &str) -> Result<()> {
    if !starts_valid(value)
        || !value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "_.+-".contains(c))
    {
        bail!("{label} `{value}` contains unsupported characters");
    }
    Ok(())
}

fn starts_valid(value: &str) -> bool {
    value
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_alphanumeric())
}

fn looks_windows_absolute(path: &str) -> bool {
    let bytes = path.as_bytes();
    bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && (bytes[2] == b'/' || bytes[2] == b'\\')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_for_merges_default_and_target() {
        let mut build = BTreeMap::new();
        build.insert("default".to_owned(), vec!["cmake".to_owned()]);
        build.insert(
            "x86_64-unknown-linux-gnu".to_owned(),
            vec!["gcc".to_owned(), "cmake".to_owned()],
        );
        let deps = Deps {
            build,
            runtime: Vec::new(),
        };

        assert_eq!(
            deps.build_for("x86_64-unknown-linux-gnu"),
            vec!["cmake".to_owned(), "gcc".to_owned()]
        );
        assert_eq!(deps.build_for("aarch64-apple-darwin"), vec!["cmake"]);
    }

    #[test]
    fn build_for_empty_when_nothing_matches() {
        let deps = Deps::default();
        assert!(deps.build_for("x86_64-unknown-linux-gnu").is_empty());
    }

    #[test]
    fn validate_names_reject_path_traversal() {
        for name in ["../evil", "a/b", "/x", "..", ".hidden", "a\\b"] {
            assert!(
                validate_tome_name(name).is_err(),
                "tome name `{name}` should be rejected"
            );
            assert!(
                validate_package_name(name).is_err(),
                "package name `{name}` should be rejected"
            );
        }
    }

    #[test]
    fn validate_names_accept_plain_identifiers() {
        for name in ["hello", "lib.foo", "g++", "py3-tools"] {
            assert!(
                validate_tome_name(name).is_ok(),
                "tome name `{name}` should be accepted"
            );
            assert!(
                validate_package_name(name).is_ok(),
                "package name `{name}` should be accepted"
            );
        }
    }
}
