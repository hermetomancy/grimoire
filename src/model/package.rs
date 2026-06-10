//! Package metadata: the `package` record a rune exports, plus the build manifest a
//! `build` function may return and target-conditional resolution for `bins`.

use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, path::Path};

use anyhow::{Result, bail};
use nu_protocol::{Record, Span, Value};

use super::*;

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
    /// User-facing post-install notes ("add yourself to the docker group"), declared
    /// statically in the rune's `package` const or returned dynamically by its `build`
    /// function. Printed once after install and replayable via `grm info`.
    #[serde(default)]
    pub notes: Vec<String>,
}

/// A declared source artifact for a source build. Every source must carry a checksum so
/// it can be verified before the build consumes it (AGENTS.md §10.1).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Source {
    pub url: String,
    pub sha256: String,
    /// Optional platform glob (same syntax as dependency brackets, e.g. `macos-*`,
    /// `linux-x86_64-musl`). The source is fetched and hashed only for matching targets —
    /// how a fixed-output package pins different prebuilt artifacts per platform.
    #[serde(default)]
    pub platform: Option<String>,
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
        let notes = optional_string_list(&record, "notes")?;

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
            notes,
        })
    }

    /// Resolve the bin map for a concrete target: `default` → OS wildcard → exact triple,
    /// with each level overriding the previous.
    pub fn bins_for(&self, target: &str) -> BTreeMap<String, String> {
        resolve_target_conditional(&self.bins, target)
    }

    /// The declared sources that apply to `target`: platform-filtered sources whose glob does
    /// not match are excluded, exactly like platform-conditional deps. This filtered view is
    /// what gets fetched *and* what the store hash covers, so per-platform artifacts do not
    /// perturb other platforms' content addresses.
    pub fn sources_for(&self, target: &str) -> BTreeMap<String, Source> {
        self.sources
            .iter()
            .filter(|(_, source)| {
                source
                    .platform
                    .as_deref()
                    .is_none_or(|pattern| dep_matches_platform(pattern, target))
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

        let mut sources = Record::new();
        for (name, source) in &self.sources {
            let mut entry = Record::new();
            entry.push("url", Value::string(&source.url, Span::unknown()));
            entry.push("sha256", Value::string(&source.sha256, Span::unknown()));
            if let Some(platform) = &source.platform {
                entry.push("platform", Value::string(platform, Span::unknown()));
            }
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

pub(crate) fn parse_target_conditional_bins(
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
        out.insert(
            name.clone(),
            Source {
                url,
                sha256,
                platform,
            },
        );
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sources_for_filters_by_platform_glob() {
        let mut sources = BTreeMap::new();
        sources.insert(
            "everywhere".to_owned(),
            Source {
                url: "https://example.com/all.tar.gz".to_owned(),
                sha256: "sha256:aaa".to_owned(),
                platform: None,
            },
        );
        sources.insert(
            "mac-only".to_owned(),
            Source {
                url: "https://example.com/mac.tar.xz".to_owned(),
                sha256: "sha256:bbb".to_owned(),
                platform: Some("macos-*".to_owned()),
            },
        );
        let metadata = PackageMetadata {
            name: "stage0".to_owned(),
            version: "1.0.0".to_owned(),
            target: None,
            store_path: None,
            targets: Vec::new(),
            fixed_output: true,
            summary: None,
            bins: BTreeMap::new(),
            sources,
            deps: Deps::default(),
            build_flags: BTreeMap::new(),
            provides: Vec::new(),
            libs: Vec::new(),
            notes: Vec::new(),
        };
        let mac = metadata.sources_for("macos-aarch64-darwin");
        assert!(mac.contains_key("everywhere") && mac.contains_key("mac-only"));
        let linux = metadata.sources_for("linux-x86_64-musl");
        assert!(linux.contains_key("everywhere") && !linux.contains_key("mac-only"));
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
            notes: Vec::new(),
        };

        let resolved: Vec<_> = meta.bins_for("linux-x86_64-musl").into_keys().collect();
        assert_eq!(resolved, vec!["awk", "sed", "tar"]);

        let resolved: Vec<_> = meta.bins_for("linux-aarch64-gnu").into_keys().collect();
        assert_eq!(resolved, vec!["awk", "sed"]);

        let resolved: Vec<_> = meta.bins_for("macos-aarch64-darwin").into_keys().collect();
        assert_eq!(resolved, vec!["sed"]);
    }
}
