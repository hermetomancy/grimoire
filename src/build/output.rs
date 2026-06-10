//! Post-build inspection of the staged package: discovering produced bins and shared
//! libraries, and normalising executable permissions.

use anyhow::{Context, Result};
use std::{collections::BTreeMap, path::Path};
/// Scan a built package directory for executable files in standard directories.
/// Returns a map of command name → relative path (e.g. "hello" → "bin/hello").
pub(super) fn discover_bins(package_dir: &Path) -> BTreeMap<String, String> {
    let mut bins = BTreeMap::new();
    for subdir in ["bin", "sbin", "libexec"] {
        let dir = package_dir.join(subdir);
        if !dir.is_dir() {
            continue;
        }
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            let Ok(meta) = entry.metadata() else {
                continue;
            };
            let is_exec = {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    meta.permissions().mode() & 0o111 != 0
                }
                #[cfg(not(unix))]
                {
                    true
                }
            };
            if !is_exec {
                continue;
            }
            if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                bins.insert(name.to_owned(), format!("{subdir}/{name}"));
            }
        }
    }
    bins
}

/// Scan a built package directory for library files in standard directories.
/// Returns a sorted list of library base names (e.g. "foo" for libfoo.so).
pub(super) fn discover_libs(package_dir: &Path) -> Vec<String> {
    let mut libs = Vec::new();
    for subdir in ["lib", "lib64"] {
        let dir = package_dir.join(subdir);
        if !dir.is_dir() {
            continue;
        }
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            let Some(base) = name.strip_prefix("lib") else {
                continue;
            };
            let base = if let Some(stripped) = base.strip_suffix(".a") {
                stripped
            } else if let Some(stripped) = base.strip_suffix(".dylib") {
                stripped
            } else if let Some(idx) = base.find(".so") {
                &base[..idx]
            } else {
                continue;
            };
            if !base.is_empty() {
                libs.push(base.to_owned());
            }
        }
    }
    libs.sort();
    libs.dedup();
    libs
}

/// Ensure every file in `bin/`, `sbin/`, and `libexec/` is executable.
/// Build scripts that use Nushell's `save` command create files without the
/// executable bit; this fixes them before packing so auto-discovery finds them.
pub(super) fn fix_bin_permissions(package_dir: &Path) -> Result<()> {
    for subdir in ["bin", "sbin", "libexec"] {
        let dir = package_dir.join(subdir);
        if !dir.is_dir() {
            continue;
        }
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mut perms = entry
                    .metadata()
                    .with_context(|| format!("read metadata for {}", path.display()))?
                    .permissions();
                if perms.mode() & 0o111 == 0 {
                    perms.set_mode(perms.mode() | 0o755);
                    std::fs::set_permissions(&path, perms)
                        .with_context(|| format!("chmod +x {}", path.display()))?;
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::{self, File};

    #[test]
    fn discover_libs_handles_various_names() {
        let temp = tempfile::tempdir().unwrap();
        let lib_dir = temp.path().join("lib");
        fs::create_dir(&lib_dir).unwrap();

        // Standard names
        File::create(lib_dir.join("libfoo.so")).unwrap();
        File::create(lib_dir.join("libfoo.so.1")).unwrap();
        File::create(lib_dir.join("libfoo.so.1.2.3")).unwrap();
        File::create(lib_dir.join("libbar.a")).unwrap();
        File::create(lib_dir.join("libbaz.dylib")).unwrap();

        // Version-in-base-name (must NOT be mangled)
        File::create(lib_dir.join("libfoo-1.2.so")).unwrap();

        let libs = discover_libs(temp.path());
        assert!(
            libs.contains(&"foo".to_string()),
            "foo should be discovered"
        );
        assert!(
            libs.contains(&"bar".to_string()),
            "bar should be discovered"
        );
        assert!(
            libs.contains(&"baz".to_string()),
            "baz should be discovered"
        );
        assert!(
            libs.contains(&"foo-1.2".to_string()),
            "foo-1.2 should preserve version in base name"
        );
    }
}
