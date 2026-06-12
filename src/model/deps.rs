//! Dependency requirements: `deps.build`/`deps.runtime` parsing, platform-conditional
//! bracket syntax, and target-glob matching.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

use anyhow::{Context, Result, bail};
use nu_protocol::{Record, Span, Value};
use semver::VersionReq;

use super::*;

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

impl Deps {
    pub(crate) fn to_value(&self) -> Value {
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

/// Whether `version` satisfies `req`, with one deliberate deviation from semver: a bare
/// requirement (`*`) also matches pre-release versions. The catalog pins unreleased
/// software as `-dev.YYYYMMDD` prereleases (rune-authoring.md version policy), and a plain
/// dependency on such a package must resolve — strict semver excludes prereleases from `*`,
/// which would make prerelease-only packages uninstallable by name. Explicit requirements
/// keep strict semver semantics: opting into a prerelease range stays deliberate.
pub fn req_matches(req: &semver::VersionReq, version: &semver::Version) -> bool {
    if *req == semver::VersionReq::STAR && !version.pre.is_empty() {
        return true;
    }
    req.matches(version)
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
pub(crate) fn glob_match(pattern: &str, text: &str) -> bool {
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

pub(crate) fn parse_deps(value: &Value) -> Result<Deps> {
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
pub(crate) fn parse_dependency_list(value: &Value, label: &str) -> Result<Vec<Dependency>> {
    let Value::List { vals, .. } = value else {
        bail!("{label} must be a list");
    };
    vals.iter()
        .map(|value| parse_dependency(value, label))
        .collect()
}

pub(crate) fn parse_dependency(value: &Value, label: &str) -> Result<Dependency> {
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

pub(crate) fn parse_version_requirement(
    value: Option<&Value>,
    label: &str,
    name: &str,
) -> Result<VersionReq> {
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
pub(crate) fn parse_bracket_syntax(val: &str) -> Result<(String, Option<String>)> {
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

/// Serializes dependencies back to the NUON list form `parse_dependency_list` accepts.
/// A bare name string when the requirement is `*` and there is no platform constraint;
/// bracket syntax `name[platform]` when the requirement is `*` but a platform is present;
/// otherwise a `{ name, version, platform? }` record.
pub(crate) fn dependency_list_value(deps: &[Dependency]) -> Value {
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
}
