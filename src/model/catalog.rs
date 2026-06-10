//! Catalog state and manifests for tomes and addenda, plus the [`Catalog`] abstraction
//! `sync_common` drives them through.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

use anyhow::{Result, bail};
use nu_protocol::{Record, Span, Value};

use super::*;

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
    /// The highest news item id (filename) already shown to the user; items sorting above it
    /// are printed on the next `grm tome update` / `grm tome news`.
    #[serde(default)]
    pub last_seen_news: Option<String>,
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

/// Shared interface for catalog state (tomes and addenda).
///
/// Both tomes and addenda follow the same lifecycle — clone, validate, promote, record —
/// and carry identical metadata fields. This trait lets `sync_common.rs` operate on either
/// without duplicating the CRUD logic.
pub trait Catalog: Clone {
    type Manifest: CatalogManifest;
    const ENTITY_KIND: &'static str;
    const SUBDIR: &'static str;

    fn name(&self) -> &str;
    fn url(&self) -> &str;
    fn ref_name(&self) -> &str;
    fn signer_pubkeys(&self) -> &[String];

    fn set_checked_ref(&mut self, v: Option<String>);
    fn set_checked_commit(&mut self, v: Option<String>);
    fn set_manifest(&mut self, v: Option<Self::Manifest>);
    fn set_signer_pubkeys(&mut self, v: Vec<String>);

    fn from_nuon(value: Value) -> Result<Self>;
    fn to_nuon(&self) -> Value;
}

/// Shared interface for catalog manifests.
pub trait CatalogManifest: Clone {
    fn name(&self) -> &str;
    fn signers(&self) -> &[String];
}

impl Catalog for AddendumState {
    type Manifest = AddendumManifest;
    const ENTITY_KIND: &'static str = "addendum";
    const SUBDIR: &'static str = "addendums";

    fn name(&self) -> &str {
        &self.name
    }
    fn url(&self) -> &str {
        &self.url
    }
    fn ref_name(&self) -> &str {
        &self.ref_name
    }
    fn signer_pubkeys(&self) -> &[String] {
        &self.signer_pubkeys
    }

    fn set_checked_ref(&mut self, v: Option<String>) {
        self.checked_ref = v;
    }
    fn set_checked_commit(&mut self, v: Option<String>) {
        self.checked_commit = v;
    }
    fn set_manifest(&mut self, v: Option<Self::Manifest>) {
        self.addendum = v;
    }
    fn set_signer_pubkeys(&mut self, v: Vec<String>) {
        self.signer_pubkeys = v;
    }

    fn from_nuon(value: Value) -> Result<Self> {
        Self::from_value(value)
    }
    fn to_nuon(&self) -> Value {
        self.to_value()
    }
}

impl CatalogManifest for AddendumManifest {
    fn name(&self) -> &str {
        &self.name
    }
    fn signers(&self) -> &[String] {
        &self.signers
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

impl Catalog for TomeState {
    type Manifest = TomeManifest;
    const ENTITY_KIND: &'static str = "tome";
    const SUBDIR: &'static str = "tomes";

    fn name(&self) -> &str {
        &self.name
    }
    fn url(&self) -> &str {
        &self.url
    }
    fn ref_name(&self) -> &str {
        &self.ref_name
    }
    fn signer_pubkeys(&self) -> &[String] {
        &self.signer_pubkeys
    }

    fn set_checked_ref(&mut self, v: Option<String>) {
        self.checked_ref = v;
    }
    fn set_checked_commit(&mut self, v: Option<String>) {
        self.checked_commit = v;
    }
    fn set_manifest(&mut self, v: Option<Self::Manifest>) {
        self.tome = v;
    }
    fn set_signer_pubkeys(&mut self, v: Vec<String>) {
        self.signer_pubkeys = v;
    }

    fn from_nuon(value: Value) -> Result<Self> {
        Self::from_value(value)
    }
    fn to_nuon(&self) -> Value {
        self.to_value()
    }
}

impl CatalogManifest for TomeManifest {
    fn name(&self) -> &str {
        &self.name
    }
    fn signers(&self) -> &[String] {
        &self.signers
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
            last_seen_news: optional_string(&record, "last_seen_news")?,
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
        if let Some(last_seen_news) = &self.last_seen_news {
            record.push(
                "last_seen_news",
                Value::string(last_seen_news, Span::unknown()),
            );
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
