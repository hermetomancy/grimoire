//! Host compiler-boundary discovery for strict managed source builds.
//!
//! Grimoire-managed build dependencies provide the normal userland (`make`, `sh`, `sed`, ...).
//! Until `core` carries a relocatable compiler toolchain, source builds may fall back only to an
//! explicit host compiler boundary discovered from `PATH` without spawning any tools.

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeSet,
    env, fs,
    io::{BufReader, Read},
    path::{Path, PathBuf},
    sync::OnceLock,
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
/// dependency and the managed path will win because it is prepended before these host tools.
const REQUIRED_GROUPS: &[(&str, &[&str])] =
    &[("C compiler", &["cc", "clang", "gcc"]), ("shell", &["sh"])];

const OPTIONAL_TOOLS: &[&str] = &[
    "c++",
    "clang++",
    "g++",
    "ld",
    "ld.bfd",
    "ld.gold",
    "lld",
    "ar",
    "ranlib",
    "strip",
    "as",
    "nm",
    "objdump",
    "objcopy",
    "readelf",
    "c++filt",
    "elfedit",
    "dwp",
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

/// A stable identity for the host build environment — currently the C compiler — folded into a
/// package's store hash so a build against a different toolchain resolves to a *different* store
/// path instead of colliding with one built elsewhere.
///
/// The identity is the resolved compiler's `--version` banner (its first line), which captures the
/// implementation (clang vs gcc) and version while staying identical across machines that share the
/// same toolchain, so a shared binary cache can still hit. Returns `None` when no host compiler
/// boundary is available: such a host cannot build from source anyway, so the installer treats a
/// published prebuilt as authoritative rather than gating it on a hash it cannot reproduce.
///
/// The result is computed once and cached for the process — `PATH` and the host compiler do not
/// change underneath a running command. Setting `GRIMOIRE_BUILD_ENV` overrides discovery with an
/// explicit identity, for reproducible builds across hosts or for pinning the toolchain in tests.
pub fn build_env_id() -> Option<String> {
    static CACHE: OnceLock<Option<String>> = OnceLock::new();
    CACHE.get_or_init(compute_build_env_id).clone()
}

fn compute_build_env_id() -> Option<String> {
    if let Some(override_id) = env::var_os("GRIMOIRE_BUILD_ENV") {
        let override_id = override_id.to_string_lossy().trim().to_string();
        if !override_id.is_empty() {
            return Some(override_id);
        }
    }
    let readiness = source_build_readiness().ok()?;
    if !readiness.is_ready() {
        return None;
    }
    // `cc` is the canonical alias inserted for whichever compiler was found (cc/clang/gcc).
    let cc = readiness.host_tools.iter().find(|tool| tool.name == "cc")?;
    // Hash a prefix of the compiler binary itself rather than spawning it (AGENTS.md §1a).
    // The first 64 KB captures enough of the binary's header and embedded strings to be a
    // stable fingerprint across identical compiler installations, while staying fast.
    let hash = hash_file_prefix(&cc.path, 64 * 1024).ok()?;
    Some(format!("cc:{hash}"))
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

/// Hashes the first `limit` bytes of `path` with SHA-256 and returns the hex digest.
fn hash_file_prefix(path: &Path, limit: usize) -> Result<String> {
    let file =
        fs::File::open(path).with_context(|| format!("open {} for hashing", path.display()))?;
    let mut reader = BufReader::new(file);
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 8192];
    let mut remaining = limit;

    while remaining > 0 {
        let to_read = buf.len().min(remaining);
        let n = reader
            .read(&mut buf[..to_read])
            .with_context(|| format!("read {} while hashing prefix", path.display()))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        remaining -= n;
    }

    Ok(format!("{:x}", hasher.finalize()))
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
