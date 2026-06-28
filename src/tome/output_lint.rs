//! Build-output lints run by `grm tome build` on each produced archive (distinct from the
//! schema linter in `lint.rs` that `grm tome lint` runs over rune source): the purity lint
//! (baked host path *strings*) and the linkage lint (dynamic references to host libraries).
//! Both warn by default and become fatal under `--strict`. They are complementary — the
//! purity lint catches a host path written into the output; the linkage lint catches a host
//! library bound *without* a baked path (configure-time feature detection, the LLVM-22 /
//! Homebrew-zstd class), which the purity scan cannot see.

use anyhow::{Result, bail};
use std::collections::HashSet;
use std::io::Read;
use std::path::Path;

use crate::build::output::{lib_base_name, needed_libraries};
use crate::model::IndexEntry;
use crate::util::output::warn;
use crate::util::paths;

/// Host path prefixes that must never appear in a store package's output: host
/// package-manager trees and the build's own ephemeral staging dir.
const IMPURE_PATTERNS: &[&str] = &[
    "/usr/local/",
    "/opt/homebrew/",
    "/opt/local/",
    "/nix/store/",
    "/var/folders/",
];

/// Every impure pattern `bytes` contains, in `IMPURE_PATTERNS` order. Returns *all*
/// matches, not the first: a benign hit (a `/usr/local/` string baked into some default)
/// must never shadow a real one (the build's own `/var/folders/` temp dir smeared through
/// a binary) in the same file. First-match-then-break once hid exactly that.
fn impure_patterns_in(bytes: &[u8]) -> Vec<&'static str> {
    IMPURE_PATTERNS
        .iter()
        .filter(|pattern| {
            bytes
                .windows(pattern.len())
                .any(|window| window == pattern.as_bytes())
        })
        .copied()
        .collect()
}

/// Post-build purity lint: scans every archive member for absolute host paths that should
/// never be baked into a store package — host package-manager prefixes and the build's own
/// ephemeral staging tree. A hit usually means the build linked a host library, baked a
/// host tool path, or embedded its own temp directory instead of the final store prefix.
/// Warns by default; `--strict` makes it fatal. The `.grimoire/` members (embedded rune
/// source) are exempt — comments legitimately mention such paths.
pub(super) fn archive_purity(archive: &Path, strict: bool) -> Result<()> {
    let file = std::fs::File::open(archive)?;
    let decoder = zstd::stream::read::Decoder::new(file)?;
    let mut tar = tar::Archive::new(decoder);
    let mut hits: Vec<(String, &str)> = Vec::new();
    for entry in tar.entries()? {
        let mut entry = entry?;
        let path = entry.path()?.display().to_string();
        if path.starts_with(".grimoire/") || path.starts_with("./.grimoire/") {
            continue;
        }
        let mut bytes = Vec::new();
        entry.read_to_end(&mut bytes)?;
        for pattern in impure_patterns_in(&bytes) {
            hits.push((path.clone(), pattern));
        }
        if hits.len() >= 8 {
            break;
        }
    }
    if hits.is_empty() {
        return Ok(());
    }
    let listing: Vec<String> = hits
        .iter()
        .map(|(path, pattern)| format!("{path} contains `{pattern}`"))
        .collect();
    if strict {
        bail!(
            "impure build output ({}); the package bakes host paths that will not exist on \
             other machines",
            listing.join("; ")
        );
    }
    for line in &listing {
        warn(&format!("impure build output: {line}"));
    }
    Ok(())
}

/// Library base names every binary may link without a declared dependency: the libc floor
/// (AGENTS §5) and the platform runtime. Matched against `lib_base_name` of a bare soname.
const LIBC_FLOOR: &[&str] = &[
    "c",
    "m",
    "pthread",
    "dl",
    "rt",
    "util",
    "resolv",
    "nsl",
    "crypt",
    "anl",
    "gcc_s",
    "ld-linux",
    "ld-linux-x86-64",
    "ld-linux-aarch64",
    "ld-musl-x86_64",
    "ld-musl-aarch64",
    "System",
    "c++",
    "c++abi",
    "objc",
    "dyld",
];

/// Absolute Mach-O install-name prefixes that are the macOS platform floor (dyld shared cache
/// and SDK), allowed without a declared dependency. Deliberately narrow: arbitrary `/usr/lib`
/// libraries (zlib, ncurses, libedit, libiconv, …) are *not* the floor — §5 requires those packaged
/// in the store or eliminated at the source. Adding a `/usr/lib` library here is an exception, and
/// per the prime directive an exception is a last resort, never a shortcut around packaging work.
const MACOS_SYSTEM_PREFIXES: &[&str] = &[
    "/usr/lib/libSystem",
    "/usr/lib/system/",
    "/usr/lib/libc++",
    "/usr/lib/libobjc",
    "/System/Library/",
];

/// Archive paths whose members are scanned for dynamic-library references.
fn is_link_scanned(path: &str) -> bool {
    let path = path.strip_prefix("./").unwrap_or(path);
    ["bin/", "sbin/", "libexec/", "lib/", "lib64/"]
        .iter()
        .any(|dir| path.starts_with(dir))
}

/// Whether a dynamic-library reference `name` (a `DT_NEEDED` soname or `LC_LOAD_DYLIB` install
/// name) will bind to a *host* library at runtime — the violation. A reference is fine when it
/// is package-internal (`@rpath`/`@loader_path`), an absolute store path, a macOS platform-floor
/// library, the libc floor, or provided by a managed (installed) package. Anything else resolves
/// from the host loader, which is exactly the impurity the lint exists to catch.
fn links_off_store(name: &str, store_root: Option<&Path>, managed: &HashSet<String>) -> bool {
    if name.starts_with("@rpath")
        || name.starts_with("@loader_path")
        || name.starts_with("@executable_path")
    {
        return false;
    }
    if name.starts_with('/') {
        if let Some(root) = store_root
            && Path::new(name).starts_with(root)
        {
            return false;
        }
        return !MACOS_SYSTEM_PREFIXES
            .iter()
            .any(|prefix| name.starts_with(prefix));
    }
    match lib_base_name(name) {
        Some(base) => !(LIBC_FLOOR.contains(&base.as_str()) || managed.contains(&base)),
        None => false,
    }
}

/// Post-build linkage lint: parses every Mach-O / ELF binary in the archive and flags dynamic
/// references that will bind to a host library at runtime — a linked library that is neither the
/// libc floor nor provided by any managed (installed) package, or an absolute host install name.
/// This catches the class the purity scan misses: a host library linked *without* a baked path
/// string. Warns by default; `--strict` makes it fatal. Static linkage leaves no dynamic
/// reference, so this narrows the host-leak class rather than closing it.
pub(super) fn archive_linkage(archive: &Path, entry: &IndexEntry, strict: bool) -> Result<()> {
    let store_root = paths::store_root().ok();
    // The managed library universe: every lib an installed package provides, plus this package's
    // own libs (a binary may link a sibling lib in the same package). Anything a binary links that
    // is outside this set and the libc floor is resolved from the host.
    let world = crate::install::InstalledWorld::load_default().unwrap_or_default();
    let mut managed: HashSet<String> = world.iter().flat_map(|state| state.libs.clone()).collect();
    managed.extend(entry.libs.iter().cloned());

    let file = std::fs::File::open(archive)?;
    let decoder = zstd::stream::read::Decoder::new(file)?;
    let mut tar = tar::Archive::new(decoder);
    let mut hits: Vec<String> = Vec::new();
    for member in tar.entries()? {
        let mut member = member?;
        let path = member.path()?.display().to_string();
        if !is_link_scanned(&path) {
            continue;
        }
        let mut bytes = Vec::new();
        member.read_to_end(&mut bytes)?;
        for lib in needed_libraries(&bytes) {
            if links_off_store(&lib, store_root.as_deref(), &managed) {
                hits.push(format!("{path} links `{lib}`"));
            }
        }
        if hits.len() >= 8 {
            break;
        }
    }
    if hits.is_empty() {
        return Ok(());
    }
    if strict {
        bail!(
            "undeclared host linkage ({}); the package links libraries that are neither a \
             declared dependency nor the libc floor and will bind to host libraries at runtime",
            hits.join("; ")
        );
    }
    for hit in &hits {
        warn(&format!("undeclared host linkage: {hit}"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_benign_match_does_not_shadow_a_real_one_in_the_same_file() {
        // The exact shape that masked the bug: a nushell binary carrying both the benign
        // XDG default string and the build's own temp dir.
        let bytes = b"...XDG_DATA_DIRS /usr/local/share/:/usr/share/ ... \
                      /var/folders/yk/T/.tmpLm8MUZ/work/.grimoire-home/.cargo/foo.rs ...";
        let matched = impure_patterns_in(bytes);
        assert!(matched.contains(&"/usr/local/"), "{matched:?}");
        assert!(
            matched.contains(&"/var/folders/"),
            "the real leak must not be shadowed by the benign one: {matched:?}"
        );
    }

    #[test]
    fn a_remapped_clean_binary_reports_nothing() {
        let bytes = b"/grimoire-build/.grimoire-home/.cargo/registry/src/foo-1.0/src/lib.rs";
        assert!(impure_patterns_in(bytes).is_empty());
    }

    #[test]
    fn linkage_flags_host_libs_not_in_the_managed_set() {
        let store = Path::new("/grm/store");
        let managed: HashSet<String> = ["zstd".to_string()].into_iter().collect();
        let off = |name: &str| links_off_store(name, Some(store), &managed);

        // libc floor and managed store libs are fine.
        assert!(!off("libc.so.6"), "libc is the floor");
        assert!(!off("libpthread.so.0"), "pthread is the floor");
        assert!(!off("libzstd.so.1"), "zstd is a managed (installed) lib");
        // A package-internal or store-resident reference is fine.
        assert!(!off("@rpath/libfoo.dylib"), "@rpath is package-internal");
        assert!(
            !off("/grm/store/aaaa-zstd/lib/libzstd.1.dylib"),
            "an absolute store install name is fine"
        );
        // macOS platform floor is fine; arbitrary host libraries are not.
        assert!(!off("/usr/lib/libSystem.B.dylib"), "libSystem is the floor");
        assert!(!off("/usr/lib/libc++.1.dylib"), "libc++ is the floor");
        assert!(
            off("/usr/lib/libz.dylib"),
            "host /usr/lib zlib must be a store dep (§5); it packages cleanly"
        );
        assert!(
            off("/usr/lib/libedit.3.dylib"),
            "host libedit must be packaged, not allowlisted"
        );
        assert!(
            off("/opt/homebrew/lib/libpng.dylib"),
            "homebrew lib is a host leak"
        );
        // An undeclared, unmanaged soname binds to the host loader cache.
        assert!(off("libpng16.so.16"), "png is neither floor nor managed");
    }
}
