//! Host compiler-boundary discovery for strict managed source builds.
//!
//! Grimoire-managed build dependencies provide the normal userland (`make`, `sh`, `sed`, ...).
//! Until `core` carries a relocatable compiler toolchain, source builds may fall back only to an
//! explicit host compiler boundary discovered from `PATH` without spawning any tools.

use anyhow::{Context, Result, bail};
use std::{
    collections::BTreeSet,
    env, fs,
    path::{Path, PathBuf},
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostTool {
    pub name: String,
    pub path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceBuildReadiness {
    pub host_tools: Vec<HostTool>,
    pub missing_required: Vec<String>,
}

impl SourceBuildReadiness {
    pub fn is_ready(&self) -> bool {
        self.missing_required.is_empty()
    }
}

/// Host tools temporarily allowed into strict source builds. `sh` is included for the stage-0
/// bootstrap/tests; once `core` publishes a real shell, package runes should declare it as a build
/// dependency and the managed path will win because it is prepended before these host shims.
const REQUIRED_GROUPS: &[(&str, &[&str])] =
    &[("C compiler", &["cc", "clang", "gcc"]), ("shell", &["sh"])];

const OPTIONAL_TOOLS: &[&str] = &[
    "c++",
    "clang++",
    "g++",
    "ld",
    "ar",
    "ranlib",
    "strip",
    "install_name_tool",
    "lipo",
];

pub fn source_build_readiness() -> Result<SourceBuildReadiness> {
    let path = env::var_os("PATH").unwrap_or_default();
    source_build_readiness_in_path(&path)
}

pub fn source_build_host_tools() -> Result<Vec<HostTool>> {
    let readiness = source_build_readiness()?;
    if readiness.is_ready() {
        return Ok(readiness.host_tools);
    }
    bail!(
        "source builds need a host compiler boundary for now; missing {}. Install the missing host tool(s), then rerun `grm doctor`.",
        readiness.missing_required.join(", ")
    );
}

fn source_build_readiness_in_path(path: &std::ffi::OsStr) -> Result<SourceBuildReadiness> {
    let mut tools = Vec::new();
    let mut seen = BTreeSet::new();
    let mut missing_required = Vec::new();

    for (label, names) in REQUIRED_GROUPS {
        let Some(found) = find_first_tool(path, names)? else {
            missing_required.push((*label).to_owned());
            continue;
        };
        insert_tool_aliases(&mut tools, &mut seen, names, &found);
    }

    for name in OPTIONAL_TOOLS {
        if let Some(found) = find_tool(path, name)? {
            insert_tool(&mut tools, &mut seen, name, found);
        }
    }

    Ok(SourceBuildReadiness {
        host_tools: tools,
        missing_required,
    })
}

fn insert_tool_aliases(
    tools: &mut Vec<HostTool>,
    seen: &mut BTreeSet<String>,
    aliases: &[&str],
    found: &Path,
) {
    for alias in aliases {
        insert_tool(tools, seen, alias, found.to_path_buf());
    }
}

fn insert_tool(tools: &mut Vec<HostTool>, seen: &mut BTreeSet<String>, name: &str, path: PathBuf) {
    if seen.insert(name.to_owned()) {
        tools.push(HostTool {
            name: name.to_owned(),
            path,
        });
    }
}

fn find_first_tool(path: &std::ffi::OsStr, names: &[&str]) -> Result<Option<PathBuf>> {
    for name in names {
        if let Some(found) = find_tool(path, name)? {
            return Ok(Some(found));
        }
    }
    Ok(None)
}

fn find_tool(path: &std::ffi::OsStr, name: &str) -> Result<Option<PathBuf>> {
    for dir in env::split_paths(path) {
        for candidate in executable_candidates(&dir, name) {
            if is_executable_file(&candidate)
                .with_context(|| format!("inspect host tool candidate {}", candidate.display()))?
            {
                return Ok(Some(candidate));
            }
        }
    }
    Ok(None)
}

fn executable_candidates(dir: &Path, name: &str) -> Vec<PathBuf> {
    #[cfg(windows)]
    {
        let mut candidates = vec![dir.join(name)];
        if Path::new(name).extension().is_none() {
            let pathext = env::var_os("PATHEXT").unwrap_or_else(|| ".COM;.EXE;.BAT;.CMD".into());
            for ext in pathext.to_string_lossy().split(';') {
                if ext.is_empty() {
                    continue;
                }
                candidates.push(dir.join(format!("{name}{ext}")));
            }
        }
        candidates
    }

    #[cfg(not(windows))]
    {
        vec![dir.join(name)]
    }
}

fn is_executable_file(path: &Path) -> Result<bool> {
    let Ok(metadata) = fs::metadata(path) else {
        return Ok(false);
    };
    if !metadata.is_file() {
        return Ok(false);
    }
    Ok(is_executable(&metadata))
}

#[cfg(unix)]
fn is_executable(metadata: &fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;

    metadata.permissions().mode() & 0o111 != 0
}

#[cfg(windows)]
fn is_executable(_metadata: &fs::Metadata) -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;

    #[test]
    fn readiness_reports_missing_required_tools() {
        let readiness = source_build_readiness_in_path(std::ffi::OsStr::new("")).unwrap();

        assert_eq!(
            readiness.missing_required,
            vec!["C compiler".to_owned(), "shell".to_owned()]
        );
    }

    #[test]
    fn readiness_finds_compiler_aliases_and_shell() {
        let temp = tempfile::tempdir().unwrap();
        make_executable(&temp.path().join("clang"));
        make_executable(&temp.path().join("sh"));

        let readiness = source_build_readiness_in_path(temp.path().as_os_str()).unwrap();
        let names = readiness
            .host_tools
            .iter()
            .map(|tool| tool.name.as_str())
            .collect::<Vec<_>>();

        assert!(readiness.is_ready());
        assert!(names.contains(&"cc"));
        assert!(names.contains(&"clang"));
        assert!(names.contains(&"gcc"));
        assert!(names.contains(&"sh"));
    }

    fn make_executable(path: &Path) {
        File::create(path).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            let mut permissions = fs::metadata(path).unwrap().permissions();
            permissions.set_mode(0o755);
            fs::set_permissions(path, permissions).unwrap();
        }
    }
}
