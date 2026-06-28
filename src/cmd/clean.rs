//! `grm clean`: reclaim disk under the install root.
//!
//! Everything reproducible leaves; everything the user asked for stays. Three phases:
//! orphaned dependency state nothing references (cached build dependencies, residue from a
//! failed multi-package install) is swept out of the install; generations older than `--keep`
//! (the current generation and the switch-back target always survive) and every store path no
//! retained generation references — including the directories store-preserving removal left
//! behind — are deleted; and the source/archive/build caches plus leftover transaction
//! staging directories are emptied. Installed packages, retained generations, tomes, addenda,
//! and the lockfile are untouched; a later install or build re-fetches what it needs.

use anyhow::{Context, Result, bail};
use std::{
    fs,
    path::{Path, PathBuf},
};
use walkdir::WalkDir;

use crate::{
    cli::CleanArgs,
    install, profile,
    util::output::{accent, faint, line, report, status},
    util::paths,
};

pub fn clean(args: CleanArgs) -> Result<()> {
    if args.dry_run {
        return dry_run_clean(&args);
    }
    // Orphaned dependency state first, so the generation built from the swept set is the one
    // garbage collection measures reachability against.
    status("sweeping unused dependencies");
    let mut world = install::InstalledWorld::load_default()?;
    let seeds: Vec<String> = world
        .iter()
        .filter(|state| !state.requested && !state.held)
        .map(|state| state.name.clone())
        .collect();
    let swept = install::sweep_orphans(&mut world, seeds)?;
    if !swept.is_empty() {
        let mut tx = install::Transaction::new();
        world.commit(&mut tx)?;
        install::finalize_state(&mut tx, &world)?;
        tx.commit();
    }

    status("collecting old generations and unreferenced store paths");
    let (freed_stores, freed_generations, store_bytes) = profile::collect_garbage(args.keep)?;

    let targets = cache_targets()?;

    let mut cache_bytes: u64 = 0;
    let mut cache_entries: u64 = 0;
    for (label, dir) in &targets {
        status(&format!("cleaning {label}"));
        let (bytes, entries) =
            clean_dir(dir).with_context(|| format!("clean {} ({})", label, dir.display()))?;
        cache_bytes += bytes;
        cache_entries += entries;
    }

    if swept.is_empty() && freed_stores == 0 && freed_generations == 0 && cache_entries == 0 {
        report("nothing to clean");
        return Ok(());
    }
    let mut parts = Vec::new();
    if !swept.is_empty() {
        parts.push(format!("{} unused package(s)", swept.len()));
    }
    if freed_generations > 0 {
        parts.push(format!("{freed_generations} old generation(s)"));
    }
    if freed_stores > 0 {
        parts.push(format!("{freed_stores} store path(s)"));
    }
    if cache_entries > 0 {
        parts.push(format!(
            "{cache_entries} cache {}",
            if cache_entries == 1 {
                "entry"
            } else {
                "entries"
            }
        ));
    }
    report(&format!(
        "cleaned {} {}",
        accent(&parts.join(", ")),
        faint(&format!("({})", format_bytes(store_bytes + cache_bytes)))
    ));
    Ok(())
}

/// `clean --dry-run`: every phase computed, nothing deleted. The sweep is simulated on an
/// in-memory copy of state, garbage collection through [`profile::plan_garbage`], and the
/// caches by a size-only walk.
fn dry_run_clean(args: &CleanArgs) -> Result<()> {
    let world = install::InstalledWorld::load_default()?;
    let states = world.to_states();
    let seeds: Vec<String> = states
        .iter()
        .filter(|state| !state.requested && !state.held)
        .map(|state| state.name.clone())
        .collect();
    let swept = install::simulate_orphan_sweep(&states, &[], &seeds);
    let (doomed_stores, old_generations, store_bytes) = profile::plan_garbage(args.keep)?;

    let targets = cache_targets()?;
    let mut cache_bytes = 0u64;
    let mut cache_entries = 0u64;
    for (_, dir) in &targets {
        if !dir.exists() {
            continue;
        }
        for entry in fs::read_dir(dir)? {
            cache_bytes += path_size(&entry?.path());
            cache_entries += 1;
        }
    }

    if swept.is_empty() && doomed_stores.is_empty() && old_generations == 0 && cache_entries == 0 {
        report("nothing to clean");
        return Ok(());
    }
    line("plan:");
    for name in &swept {
        line(&format!("  - {name} (unused dependency)"));
    }
    if old_generations > 0 {
        line(&format!("  - {old_generations} old generation(s)"));
    }
    for basename in &doomed_stores {
        line(&format!("  - store/{basename}"));
    }
    if cache_entries > 0 {
        line(&format!(
            "  - {cache_entries} cache entr{}",
            if cache_entries == 1 { "y" } else { "ies" }
        ));
    }
    line(&format!(
        "would reclaim {}",
        format_bytes(store_bytes + cache_bytes)
    ));
    Ok(())
}

/// The cache and transaction directories swept by both `clean` and `clean --dry-run`, paired
/// with the labels shown in output.
fn cache_targets() -> Result<[(&'static str, PathBuf); 5]> {
    let root = paths::install_root()?;
    Ok([
        ("cache/sources", paths::source_cache_dir()?),
        ("cache/archives", paths::archive_cache_dir()?),
        ("cache/builds", paths::build_output_dir()?),
        // The whole rune-meta tree, not just this version's subdirectory: stale versions'
        // entries are pure dead weight.
        ("cache/rune-meta", root.join("cache").join("rune-meta")),
        ("transactions", root.join("transactions")),
    ])
}

/// Removes every immediate child of `dir`, returning `(bytes_freed, entries_removed)`. A
/// missing `dir` is treated as already-clean. The directory itself is left in place so the
/// next install does not have to re-create it.
fn clean_dir(dir: &Path) -> Result<(u64, u64)> {
    if !dir.exists() {
        return Ok((0, 0));
    }
    if fs::symlink_metadata(dir)?.file_type().is_symlink() {
        bail!("refusing to clean `{}`: it is a symlink", dir.display());
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
        .follow_links(false)
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
