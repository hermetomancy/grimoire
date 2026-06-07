//! The data model: typed representations of Grimoire's on-disk NUON documents.
//!
//! Package metadata, dependency requirements, package indexes, installed-package state, tome
//! manifests, and the lockfile all live here, with `from_value`/`to_value` conversions to and
//! from Nushell `Value`s. Construction validates structure (names, targets, semver) so the rest
//! of the codebase works with already-checked data.

use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, path::Path};

use anyhow::{Context, Result, bail};
use nu_protocol::{Record, Span, Value};
use semver::{Version, VersionReq};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PackageMetadata {
    pub name: String,
    pub version: String,
    pub target: Option<String>,
    #[serde(default)]
    pub store_path: Option<String>,
    /// Supported target triples for source builds. When empty, the rune accepts any target.
    #[serde(default)]
    pub targets: Vec<String>,
    /// `true` for a fixed-output (x-bin / fetch-only) package: its `build` only fetches and
    /// sha256-verifies prebuilt sources rather than compiling. Such a package is content-addressed
    /// by its sources alone, so its store hash excludes the host build environment and dependency
    /// closure (a Nix fixed-output derivation).
    #[serde(default)]
    pub fixed_output: bool,
    #[serde(default)]
    pub summary: Option<String>,
    /// Binaries this package provides, keyed by target pattern (`default`, OS name like `linux`,
    /// or full triple like `linux-x86_64-musl`). Merged at resolution time: `default` → OS → exact.
    #[serde(default)]
    pub bins: BTreeMap<String, BTreeMap<String, String>>,
    #[serde(default)]
    pub sources: BTreeMap<String, Source>,
    #[serde(default)]
    pub deps: Deps,
    #[serde(default)]
    pub build_flags: BTreeMap<String, String>,
    /// Command names this package provides, discovered at build time.
    #[serde(default)]
    pub provides: Vec<String>,
    /// Library base names (e.g. "foo" for libfoo.so) discovered at build time.
    #[serde(default)]
    pub libs: Vec<String>,
}

/// A declared source artifact for a source build. Every source must carry a checksum so
/// it can be verified before the build consumes it (AGENTS.md §5.1).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Source {
    pub url: String,
    pub sha256: String,
}

/// A dependency on another package, optionally constrained to a semver range. A bare name in a
/// rune (`"hello"`) parses to a [`VersionReq`] of `*` (any version); a record
/// (`{ name: "libc", version: ">=2.0" }`) carries an explicit requirement.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Dependency {
    pub name: String,
    pub req: VersionReq,
    #[serde(default)]
    pub platform: Option<String>,
}

impl Dependency {
    pub fn any(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            req: VersionReq::STAR,
            platform: None,
        }
    }

    /// Returns `true` when this dependency has no platform constraint or the constraint matches
    /// `target_triple` per [`dep_matches_platform`].
    pub fn matches_platform(&self, target_triple: &str) -> bool {
        match &self.platform {
            None => true,
            Some(pattern) => dep_matches_platform(pattern, target_triple),
        }
    }
}

/// Build and runtime dependencies declared by a rune. Runtime deps are installed with the
/// package; build deps are required only for a source build.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Deps {
    /// Build dependencies keyed by target triple, plus an optional `default` set that
    /// applies to every target. Use [`Deps::build_for`] to resolve the set for a target.
    #[serde(default)]
    pub build: BTreeMap<String, Vec<Dependency>>,
    #[serde(default)]
    pub runtime: Vec<Dependency>,
}

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
    /// Command names this package provides (discovered at build time). Used for capability
    /// resolution when the solver reads from indexes or installed state.
    #[serde(default)]
    pub provides: Vec<String>,
    /// Library base names (e.g. "foo" for libfoo.so) discovered at build time.
    #[serde(default)]
    pub libs: Vec<String>,
}

/// A binary package repository index (`index.nuon`): the set of pre-built archives a tome's
/// package repository offers. Read-only data — Grimoire reads it, never executes it (§3).
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
    /// The minisign public keys this tome's packages are verified against, pinned on first sync
    /// (trust-on-first-use). Empty for an unsigned tome. Once set, every later sync must
    /// present the same set; packages without a valid signature from one of these keys are
    /// refused. See `src/signing.rs`.
    #[serde(default)]
    pub signer_pubkeys: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AddendumState {
    pub name: String,
    pub url: String,
    #[serde(rename = "ref")]
    pub ref_name: String,
    #[serde(default)]
    pub checked_ref: Option<String>,
    #[serde(default)]
    pub checked_commit: Option<String>,
    #[serde(default)]
    pub addendum: Option<AddendumManifest>,
    /// The minisign public keys this addendum is verified against, pinned on first sync
    /// (trust-on-first-use). Empty for an unsigned addendum. Once set, every later sync must
    /// present the same set.
    #[serde(default)]
    pub signer_pubkeys: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AddendumManifest {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub patches: Vec<AddendumPatch>,
    /// Minisign public keys (base64) that may sign this addendum's `addendum.nuon`.
    #[serde(default)]
    pub signers: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AddendumPatch {
    #[serde(default)]
    pub tome: Option<String>,
    pub package: String,
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub target: Option<String>,
    #[serde(default)]
    pub summary: Option<String>,
    #[serde(default)]
    pub bins: Option<BTreeMap<String, BTreeMap<String, String>>>,
    #[serde(default)]
    pub sources: Option<BTreeMap<String, Source>>,
    #[serde(default)]
    pub deps: Option<Deps>,
    #[serde(default)]
    pub build_flags: Option<BTreeMap<String, String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TomeManifest {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub packages: Option<TomePackages>,
    /// Minisign public keys that may sign packages in this tome. When non-empty, every
    /// package (rune and archive) must carry a valid detached `.minisig` from one of these keys.
    #[serde(default)]
    pub signers: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TomePackages {
    pub repo: String,
    pub format: String,
    pub index: String,
}

impl Deps {
    /// Build dependencies that apply to `target`: the `default` set plus any entry keyed by
    /// the exact target triple, de-duplicated while preserving order. Platform-conditional deps
    /// that do not match `target` are filtered out. Distinct requirements on the same dependency
    /// are kept so the resolver can intersect them.
    pub fn build_for(&self, target: &str) -> Vec<Dependency> {
        let mut deps: Vec<Dependency> = Vec::new();
        let os = target.split('-').next().unwrap_or("");
        for key in ["default", os, target] {
            if let Some(entries) = self.build.get(key) {
                for dep in entries {
                    if !dep.matches_platform(target) {
                        continue;
                    }
                    if !deps.iter().any(|d| d.name == dep.name && d.req == dep.req) {
                        deps.push(dep.clone());
                    }
                }
            }
        }
        deps
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
        let store_path = optional_string(&record, "store_path")?;
        let fixed_output = optional_bool(&record, "fixed_output")?.unwrap_or(false);
        // A package with no executables (e.g. a library) is valid: `bins` defaults to empty.
        let bins = match record.get("bins") {
            Some(value) => parse_target_conditional_bins(value, "package field `bins`")?,
            None => BTreeMap::new(),
        };

        let sources = match record.get("sources") {
            Some(value) => parse_sources(value)?,
            None => BTreeMap::new(),
        };
        let deps = match record.get("deps") {
            Some(value) => parse_deps(value)?,
            None => Deps::default(),
        };
        let build_flags = match record.get("build_flags") {
            Some(Value::Nothing { .. }) | None => BTreeMap::new(),
            Some(value) => expect_string_map(value, "package field `build_flags`")?,
        };
        let targets = optional_string_list(&record, "targets")?;
        let provides = optional_string_list(&record, "provides")?;
        let libs = optional_string_list(&record, "libs")?;

        Ok(Self {
            name,
            version,
            target,
            store_path,
            targets,
            fixed_output,
            summary,
            bins,
            sources,
            deps,
            build_flags,
            provides,
            libs,
        })
    }

    /// Resolve the bin map for a concrete target: `default` → OS wildcard → exact triple,
    /// with each level overriding the previous.
    pub fn bins_for(&self, target: &str) -> BTreeMap<String, String> {
        resolve_target_conditional(&self.bins, target)
    }

    /// Merge a build manifest into this metadata's `bins`. The manifest's entries override
    /// existing entries for the same target key.
    pub fn merge_build_manifest(&mut self, manifest: &BuildManifest) {
        for (target_key, inner) in &manifest.bins {
            self.bins
                .entry(target_key.clone())
                .or_default()
                .extend(inner.clone());
        }
    }

    pub fn archive_value(&self, target: &str, store_path: Option<&Path>) -> Value {
        let mut record = Record::new();
        record.push("format", Value::int(1, Span::unknown()));
        record.push("name", Value::string(&self.name, Span::unknown()));
        record.push("version", Value::string(&self.version, Span::unknown()));
        record.push("target", Value::string(target, Span::unknown()));
        if let Some(store_path) = store_path {
            record.push(
                "store_path",
                Value::string(store_path.display().to_string(), Span::unknown()),
            );
        } else if let Some(store_path) = &self.store_path {
            record.push("store_path", Value::string(store_path, Span::unknown()));
        }
        if self.fixed_output {
            record.push("fixed_output", Value::bool(true, Span::unknown()));
        }

        // Archives are target-specific, so write resolved bins under `default`.
        let mut bins = Record::new();
        let mut default = Record::new();
        for (name, path) in self.bins_for(target) {
            default.push(&name, Value::string(&path, Span::unknown()));
        }
        bins.push("default", Value::record(default, Span::unknown()));
        record.push("bins", Value::record(bins, Span::unknown()));

        if let Some(summary) = &self.summary {
            record.push("summary", Value::string(summary, Span::unknown()));
        }
        record.push("provides", string_list_value(&self.provides));
        record.push("libs", string_list_value(&self.libs));

        let mut sources = Record::new();
        for (name, source) in &self.sources {
            let mut entry = Record::new();
            entry.push("url", Value::string(&source.url, Span::unknown()));
            entry.push("sha256", Value::string(&source.sha256, Span::unknown()));
            sources.push(name, Value::record(entry, Span::unknown()));
        }
        record.push("sources", Value::record(sources, Span::unknown()));
        record.push("deps", self.deps.to_value());
        record.push("build_flags", string_map_value(&self.build_flags));

        Value::record(record, Span::unknown())
    }

    pub fn apply_addendum_patch(&mut self, patch: &AddendumPatch) {
        if let Some(version) = &patch.version {
            self.version = version.clone();
        }
        if let Some(target) = &patch.target {
            self.target = Some(target.clone());
        }
        if let Some(summary) = &patch.summary {
            self.summary = Some(summary.clone());
        }
        if let Some(bins) = &patch.bins {
            for (target_key, inner) in bins {
                self.bins
                    .entry(target_key.clone())
                    .or_default()
                    .extend(inner.clone());
            }
        }
        if let Some(sources) = &patch.sources {
            self.sources.extend(sources.clone());
        }
        if let Some(deps) = &patch.deps {
            self.deps = deps.clone();
        }
        if let Some(build_flags) = &patch.build_flags {
            self.build_flags.extend(build_flags.clone());
        }
    }
}

/// A manifest returned by a rune's `build` function describing what was actually produced.
/// Grimoire merges this with the static `package.bins` declaration so the build is the
/// ground truth for what gets validated and installed.
#[derive(Debug, Clone, Default)]
pub struct BuildManifest {
    pub bins: BTreeMap<String, BTreeMap<String, String>>,
}

impl BuildManifest {
    pub fn from_value(value: Value) -> Result<Self> {
        let record = expect_record(value, "build manifest")?;
        let bins = match record.get("bins") {
            Some(value) => parse_target_conditional_bins(value, "build manifest field `bins`")?,
            None => BTreeMap::new(),
        };
        Ok(Self { bins })
    }
}

/// Resolve a target-conditional map (`default` → OS → exact triple) for a concrete target.
pub fn resolve_target_conditional(
    map: &BTreeMap<String, BTreeMap<String, String>>,
    target: &str,
) -> BTreeMap<String, String> {
    let mut result = BTreeMap::new();
    if let Some(default) = map.get("default") {
        result.extend(default.clone());
    }
    let os = target.split('-').next().unwrap_or("");
    if !os.is_empty() && os != "default" {
        if let Some(os_bins) = map.get(os) {
            result.extend(os_bins.clone());
        }
    }
    if let Some(target_bins) = map.get(target) {
        result.extend(target_bins.clone());
    }
    result
}

impl Deps {
    fn to_value(&self) -> Value {
        let mut record = Record::new();
        let mut build = Record::new();
        for (target, deps) in &self.build {
            build.push(target, dependency_list_value(deps));
        }
        record.push("build", Value::record(build, Span::unknown()));
        record.push("runtime", dependency_list_value(&self.runtime));
        Value::record(record, Span::unknown())
    }
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
        let provides = optional_string_list(&record, "provides")?;
        let libs = optional_string_list(&record, "libs")?;

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
            provides,
            libs,
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
        record.push("provides", string_list_value(&self.provides));
        record.push("libs", string_list_value(&self.libs));
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

impl AddendumState {
    pub fn from_value(value: Value) -> Result<Self> {
        let record = expect_record(value, "addendum state")?;
        let name = required_field_string(&record, "addendum state", "name")?;
        let url = required_field_string(&record, "addendum state", "url")?;
        let ref_name = required_field_string(&record, "addendum state", "ref")?;
        let checked_ref = optional_string(&record, "checked_ref")?;
        let checked_commit = optional_string(&record, "checked_commit")?;

        Ok(Self {
            name,
            url,
            ref_name,
            checked_ref,
            checked_commit,
            addendum: match record.get("addendum") {
                Some(Value::Nothing { .. }) | None => None,
                Some(value) => Some(AddendumManifest::from_value(value.clone())?),
            },
            signer_pubkeys: optional_string_list(&record, "signer_pubkeys")?,
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
        if let Some(addendum) = &self.addendum {
            record.push("addendum", addendum.to_value());
        }
        if !self.signer_pubkeys.is_empty() {
            record.push("signer_pubkeys", string_list_value(&self.signer_pubkeys));
        }
        Value::record(record, Span::unknown())
    }
}

impl AddendumManifest {
    pub fn from_value(value: Value) -> Result<Self> {
        let record = expect_record(value, "addendum manifest")?;
        let name = required_field_string(&record, "addendum manifest", "name")?;
        validate_tome_name(&name)?;
        let description = optional_string(&record, "description")?;
        let patches = match record.get("patches") {
            Some(Value::List { vals, .. }) => vals
                .iter()
                .map(AddendumPatch::from_value)
                .collect::<Result<Vec<_>>>()?,
            Some(_) => bail!("addendum manifest field `patches` must be a list"),
            None => Vec::new(),
        };
        let signers = optional_string_list(&record, "signers")?;

        Ok(Self {
            name,
            description,
            patches,
            signers,
        })
    }

    pub fn to_value(&self) -> Value {
        let mut record = Record::new();
        record.push("name", Value::string(&self.name, Span::unknown()));
        if let Some(description) = &self.description {
            record.push("description", Value::string(description, Span::unknown()));
        }
        let patches = self
            .patches
            .iter()
            .map(AddendumPatch::to_value)
            .collect::<Vec<_>>();
        record.push("patches", Value::list(patches, Span::unknown()));
        if !self.signers.is_empty() {
            record.push("signers", string_list_value(&self.signers));
        }
        Value::record(record, Span::unknown())
    }
}

impl AddendumPatch {
    fn from_value(value: &Value) -> Result<Self> {
        let Value::Record { val, .. } = value else {
            bail!("addendum patch must be a record");
        };
        let package = required_field_string(val, "addendum patch", "package")?;
        validate_package_name(&package)?;
        let tome = optional_string(val, "tome")?;
        if let Some(tome) = &tome {
            validate_tome_name(tome)?;
        }
        let version = optional_string(val, "version")?;
        if let Some(version) = &version {
            validate_package_version(version)?;
        }
        let target = optional_string(val, "target")?;
        let summary = optional_string(val, "summary")?;
        let bins = match val.get("bins") {
            Some(Value::Nothing { .. }) | None => None,
            Some(value) => Some(parse_target_conditional_bins(
                value,
                "addendum patch field `bins`",
            )?),
        };
        let sources = match val.get("sources") {
            Some(Value::Nothing { .. }) | None => None,
            Some(value) => Some(parse_sources(value)?),
        };
        let deps = match val.get("deps") {
            Some(Value::Nothing { .. }) | None => None,
            Some(value) => Some(parse_deps(value)?),
        };
        let build_flags = match val.get("build_flags") {
            Some(Value::Nothing { .. }) | None => None,
            Some(value) => Some(expect_string_map(
                value,
                "addendum patch field `build_flags`",
            )?),
        };

        Ok(Self {
            tome,
            package,
            version,
            target,
            summary,
            bins,
            sources,
            deps,
            build_flags,
        })
    }

    fn to_value(&self) -> Value {
        let mut record = Record::new();
        if let Some(tome) = &self.tome {
            record.push("tome", Value::string(tome, Span::unknown()));
        }
        record.push("package", Value::string(&self.package, Span::unknown()));
        if let Some(version) = &self.version {
            record.push("version", Value::string(version, Span::unknown()));
        }
        if let Some(target) = &self.target {
            record.push("target", Value::string(target, Span::unknown()));
        }
        if let Some(summary) = &self.summary {
            record.push("summary", Value::string(summary, Span::unknown()));
        }
        if let Some(bins) = &self.bins {
            record.push("bins", string_map_of_maps_value(bins));
        }
        if let Some(sources) = &self.sources {
            let mut out = Record::new();
            for (name, source) in sources {
                let mut entry = Record::new();
                entry.push("url", Value::string(&source.url, Span::unknown()));
                entry.push("sha256", Value::string(&source.sha256, Span::unknown()));
                out.push(name, Value::record(entry, Span::unknown()));
            }
            record.push("sources", Value::record(out, Span::unknown()));
        }
        if let Some(deps) = &self.deps {
            record.push("deps", deps.to_value());
        }
        if let Some(build_flags) = &self.build_flags {
            record.push("build_flags", string_map_value(build_flags));
        }
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
            signer_pubkeys: optional_string_list(&record, "signer_pubkeys")?,
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
        if !self.signer_pubkeys.is_empty() {
            record.push("signer_pubkeys", string_list_value(&self.signer_pubkeys));
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
        let signers = optional_string_list(&record, "signers")?;

        Ok(Self {
            name,
            description,
            packages,
            signers,
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
        if !self.signers.is_empty() {
            record.push("signers", string_list_value(&self.signers));
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

/// Validates that the current target is supported by a rune's declared `targets` list.
/// An empty `targets` means the rune accepts any target.
pub fn validate_targets(metadata: &PackageMetadata, current: &str) -> Result<()> {
    if metadata.targets.is_empty() {
        return Ok(());
    }

    if metadata.targets.iter().any(|t| t == current) {
        return Ok(());
    }

    bail!(
        "package `{}` does not support target `{}`; supported targets are: {}",
        metadata.name,
        current,
        metadata.targets.join(", ")
    );
}

/// Returns `true` when `pattern` matches `target_triple`.
///
/// * If `pattern` contains `-` or `*`, it is matched against the full target triple using simple
///   glob semantics (`*` matches any sequence).
/// * Otherwise the pattern is matched against the OS component (the first segment of the triple).
pub fn dep_matches_platform(pattern: &str, target_triple: &str) -> bool {
    if pattern.contains('-') || pattern.contains('*') {
        glob_match(pattern, target_triple)
    } else {
        target_triple.split('-').next() == Some(pattern)
    }
}

/// Simple glob matching: `*` matches any (possibly empty) sequence of characters.
fn glob_match(pattern: &str, text: &str) -> bool {
    let mut pat = pattern.chars().peekable();
    let mut txt = text.chars().peekable();

    while let Some(p) = pat.next() {
        if p == '*' {
            // Consecutive stars are semantically identical to a single star in glob patterns.
            while pat.peek() == Some(&'*') {
                pat.next();
            }
            let remaining_pat: String = pat.clone().collect();
            if remaining_pat.is_empty() {
                return true;
            }
            let remaining_txt: String = txt.clone().collect();
            for skip in 0..=remaining_txt.len() {
                if glob_match(&remaining_pat, &remaining_txt[skip..]) {
                    return true;
                }
            }
            return false;
        }
        match txt.next() {
            Some(t) if t == p => continue,
            _ => return false,
        }
    }

    txt.next().is_none()
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

pub fn expect_record(value: Value, label: &str) -> Result<Record> {
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

fn optional_bool(record: &Record, field: &str) -> Result<Option<bool>> {
    match record.get(field) {
        Some(Value::Nothing { .. }) | None => Ok(None),
        Some(Value::Bool { val, .. }) => Ok(Some(*val)),
        Some(_) => bail!("field `{field}` must be a boolean"),
    }
}

pub fn optional_i64(record: &Record, field: &str) -> Result<Option<i64>> {
    match record.get(field) {
        Some(Value::Nothing { .. }) | None => Ok(None),
        Some(Value::Int { val, .. }) => Ok(Some(*val)),
        Some(_) => bail!("field `{field}` must be an integer"),
    }
}

pub fn required_field_i64(record: &Record, label: &str, field: &str) -> Result<i64> {
    let value = record
        .get(field)
        .ok_or_else(|| anyhow::anyhow!("{label} is missing required field `{field}`"))?;
    match value {
        Value::Int { val, .. } => Ok(*val),
        _ => bail!("{label} field `{field}` must be an integer"),
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

fn parse_target_conditional_bins(
    value: &Value,
    label: &str,
) -> Result<BTreeMap<String, BTreeMap<String, String>>> {
    let Value::Record { val, .. } = value else {
        bail!("{label} must be a record keyed by target pattern");
    };

    let mut out = BTreeMap::new();
    for (target_key, inner) in val.iter() {
        let inner_map = expect_string_map(inner, &format!("{label} target `{target_key}`"))?;
        for (name, path) in &inner_map {
            validate_bin_name(name)?;
            validate_relative_package_path(path, &format!("bin `{name}`"))?;
        }
        out.insert(target_key.clone(), inner_map);
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
        validate_sha256(&sha256, &format!("source `{name}` sha256"))?;
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
            for (target, deps) in val.iter() {
                out.insert(
                    target.clone(),
                    parse_dependency_list(deps, &format!("build deps for `{target}`"))?,
                );
            }
            out
        }
        Some(Value::Nothing { .. }) | None => BTreeMap::new(),
        Some(_) => bail!("package field `deps.build` must be a record keyed by target"),
    };
    let runtime = match val.get("runtime") {
        Some(value) => parse_dependency_list(value, "runtime deps")?,
        None => Vec::new(),
    };

    Ok(Deps { build, runtime })
}

/// Parses a NUON list of dependencies. Each element is either a bare name string (any version)
/// or a record `{ name, version }` whose `version` is a semver requirement.
fn parse_dependency_list(value: &Value, label: &str) -> Result<Vec<Dependency>> {
    let Value::List { vals, .. } = value else {
        bail!("{label} must be a list");
    };
    vals.iter()
        .map(|value| parse_dependency(value, label))
        .collect()
}

fn parse_dependency(value: &Value, label: &str) -> Result<Dependency> {
    match value {
        Value::String { val, .. } => {
            let (name, platform) = parse_bracket_syntax(val)?;
            validate_package_name(&name)?;
            Ok(Dependency {
                name,
                req: VersionReq::STAR,
                platform,
            })
        }
        Value::Record { val, .. } => {
            let name = required_field_string(val, label, "name")?;
            validate_package_name(&name)?;
            let req = parse_version_requirement(val.get("version"), label, &name)?;
            let platform = optional_string(val, "platform")?;
            Ok(Dependency {
                name,
                req,
                platform,
            })
        }
        _ => bail!(
            "{label} entries must be a name string, bracket string, or a {{ name, version }} record"
        ),
    }
}

fn parse_version_requirement(value: Option<&Value>, label: &str, name: &str) -> Result<VersionReq> {
    match value {
        Some(Value::Nothing { .. }) | None => Ok(VersionReq::STAR),
        Some(value) => {
            let raw = expect_string(value, &format!("{label} field `version`"))?;
            VersionReq::parse(&raw).with_context(|| {
                format!("dependency `{name}` version requirement `{raw}` is invalid")
            })
        }
    }
}

/// Parses bracket syntax `name[platform]` into `(name, Some(platform))`. Bare names return
/// `(name, None)`.
fn parse_bracket_syntax(val: &str) -> Result<(String, Option<String>)> {
    let Some(open) = val.find('[') else {
        return Ok((val.to_string(), None));
    };
    let Some(close) = val.rfind(']') else {
        bail!("dependency bracket syntax `{val}` is missing closing `]`");
    };
    if close != val.len() - 1 {
        bail!("dependency bracket syntax `{val}` has trailing characters after `]`");
    }
    let name = val[..open].trim().to_string();
    let platform = val[open + 1..close].trim().to_string();
    if platform.is_empty() {
        bail!("dependency bracket syntax `{val}` has empty platform");
    }
    Ok((name, Some(platform)))
}

fn expect_string_list(value: &Value, label: &str) -> Result<Vec<String>> {
    let Value::List { vals, .. } = value else {
        bail!("{label} must be a list");
    };
    vals.iter()
        .map(|value| expect_string(value, label))
        .collect()
}

pub fn optional_string_list(record: &Record, field: &str) -> Result<Vec<String>> {
    match record.get(field) {
        Some(Value::Nothing { .. }) | None => Ok(Vec::new()),
        Some(value) => expect_string_list(value, &format!("field `{field}`")),
    }
}

/// Serializes dependencies back to the NUON list form `parse_dependency_list` accepts.
/// A bare name string when the requirement is `*` and there is no platform constraint;
/// bracket syntax `name[platform]` when the requirement is `*` but a platform is present;
/// otherwise a `{ name, version, platform? }` record.
fn dependency_list_value(deps: &[Dependency]) -> Value {
    let items = deps
        .iter()
        .map(|dep| {
            if dep.req == VersionReq::STAR && dep.platform.is_none() {
                Value::string(&dep.name, Span::unknown())
            } else if dep.req == VersionReq::STAR {
                if let Some(platform) = &dep.platform {
                    Value::string(format!("{}[{}]", dep.name, platform), Span::unknown())
                } else {
                    Value::string(&dep.name, Span::unknown())
                }
            } else {
                let mut record = Record::new();
                record.push("name", Value::string(&dep.name, Span::unknown()));
                record.push(
                    "version",
                    Value::string(dep.req.to_string(), Span::unknown()),
                );
                if let Some(platform) = &dep.platform {
                    record.push("platform", Value::string(platform, Span::unknown()));
                }
                Value::record(record, Span::unknown())
            }
        })
        .collect();
    Value::list(items, Span::unknown())
}

pub fn string_list_value(items: &[String]) -> Value {
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

fn string_map_of_maps_value(items: &BTreeMap<String, BTreeMap<String, String>>) -> Value {
    let mut record = Record::new();
    for (key, inner) in items {
        record.push(key, string_map_value(inner));
    }
    Value::record(record, Span::unknown())
}

fn required_field_string(record: &Record, label: &str, field: &str) -> Result<String> {
    let value = record
        .get(field)
        .ok_or_else(|| anyhow::anyhow!("{label} is missing required field `{field}`"))?;
    expect_string(value, &format!("{label} field `{field}`"))
}

/// Orders two version strings by semver precedence. Versions are semver-validated on the way
/// in, so parsing succeeds in practice; an unparsable value falls back to lexical order.
pub fn compare_versions(a: &str, b: &str) -> std::cmp::Ordering {
    match (parse_version_relaxed(a), parse_version_relaxed(b)) {
        (Ok(a), Ok(b)) => a.cmp(&b),
        _ => a.cmp(b),
    }
}

pub fn validate_package_name(name: &str) -> Result<()> {
    validate_ident(name, "package name")
}

/// Parse a version string, normalizing two-component (and one-component) versions to semver
/// by appending missing `.0` components: `"5.3"` → `"5.3.0"`, `"2"` → `"2.0.0"`.
pub fn parse_version_relaxed(s: &str) -> Result<Version> {
    Version::parse(s).or_else(|_| {
        let normalized = if s.contains('-') || s.contains('+') {
            s.to_string()
        } else {
            let dots = s.matches('.').count();
            match dots {
                0 => format!("{s}.0.0"),
                1 => format!("{s}.0"),
                _ => s.to_string(),
            }
        };
        Version::parse(&normalized).with_context(|| {
            format!("version `{s}` (normalized: `{normalized}`) is not valid semver")
        })
    })
}

pub fn validate_package_version(version: &str) -> Result<()> {
    parse_version_relaxed(version)
        .map(|_| ())
        .with_context(|| format!("package version `{version}` is not valid semver"))
}

/// A bin name becomes a profile entry *file name* under `profiles/current/bin/` — a hard link
/// into the store on all platforms — and is never interpreted as code. So, unlike package/tome
/// identifiers, a bin name only has to be a safe single path component that works on both
/// platforms. We allow the extra punctuation real command names use (notably `[` from coreutils)
/// but reject path separators, control characters, the `.`/`..` directory names, a leading `.`
/// (hidden entries), and the characters Windows forbids in file names so a name valid on one
/// platform cannot fail to install on another.
fn validate_bin_name(name: &str) -> Result<()> {
    const WINDOWS_RESERVED: &str = "<>:\"/\\|?*";

    if name.is_empty() {
        bail!("bin name must not be empty");
    }
    if name == "." || name == ".." {
        bail!("bin name `{name}` is not a valid file name");
    }
    if name.starts_with('.') {
        bail!("bin name `{name}` must not start with `.`");
    }
    for c in name.chars() {
        if !c.is_ascii_graphic() {
            bail!("bin name `{name}` contains unsupported character (must be printable ASCII)");
        }
        if WINDOWS_RESERVED.contains(c) {
            bail!("bin name `{name}` contains unsupported character `{c}`");
        }
    }
    Ok(())
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

pub(crate) fn looks_windows_absolute(path: &str) -> bool {
    let bytes = path.as_bytes();
    bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && (bytes[2] == b'/' || bytes[2] == b'\\')
}

pub fn validate_sha256(hash: &str, label: &str) -> Result<()> {
    let hex = hash.strip_prefix("sha256:").unwrap_or(hash).trim();
    if hex.len() != 64 || !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        bail!("{label} must be a sha256 digest (`sha256:<64 hex>` or bare 64 hex)");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_for_merges_default_and_target() {
        let mut build = BTreeMap::new();
        build.insert("default".to_owned(), vec![Dependency::any("cmake")]);
        build.insert(
            "x86_64-unknown-linux-gnu".to_owned(),
            vec![Dependency::any("gcc"), Dependency::any("cmake")],
        );
        let deps = Deps {
            build,
            runtime: Vec::new(),
        };

        let names = |target| {
            deps.build_for(target)
                .into_iter()
                .map(|dep| dep.name)
                .collect::<Vec<_>>()
        };
        assert_eq!(names("x86_64-unknown-linux-gnu"), vec!["cmake", "gcc"]);
        assert_eq!(names("aarch64-apple-darwin"), vec!["cmake"]);
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

    #[test]
    fn validate_bin_name_accepts_real_command_names() {
        // Plain names plus the punctuation real tools use, including coreutils `[` and names
        // that lead with a digit or symbol — none of which are valid package identifiers.
        for name in [
            "ls",
            "g++",
            "py3-tools",
            "[",
            "7z",
            "2to3",
            "x86_64-gcc",
            "a+b",
        ] {
            assert!(
                validate_bin_name(name).is_ok(),
                "bin name `{name}` should be accepted"
            );
        }
    }

    #[test]
    fn validate_bin_name_rejects_unsafe_file_names() {
        // Path separators, traversal, hidden entries, Windows-reserved characters, whitespace,
        // control characters, and non-ASCII all break the "safe cross-platform file name" rule.
        for name in [
            "", "a/b", "a\\b", ".", "..", ".hidden", "a:b", "a*b", "a?b", "a|b", "a<b", "a>b",
            "a\"b", "a b", "a\tb", "café",
        ] {
            assert!(
                validate_bin_name(name).is_err(),
                "bin name `{name}` should be rejected"
            );
        }
    }

    #[test]
    fn parse_version_relaxed_normalizes_short_versions() {
        assert_eq!(parse_version_relaxed("5.3").unwrap(), Version::new(5, 3, 0));
        assert_eq!(
            parse_version_relaxed("2.72").unwrap(),
            Version::new(2, 72, 0)
        );
        assert_eq!(parse_version_relaxed("1").unwrap(), Version::new(1, 0, 0));
        assert_eq!(
            parse_version_relaxed("1.2.3").unwrap(),
            Version::new(1, 2, 3)
        );
        assert_eq!(
            parse_version_relaxed("1.2.3-alpha").unwrap(),
            Version::parse("1.2.3-alpha").unwrap()
        );
    }

    #[test]
    fn parse_version_relaxed_rejects_garbage() {
        assert!(parse_version_relaxed("not-a-version").is_err());
    }

    #[test]
    fn dep_matches_platform_simple_os() {
        assert!(dep_matches_platform("linux", "linux-x86_64-musl"));
        assert!(!dep_matches_platform("macos", "linux-x86_64-musl"));
        assert!(dep_matches_platform("macos", "macos-aarch64-darwin"));
    }

    #[test]
    fn dep_matches_platform_glob_full_triple() {
        assert!(dep_matches_platform("linux-*-musl", "linux-x86_64-musl"));
        assert!(dep_matches_platform("linux-*-musl", "linux-aarch64-musl"));
        assert!(!dep_matches_platform("linux-*-gnu", "linux-x86_64-musl"));
        assert!(dep_matches_platform("*", "linux-x86_64-musl"));
        assert!(dep_matches_platform(
            "macos-*-darwin",
            "macos-aarch64-darwin"
        ));
    }

    #[test]
    fn dep_matches_platform_exact_triple() {
        assert!(dep_matches_platform(
            "linux-x86_64-musl",
            "linux-x86_64-musl"
        ));
        assert!(!dep_matches_platform(
            "linux-aarch64-musl",
            "linux-x86_64-musl"
        ));
    }

    #[test]
    fn parse_bracket_syntax_parses_platform() {
        assert_eq!(
            parse_bracket_syntax("linux-headers[linux]").unwrap(),
            ("linux-headers".to_string(), Some("linux".to_string()))
        );
        assert_eq!(
            parse_bracket_syntax("musl[linux-*-musl]").unwrap(),
            ("musl".to_string(), Some("linux-*-musl".to_string()))
        );
        assert_eq!(
            parse_bracket_syntax("llvm").unwrap(),
            ("llvm".to_string(), None)
        );
    }

    #[test]
    fn parse_bracket_syntax_rejects_invalid() {
        assert!(parse_bracket_syntax("foo[bar").is_err());
        assert!(parse_bracket_syntax("foo[bar]baz").is_err());
        assert!(parse_bracket_syntax("foo[]").is_err());
    }

    #[test]
    fn build_for_filters_platform_deps() {
        let mut build = BTreeMap::new();
        build.insert(
            "default".to_owned(),
            vec![
                Dependency {
                    name: "always".to_string(),
                    req: VersionReq::STAR,
                    platform: None,
                },
                Dependency {
                    name: "linux-only".to_string(),
                    req: VersionReq::STAR,
                    platform: Some("linux".to_string()),
                },
                Dependency {
                    name: "macos-only".to_string(),
                    req: VersionReq::STAR,
                    platform: Some("macos".to_string()),
                },
            ],
        );
        let deps = Deps {
            build,
            runtime: Vec::new(),
        };
        let names: Vec<_> = deps
            .build_for("linux-x86_64-musl")
            .into_iter()
            .map(|d| d.name)
            .collect();
        assert_eq!(names, vec!["always", "linux-only"]);
    }

    #[test]
    fn build_for_merges_os_wildcard() {
        let mut build = BTreeMap::new();
        build.insert("default".to_owned(), vec![Dependency::any("cmake")]);
        build.insert("linux".to_owned(), vec![Dependency::any("m4")]);
        build.insert("linux-x86_64-musl".to_owned(), vec![Dependency::any("gcc")]);
        let deps = Deps {
            build,
            runtime: Vec::new(),
        };
        let names: Vec<_> = deps
            .build_for("linux-x86_64-musl")
            .into_iter()
            .map(|d| d.name)
            .collect();
        assert_eq!(names, vec!["cmake", "m4", "gcc"]);
        assert_eq!(
            deps.build_for("macos-aarch64-darwin")
                .into_iter()
                .map(|d| d.name)
                .collect::<Vec<_>>(),
            vec!["cmake"]
        );
    }

    #[test]
    fn bins_for_merges_default_os_and_target() {
        let mut bins = BTreeMap::new();
        let mut default = BTreeMap::new();
        default.insert("sed".to_owned(), "bin/sed".to_owned());
        bins.insert("default".to_owned(), default);

        let mut linux = BTreeMap::new();
        linux.insert("awk".to_owned(), "bin/awk".to_owned());
        bins.insert("linux".to_owned(), linux);

        let mut musl = BTreeMap::new();
        musl.insert("tar".to_owned(), "bin/tar".to_owned());
        bins.insert("linux-x86_64-musl".to_owned(), musl);

        let meta = PackageMetadata {
            name: "toybox".to_owned(),
            version: "0.8.13".to_owned(),
            target: None,
            store_path: None,
            targets: vec![],
            fixed_output: false,
            summary: None,
            bins,
            sources: BTreeMap::new(),
            deps: Deps::default(),
            build_flags: BTreeMap::new(),
            provides: Vec::new(),
            libs: Vec::new(),
        };

        let resolved: Vec<_> = meta.bins_for("linux-x86_64-musl").into_keys().collect();
        assert_eq!(resolved, vec!["awk", "sed", "tar"]);

        let resolved: Vec<_> = meta.bins_for("linux-aarch64-gnu").into_keys().collect();
        assert_eq!(resolved, vec!["awk", "sed"]);

        let resolved: Vec<_> = meta.bins_for("macos-aarch64-darwin").into_keys().collect();
        assert_eq!(resolved, vec!["sed"]);
    }
}
