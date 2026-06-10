//! Catalog state and manifests for tomes and addenda, plus the [`Catalog`] abstraction
//! `sync_common` drives them through.

use serde::{Deserialize, Serialize};

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
