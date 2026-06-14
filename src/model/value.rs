//! Shared NUON `Value` accessors and identifier/version/path validators used by every
//! model type's `from_value`/`to_value`.

use std::collections::BTreeMap;

use anyhow::{Context, Result, bail};
use nu_protocol::{Record, Span, Value};
use semver::Version;

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

pub fn expect_record(value: Value, label: &str) -> Result<Record> {
    match value {
        Value::Record { val, .. } => Ok(val.into_owned()),
        _ => bail!("{label} must be a record"),
    }
}

pub(crate) fn optional_string(record: &Record, field: &str) -> Result<Option<String>> {
    match record.get(field) {
        Some(Value::Nothing { .. }) | None => Ok(None),
        Some(value) => expect_string(value, &format!("package field `{field}`")).map(Some),
    }
}

pub(crate) fn optional_bool(record: &Record, field: &str) -> Result<Option<bool>> {
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

pub(crate) fn expect_string(value: &Value, label: &str) -> Result<String> {
    match value {
        Value::String { val, .. } => Ok(val.clone()),
        _ => bail!("{label} must be a string"),
    }
}

pub(crate) fn expect_string_map(value: &Value, label: &str) -> Result<BTreeMap<String, String>> {
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

pub(crate) fn expect_string_list(value: &Value, label: &str) -> Result<Vec<String>> {
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

pub fn string_list_value(items: &[String]) -> Value {
    Value::list(
        items
            .iter()
            .map(|item| Value::string(item, Span::unknown()))
            .collect(),
        Span::unknown(),
    )
}

pub(crate) fn string_map_value(items: &BTreeMap<String, String>) -> Value {
    let mut record = Record::new();
    for (key, value) in items {
        record.push(key, Value::string(value, Span::unknown()));
    }
    Value::record(record, Span::unknown())
}

pub(crate) fn string_map_of_maps_value(
    items: &BTreeMap<String, BTreeMap<String, String>>,
) -> Value {
    let mut record = Record::new();
    for (key, inner) in items {
        record.push(key, string_map_value(inner));
    }
    Value::record(record, Span::unknown())
}

pub(crate) fn required_field_string(record: &Record, label: &str, field: &str) -> Result<String> {
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

/// A bin name becomes a profile entry *file name* under `profiles/current/bin/` — a symlink
/// into the store — and is never interpreted as code. So, unlike package/tome
/// identifiers, a bin name only has to be a safe single path component that works on both
/// platforms. We allow the extra punctuation real command names use (notably `[` from coreutils)
/// but reject path separators, control characters, the `.`/`..` directory names, a leading `.`
/// (hidden entries), and the characters Windows forbids in file names so a name valid on one
/// platform cannot fail to install on another.
pub(crate) fn validate_bin_name(name: &str) -> Result<()> {
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

pub(crate) fn validate_ident(value: &str, label: &str) -> Result<()> {
    if !starts_valid(value)
        || !value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "_.+-".contains(c))
    {
        bail!("{label} `{value}` contains unsupported characters");
    }
    Ok(())
}

pub(crate) fn starts_valid(value: &str) -> bool {
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
    use crate::model::validate_tome_name;

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
}
