//! Package metadata: the `package` record a rune exports, plus the build manifest a
//! `build` function may return and target-conditional resolution for `bins`.

use std::{collections::BTreeMap, path::Path};

use anyhow::{Result, bail};
use nu_protocol::{Record, Span, Value};

use super::*;

#[derive(Debug, Clone)]
pub struct PackageMetadata {
    pub name: String,
    pub version: String,
    pub target: Option<String>,
    pub store_path: Option<String>,
    /// Supported target triples for source builds. When empty, the rune accepts any target.
    pub targets: Vec<String>,
    /// `true` for a fixed-output (x-bin / fetch-only) package: its `build` only fetches and
    /// sha256-verifies prebuilt sources rather than compiling. Such a package is content-addressed
    /// by its sources alone, so its store hash excludes the host build environment and dependency
    /// closure (a Nix fixed-output derivation).
    pub fixed_output: bool,
    /// `true` for the managed build-environment toolchain (`build-env` and its kind). It is
    /// installed and pinned like any requested package, but neither it nor its runtime closure is
    /// linked into the active profile — the toolchain is build machinery, not user commands, so it
    /// stays in the store (available for builds, survives `grm clean`) without flooding the PATH.
    pub build_only: bool,
    pub summary: Option<String>,
    /// Binaries this package provides, keyed by target pattern (`default`, OS name like `linux`,
    /// or full triple like `linux-x86_64-musl`). Merged at resolution time: `default` → OS → exact.
    pub bins: BTreeMap<String, BTreeMap<String, String>>,
    pub sources: BTreeMap<String, Source>,
    pub deps: Deps,
    pub build_flags: BTreeMap<String, String>,
    /// Command names this package provides, discovered at build time.
    pub provides: Vec<String>,
    /// Library base names (e.g. "foo" for libfoo.so) discovered at build time.
    pub libs: Vec<String>,
    /// User-facing post-install notes ("add yourself to the docker group"), declared
    /// statically in the rune's `package` const or returned dynamically by its `build`
    /// function. Printed once after install and replayable via `grm info`.
    pub notes: Vec<String>,
    /// The real upstream version string when `version` had to be normalized to semver
    /// (e.g. upstream `2025a` recorded as `2025.1.0`). Display only — never ordered.
    pub upstream_version: Option<String>,
    /// Installed packages this one cannot coexist with (bare names; checked symmetrically
    /// at install time for linked installs).
    pub conflicts: Vec<String>,
    /// Package names this one supersedes. Installing this package removes them in the same
    /// transaction, migrating their requested/held intent onto this package.
    pub replaces: Vec<String>,
    /// The parent package this rune splits from. A split member declares no sources and no
    /// `build` function: its files are carved out of the parent rune's build output by the
    /// `files` globs, and the whole group is built in one pass.
    pub split_from: Option<String>,
    /// Glob patterns (relative to the package payload root; `*` stays within a path
    /// component, `**` crosses directories) claiming this member's files from the parent
    /// build's output. Present exactly when `split_from` is set; the parent package
    /// receives every unclaimed file.
    pub files: Vec<String>,
}

/// A declared source artifact for a source build. Every source must carry a checksum so
/// it can be verified before the build consumes it (AGENTS.md §10.1).
#[derive(Debug, Clone)]
pub struct Source {
    pub url: String,
    pub sha256: String,
    /// Optional platform glob (same syntax as dependency brackets, e.g. `macos-*`,
    /// `linux-x86_64-musl`). The source is fetched and hashed only for matching targets —
    /// how a fixed-output package pins different prebuilt artifacts per platform.
    pub platform: Option<String>,
    /// Optional build-HOST libc filter (`"musl"` | `"glibc"`). When set, the source is selected
    /// only where grm's own host libc (see [`crate::util::paths::host_libc`]) matches — how
    /// `rust-stage0` pins a different bootstrap seed per host: a glibc host cross-seeds from the gnu
    /// release, a pure-musl host seeds from the musl release that only its `ld-musl` loader can run.
    /// `None` matches any host, so this never perturbs ordinary single-seed packages.
    pub host_libc: Option<String>,
}

impl PackageMetadata {
    pub fn from_value(value: Value, require_target: bool) -> Result<Self> {
        let record = expect_record(value, "package metadata")?;
        let allowed = if require_target {
            ARCHIVE_PACKAGE_FIELDS
        } else {
            AUTHORED_PACKAGE_FIELDS
        };
        reject_unknown_fields(&record, "package metadata", allowed)?;
        validate_meta_field(&record)?;
        let name = required_field_string(&record, "package metadata", "name")?;
        let version = required_field_string(&record, "package metadata", "version")?;
        validate_package_name(&name)?;
        validate_package_version(&version)?;

        let target = optional_string(&record, "target")?;
        if let Some(target) = &target {
            crate::util::paths::validate_target_triple(target)?;
        }
        if require_target && target.is_none() {
            bail!("package metadata is missing required field `target`");
        }

        let summary = optional_string(&record, "summary")?;
        let store_path = optional_string(&record, "store_path")?;
        let fixed_output = optional_bool(&record, "fixed_output")?.unwrap_or(false);
        let build_only = optional_bool(&record, "build_only")?.unwrap_or(false);
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
        for target in &targets {
            crate::util::paths::validate_target_triple(target)?;
        }
        let provides = optional_string_list(&record, "provides")?;
        for provide in &provides {
            validate_bin_name(provide)?;
        }
        let libs = optional_string_list(&record, "libs")?;
        let notes = optional_string_list(&record, "notes")?;
        let upstream_version = optional_string(&record, "upstream_version")?;
        let conflicts = optional_string_list(&record, "conflicts")?;
        let replaces = optional_string_list(&record, "replaces")?;
        let split_from = optional_string(&record, "split_from")?;
        let files = optional_string_list(&record, "files")?;
        validate_split_fields(
            &name,
            split_from.as_deref(),
            &files,
            &sources,
            &deps,
            &build_flags,
            &targets,
            fixed_output,
        )?;
        if !require_target {
            validate_fixed_output(&name, fixed_output, &sources, &deps)?;
        }

        Ok(Self {
            name,
            version,
            target,
            store_path,
            targets,
            fixed_output,
            build_only,
            summary,
            bins,
            sources,
            deps,
            build_flags,
            provides,
            libs,
            notes,
            upstream_version,
            conflicts,
            replaces,
            split_from,
            files,
        })
    }

    /// `true` when this package is a split member: a companion rune whose files come out of
    /// its parent's build rather than a build of its own.
    pub fn is_split_member(&self) -> bool {
        self.split_from.is_some()
    }

    /// Resolve the bin map for a concrete target: `default` → OS wildcard → exact triple,
    /// with each level overriding the previous.
    pub fn bins_for(&self, target: &str) -> BTreeMap<String, String> {
        resolve_target_conditional(&self.bins, target)
    }

    /// The declared sources that apply to `target`: platform-filtered sources whose glob does
    /// not match are excluded, exactly like platform-conditional deps, AND host-filtered by the
    /// optional `host_libc` field against this machine's [`crate::util::paths::host_libc`]. This
    /// filtered view is what gets fetched *and* what the store hash covers, so per-platform (and,
    /// for `rust-stage0`, per-host) artifacts do not perturb other platforms' content addresses.
    /// Like `target_triple()`, the host-libc input is a machine property — so a source set that
    /// declares `host_libc` is intentionally addressed per build host (correct for a bootstrap seed
    /// that must run on the host); sources that omit it are host-independent as before.
    pub fn sources_for(&self, target: &str) -> BTreeMap<String, Source> {
        let host_libc = crate::util::paths::host_libc();
        self.sources
            .iter()
            .filter(|(_, source)| {
                source
                    .platform
                    .as_deref()
                    .is_none_or(|pattern| dep_matches_platform(pattern, target))
                    && source.host_libc.as_deref().is_none_or(|h| h == host_libc)
            })
            .map(|(name, source)| (name.clone(), source.clone()))
            .collect()
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
        for note in &manifest.notes {
            if !self.notes.contains(note) {
                self.notes.push(note.clone());
            }
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
        if self.build_only {
            record.push("build_only", Value::bool(true, Span::unknown()));
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
        record.push("notes", string_list_value(&self.notes));
        if let Some(upstream) = &self.upstream_version {
            record.push("upstream_version", Value::string(upstream, Span::unknown()));
        }
        record.push("conflicts", string_list_value(&self.conflicts));
        record.push("replaces", string_list_value(&self.replaces));
        if let Some(split_from) = &self.split_from {
            record.push("split_from", Value::string(split_from, Span::unknown()));
            record.push("files", string_list_value(&self.files));
        }

        let mut sources = Record::new();
        for (name, source) in self.sources_for(target) {
            let mut entry = Record::new();
            entry.push("url", Value::string(source.url, Span::unknown()));
            entry.push("sha256", Value::string(source.sha256, Span::unknown()));
            if let Some(platform) = source.platform {
                entry.push("platform", Value::string(platform, Span::unknown()));
            }
            if let Some(host_libc) = source.host_libc {
                entry.push("host_libc", Value::string(host_libc, Span::unknown()));
            }
            sources.push(name, Value::record(entry, Span::unknown()));
        }
        record.push("sources", Value::record(sources, Span::unknown()));
        record.push("deps", self.deps.to_value());
        record.push("build_flags", string_map_value(&self.build_flags));

        Value::record(record, Span::unknown())
    }
}

const COMMON_PACKAGE_FIELDS: &[&str] = &[
    "name",
    "version",
    "targets",
    "fixed_output",
    "build_only",
    "summary",
    "meta",
    "bins",
    "sources",
    "deps",
    "build_flags",
    "provides",
    "libs",
    "notes",
    "upstream_version",
    "conflicts",
    "replaces",
    "split_from",
    "files",
];

const AUTHORED_PACKAGE_FIELDS: &[&str] = COMMON_PACKAGE_FIELDS;

const ARCHIVE_PACKAGE_FIELDS: &[&str] = &[
    "format",
    "target",
    "store_path",
    "name",
    "version",
    "targets",
    "fixed_output",
    "build_only",
    "summary",
    "meta",
    "bins",
    "sources",
    "deps",
    "build_flags",
    "provides",
    "libs",
    "notes",
    "upstream_version",
    "conflicts",
    "replaces",
    "split_from",
    "files",
];

fn validate_meta_field(record: &nu_protocol::Record) -> Result<()> {
    if let Some(value) = record.get("meta")
        && !matches!(value, Value::Record { .. } | Value::Nothing { .. })
    {
        bail!("package field `meta` must be a record");
    }
    Ok(())
}

/// A manifest returned by a rune's `build` function describing what was actually produced.
/// Grimoire merges this with the static `package.bins` declaration so the build is the
/// ground truth for what gets validated and installed.
#[derive(Debug, Clone, Default)]
pub struct BuildManifest {
    pub bins: BTreeMap<String, BTreeMap<String, String>>,
    /// User-facing post-install notes the build discovered (merged into the package notes).
    pub notes: Vec<String>,
}

impl BuildManifest {
    pub fn from_value(value: Value) -> Result<Self> {
        let record = expect_record(value, "build manifest")?;
        let bins = match record.get("bins") {
            Some(value) => parse_target_conditional_bins(value, "build manifest field `bins`")?,
            None => BTreeMap::new(),
        };
        let notes = optional_string_list(&record, "notes")?;
        Ok(Self { bins, notes })
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
    if !os.is_empty()
        && os != "default"
        && let Some(os_bins) = map.get(os)
    {
        result.extend(os_bins.clone());
    }
    if let Some(target_bins) = map.get(target) {
        result.extend(target_bins.clone());
    }
    result
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

/// Extracts the content-address hash embedded in an archive's `store_path` basename.
///
/// Archives store only the portable basename (`<hash>-<name>-<version>`), not the local store
/// root. Publishing and installing both validate against this same parser so an archive cannot be
/// indexed under one content address and later install into another.
pub fn embedded_store_hash(metadata: &PackageMetadata) -> Result<String> {
    let Some(basename) = metadata.store_path.as_deref() else {
        bail!(
            "package `{}` metadata is missing its store_path basename",
            metadata.name
        );
    };
    validate_relative_package_path(basename, "metadata store_path")?;
    let suffix = format!("-{}-{}", metadata.name, metadata.version);
    let Some(hash) = basename.strip_suffix(&suffix) else {
        bail!(
            "package `{}` metadata store_path `{basename}` is not `<hash>-{}-{}`",
            metadata.name,
            metadata.name,
            metadata.version
        );
    };
    if hash.is_empty() || hash.contains('/') {
        bail!(
            "package `{}` metadata store_path `{basename}` has an invalid hash component",
            metadata.name
        );
    }
    Ok(hash.to_owned())
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

/// Validates the `split_from`/`files` pair: a split member is *only* a claim on its parent's
/// build output, so everything that describes an independent build is forbidden on it. The
/// member inherits sources, build deps, build flags, and supported targets from the parent.
#[allow(clippy::too_many_arguments)]
fn validate_split_fields(
    name: &str,
    split_from: Option<&str>,
    files: &[String],
    sources: &BTreeMap<String, Source>,
    deps: &Deps,
    build_flags: &BTreeMap<String, String>,
    targets: &[String],
    fixed_output: bool,
) -> Result<()> {
    let Some(parent) = split_from else {
        if !files.is_empty() {
            bail!("package field `files` requires `split_from`: only a split member claims files");
        }
        return Ok(());
    };
    validate_package_name(parent)?;
    if parent == name {
        bail!("package `{name}` cannot split from itself");
    }
    if files.is_empty() {
        bail!("split member `{name}` must declare a non-empty `files` glob list");
    }
    for pattern in files {
        validate_relative_package_path(pattern, &format!("files glob `{pattern}`"))?;
        nu_glob::Pattern::new(pattern)
            .map_err(|err| anyhow::anyhow!("files glob `{pattern}` is invalid: {err}"))?;
    }
    if !sources.is_empty() {
        bail!("split member `{name}` must not declare `sources`; they come from `{parent}`");
    }
    if !deps.build.is_empty() {
        bail!("split member `{name}` must not declare build deps; declare them on `{parent}`");
    }
    if !build_flags.is_empty() {
        bail!("split member `{name}` must not declare `build_flags`; declare them on `{parent}`");
    }
    if !targets.is_empty() {
        bail!("split member `{name}` must not declare `targets`; it inherits `{parent}`'s");
    }
    if fixed_output {
        bail!("split member `{name}` cannot be fixed-output; splits only apply to compiled builds");
    }
    Ok(())
}

pub(crate) fn parse_target_conditional_bins(
    value: &Value,
    label: &str,
) -> Result<BTreeMap<String, BTreeMap<String, String>>> {
    let Value::Record { val, .. } = value else {
        bail!("{label} must be a record keyed by target pattern");
    };

    let mut out = BTreeMap::new();
    for (target_key, inner) in val.iter() {
        crate::util::paths::validate_target_key(target_key, label)?;
        let inner_map = expect_string_map(inner, &format!("{label} target `{target_key}`"))?;
        for (name, path) in &inner_map {
            validate_bin_name(name)?;
            validate_relative_package_path(path, &format!("bin `{name}`"))?;
        }
        out.insert(target_key.clone(), inner_map);
    }
    Ok(out)
}

fn validate_fixed_output(
    name: &str,
    fixed_output: bool,
    sources: &BTreeMap<String, Source>,
    deps: &Deps,
) -> Result<()> {
    if !fixed_output {
        return Ok(());
    }
    if sources.is_empty() {
        bail!("fixed-output package `{name}` must declare at least one source");
    }
    if !deps.build.is_empty() {
        bail!("fixed-output package `{name}` must not declare build deps");
    }
    Ok(())
}

pub(crate) fn parse_sources(value: &Value) -> Result<BTreeMap<String, Source>> {
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
        let platform = optional_string(source, "platform")?;
        let host_libc = optional_string(source, "host_libc")?;
        out.insert(
            name.clone(),
            Source {
                url,
                sha256,
                platform,
                host_libc,
            },
        );
    }
    Ok(out)
}

#[cfg(test)]
#[path = "package_tests.rs"]
mod tests;
