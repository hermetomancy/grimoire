//! Host compiler-boundary discovery for strict managed source builds.
//!
//! Grimoire-managed build dependencies provide the normal userland (`make`, `sh`, etc.).
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

/// A stable identity for the host build environment folded into a package's store hash so a build
/// against a different toolchain resolves to a *different* store path instead of colliding with one
/// built elsewhere.
///
/// The identity is derived from each tool's `--version` banner (first line only). This captures
/// the implementation family and major/minor version while staying identical across machines that
/// share the same toolchain release, so a shared binary cache can still hit. A machine with a
/// different compiler patch level but the same version string will produce the same hash — this is
/// intentional: the store hash is an input hash, and the actual archive content is verified
/// independently.
///
/// Returns `None` when no host compiler boundary is available: such a host cannot build from source
/// anyway, so the installer treats a published prebuilt as authoritative rather than gating it on a
/// hash it cannot reproduce.
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
    let cc_ver = tool_version_string(&cc.path, "cc").ok()?;
    let mut parts = vec![format!("cc:{cc_ver}")];

    // Also capture the linker, assembler, and platform-specific post-link tools — anything that
    // affects the bytes in the final binary so that a different host boundary resolves to a
    // different store path instead of silently colliding.
    for tool in &readiness.host_tools {
        if tool.path == cc.path || !is_binary_affecting_tool(&tool.name) {
            continue;
        }
        if let Ok(ver) = tool_version_string(&tool.path, &tool.name) {
            parts.push(format!("{}:{ver}", tool.name));
        }
    }

    // On macOS the system SDK version affects headers, libraries, and binary output.
    if let Some(sdk_ver) = macos_sdk_version() {
        parts.push(format!("sdk:{sdk_ver}"));
    }

    parts.sort();
    Some(parts.join(","))
}

/// Tools beyond the compiler that influence the bytes in a built binary.
fn is_binary_affecting_tool(name: &str) -> bool {
    matches!(
        name,
        "ld" | "ld.bfd" | "ld.gold" | "lld" | "as" | "install_name_tool" | "lipo"
    )
}

/// Returns the macOS system SDK version (e.g. "15.2") when running on macOS with
/// `xcrun` available. This affects headers, libraries, and binary output, so it is
/// part of the build-environment identity.
fn macos_sdk_version() -> Option<String> {
    if env::consts::OS != "macos" {
        return None;
    }
    let output = std::process::Command::new("xcrun")
        .args(["--show-sdk-version"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let ver = stdout.lines().next()?.trim();
    if ver.is_empty() {
        return None;
    }
    Some(ver.to_string())
}

/// Runs `tool --version` (or `tool -version` for macOS tools) and returns the first line of stdout,
/// trimmed. Falls back to hashing the binary prefix if the tool does not produce version output.
fn tool_version_string(path: &Path, name: &str) -> Result<String> {
    let flag = if name == "install_name_tool" || name == "lipo" {
        "-version"
    } else {
        "--version"
    };

    let output = std::process::Command::new(path)
        .arg(flag)
        .output()
        .with_context(|| format!("spawn {} {} for version", path.display(), flag))?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let first_line = stdout.lines().next().unwrap_or("").trim();

    if !first_line.is_empty() {
        return Ok(first_line.to_string());
    }

    // Some tools (e.g. ancient `ld`) don't support --version. Fall back to a binary prefix hash
    // so we still have *some* stable identity.
    let hash = hash_file_prefix(path, 64 * 1024).with_context(|| {
        format!(
            "fallback hash of {} after no version output",
            path.display()
        )
    })?;
    Ok(format!("bin:{hash}"))
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
    vec![dir.join(name)]
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

fn is_executable(metadata: &fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;

    metadata.permissions().mode() & 0o111 != 0
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
        use std::os::unix::fs::PermissionsExt;

        let mut permissions = fs::metadata(path).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(path, permissions).unwrap();
    }
}
