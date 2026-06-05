//! `grm clean`: reclaim disk under the install root.
//!
//! Empties the source/archive/build caches and any leftover transaction staging directories
//! without touching installed packages, profiles, state, tomes, addenda, or the lockfile.
//! Everything removed here is reproducible from the original sources, so a later install just
//! re-fetches and re-verifies what it needs.

use anyhow::{Context, Result};
use std::{
    fs,
    path::{Path, PathBuf},
};
use walkdir::WalkDir;

use crate::{
    paths,
    progress::{report, status},
};

pub fn clean() -> Result<()> {
    let root = paths::install_root()?;
    let targets: [(&str, PathBuf); 4] = [
        ("cache/sources", paths::source_cache_dir()?),
        ("cache/archives", paths::archive_cache_dir()?),
        ("cache/builds", paths::build_output_dir()?),
        ("transactions", root.join("transactions")),
    ];

    let mut total_bytes: u64 = 0;
    let mut total_entries: u64 = 0;
    for (label, dir) in &targets {
        status(&format!("cleaning {label}"));
        let (bytes, entries) =
            clean_dir(dir).with_context(|| format!("clean {} ({})", label, dir.display()))?;
        total_bytes += bytes;
        total_entries += entries;
    }

    report(&format!(
        "cleaned {} {} ({})",
        total_entries,
        if total_entries == 1 {
            "entry"
        } else {
            "entries"
        },
        format_bytes(total_bytes)
    ));
    Ok(())
}

/// Removes every immediate child of `dir`, returning `(bytes_freed, entries_removed)`. A
/// missing `dir` is treated as already-clean. The directory itself is left in place so the
/// next install does not have to re-create it.
fn clean_dir(dir: &Path) -> Result<(u64, u64)> {
    if !dir.exists() {
        return Ok((0, 0));
    }
    let mut bytes = 0u64;
    let mut entries = 0u64;
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        bytes += path_size(&path);
        if entry.file_type()?.is_dir() {
            fs::remove_dir_all(&path).with_context(|| format!("remove {}", path.display()))?;
        } else {
            fs::remove_file(&path).with_context(|| format!("remove {}", path.display()))?;
        }
        entries += 1;
    }
    Ok((bytes, entries))
}

/// Sums regular-file sizes under `path` (or the file's own size if it is a file). Best-effort:
/// unreadable entries are skipped so a permissions glitch on a stray file does not abort the
/// whole clean.
fn path_size(path: &Path) -> u64 {
    WalkDir::new(path)
        .into_iter()
        .filter_map(Result::ok)
        .filter_map(|entry| entry.metadata().ok())
        .filter(|metadata| metadata.is_file())
        .map(|metadata| metadata.len())
        .sum()
}

fn format_bytes(bytes: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = 1024 * KIB;
    const GIB: u64 = 1024 * MIB;
    if bytes >= GIB {
        format!("{:.2} GiB", bytes as f64 / GIB as f64)
    } else if bytes >= MIB {
        format!("{:.2} MiB", bytes as f64 / MIB as f64)
    } else if bytes >= KIB {
        format!("{:.2} KiB", bytes as f64 / KIB as f64)
    } else {
        format!("{bytes} B")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_bytes_picks_a_unit_per_magnitude() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(512), "512 B");
        assert_eq!(format_bytes(2048), "2.00 KiB");
        assert_eq!(format_bytes(5 * 1024 * 1024), "5.00 MiB");
        assert_eq!(format_bytes(3 * 1024 * 1024 * 1024), "3.00 GiB");
    }
}
