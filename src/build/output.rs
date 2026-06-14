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
            let Some(base) = path
                .file_name()
                .and_then(|n| n.to_str())
                .and_then(lib_base_name)
            else {
                continue;
            };
            libs.push(base);
        }
    }
    libs.sort();
    libs.dedup();
    libs
}

/// The base name of a library file or soname: `libfoo.so.1` → `foo`, `libbar.a` → `bar`,
/// `libbaz.dylib` → `baz`, `libfoo-1.2.so` → `foo-1.2`. `None` for anything not shaped like a
/// library. Shared by discovery and the linkage lint so a soname matches a discovered lib name.
pub(crate) fn lib_base_name(name: &str) -> Option<String> {
    let base = name.strip_prefix("lib")?;
    let base = if let Some(stripped) = base.strip_suffix(".a") {
        stripped
    } else if let Some(stripped) = base.strip_suffix(".dylib") {
        stripped
    } else if let Some(idx) = base.find(".so") {
        &base[..idx]
    } else {
        return None;
    };
    (!base.is_empty()).then(|| base.to_owned())
}

/// The dynamic libraries a single Mach-O or ELF image references (its `LC_LOAD_DYLIB`
/// install names or `DT_NEEDED` sonames). Empty for any other byte stream. Static linkage
/// leaves no reference here, so a linkage lint built on this narrows the host-leak class,
/// not closes it.
pub(crate) fn needed_libraries(data: &[u8]) -> Vec<String> {
    use object::elf::{FileHeader32, FileHeader64};
    use object::macho::{MachHeader32, MachHeader64};
    use object::{Endianness, FileKind};
    match FileKind::parse(data) {
        Ok(FileKind::Elf32) => elf_needed::<FileHeader32<Endianness>>(data),
        Ok(FileKind::Elf64) => elf_needed::<FileHeader64<Endianness>>(data),
        Ok(FileKind::MachO32) => macho_needed::<MachHeader32<Endianness>>(data),
        Ok(FileKind::MachO64) => macho_needed::<MachHeader64<Endianness>>(data),
        _ => Vec::new(),
    }
}

fn elf_needed<Elf: object::read::elf::FileHeader<Endian = object::Endianness>>(
    data: &[u8],
) -> Vec<String> {
    use object::read::elf::Dyn;
    let mut out = Vec::new();
    let Ok(header) = Elf::parse(data) else {
        return out;
    };
    let Ok(endian) = header.endian() else {
        return out;
    };
    let Ok(sections) = header.sections(endian, data) else {
        return out;
    };
    let Ok(Some((dynamic, index))) = sections.dynamic(endian, data) else {
        return out;
    };
    let Ok(strings) = sections.strings(endian, data, index) else {
        return out;
    };
    for entry in dynamic {
        if entry.tag32(endian) == Some(object::elf::DT_NEEDED)
            && let Ok(name) = entry.string(endian, strings)
        {
            out.push(String::from_utf8_lossy(name).into_owned());
        }
    }
    out
}

fn macho_needed<Mach: object::read::macho::MachHeader<Endian = object::Endianness>>(
    data: &[u8],
) -> Vec<String> {
    let mut out = Vec::new();
    let Ok(header) = Mach::parse(data, 0) else {
        return out;
    };
    let Ok(endian) = header.endian() else {
        return out;
    };
    let Ok(mut commands) = header.load_commands(endian, data, 0) else {
        return out;
    };
    while let Ok(Some(command)) = commands.next() {
        if let Ok(Some(dylib)) = command.dylib()
            && let Ok(name) = command.string(endian, dylib.dylib.name)
        {
            out.push(String::from_utf8_lossy(name).into_owned());
        }
    }
    out
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
    fn needed_libraries_parses_the_host_binary() {
        // The test binary is a real Mach-O/ELF image; the parser must read its dynamic
        // references without choking. A dynamically linked binary names the platform libc; a
        // fully static one legitimately names nothing.
        let exe = std::env::current_exe().expect("current exe");
        let data = fs::read(&exe).expect("read exe");
        let needed = needed_libraries(&data);
        #[cfg(target_os = "macos")]
        assert!(
            needed.iter().any(|n| n.contains("libSystem")),
            "a dynamic macOS binary must link libSystem: {needed:?}"
        );
        let _ = needed;
    }

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
