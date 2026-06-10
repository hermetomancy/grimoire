//! User-chosen preferred providers for contested capabilities, stored as inert NUON in
//! `state/preferences.nuon` (§4): a flat map from capability name (e.g. `awk`) to the package
//! that should provide it (e.g. `gawk`). Consulted by the solver when expanding a capability
//! dependency and by generation linking when two installed packages declare the same bin.

use anyhow::{Context, Result};
use nu_protocol::{Record, Span, Value};
use std::{collections::BTreeMap, fs, path::PathBuf};

use crate::{
    model::{expect_record, expect_string_map},
    nu::nuon_io,
    util::paths,
};

#[derive(Debug, Clone, Default)]
pub struct Preferences {
    /// Capability name -> preferred provider package name.
    pub providers: BTreeMap<String, String>,
}

impl Preferences {
    pub fn from_value(value: Value) -> Result<Self> {
        let record = expect_record(value, "preferences")?;
        let providers = match record.get("providers") {
            Some(value) => expect_string_map(value, "preferences field `providers`")?,
            None => BTreeMap::new(),
        };
        Ok(Self { providers })
    }

    pub fn to_value(&self) -> Value {
        let mut providers = Record::new();
        for (capability, package) in &self.providers {
            providers.push(capability, Value::string(package, Span::unknown()));
        }
        let mut record = Record::new();
        record.push("format", Value::int(1, Span::unknown()));
        record.push("providers", Value::record(providers, Span::unknown()));
        Value::record(record, Span::unknown())
    }

    /// Loads the preferences file; a missing file is an empty set, not an error.
    pub fn load() -> Result<Self> {
        let path = preferences_path()?;
        if !path.exists() {
            return Ok(Self::default());
        }
        Self::from_value(nuon_io::read_nuon(&path)?)
            .with_context(|| format!("read preferences {}", path.display()))
    }

    pub fn save(&self) -> Result<()> {
        let path = preferences_path()?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        nuon_io::write_nuon(&path, &self.to_value())
    }
}

pub fn preferences_path() -> Result<PathBuf> {
    Ok(paths::install_root()?
        .join("state")
        .join("preferences.nuon"))
}
