//! Reclaiming space: generation retention and store-path reachability from retained
//! generations. The `grm clean` half that touches durable state; the cache half lives in
//! `cmd::clean`.

use anyhow::Result;
use std::{collections::BTreeSet, fs, path::Path};

use crate::{
    util::paths,
    util::progress::{accent, faint, report, warn},
};

use super::*;

/// Collects unreferenced store paths and old generations, returning
/// `(freed_store_paths, freed_generations, bytes_freed)`.
///
/// Keeps the `keep` most recent generations (including the current one and the rollback
/// target), deletes older generations, then deletes any store path not referenced by a
/// retained generation. With no generations at all nothing is touched: store reachability is
/// derived from generation metadata, so an empty registry must not condemn the whole store.
pub fn collect_garbage(keep: usize) -> Result<(usize, usize, u64)> {
    let mut generations = list_generations()?;
    generations.sort_by_key(|b| std::cmp::Reverse(b.id));

    if generations.is_empty() {
        return Ok((0, 0, 0));
    }

    let current = current_generation_id()?;
    let mut to_retain: BTreeSet<u64> = generations.iter().take(keep).map(|g| g.id).collect();
    if let Some(current_id) = current {
        to_retain.insert(current_id);
        // Always keep the rollback target too: with `--keep 1` the previous generation would
        // otherwise be collected and the next `grm rollback` would have nowhere to go.
        if let Some(previous) = generations
            .iter()
            .map(|g| g.id)
            .filter(|id| *id < current_id)
            .max()
        {
            to_retain.insert(previous);
        }
    }

    let freed_generations = delete_old_generations(&generations, &to_retain, current)?;
    prune_registry()?;
    let referenced = collect_referenced_paths(&to_retain)?;
    let (freed_stores, freed_bytes) = collect_unreferenced_stores(&referenced)?;

    Ok((freed_stores, freed_generations, freed_bytes))
}

/// Pure version of [`collect_garbage`]: what *would* be reclaimed, with nothing deleted.
/// Returns `(unreferenced_store_paths, old_generations, bytes)`.
pub fn plan_garbage(keep: usize) -> Result<(Vec<String>, usize, u64)> {
    let mut generations = list_generations()?;
    generations.sort_by_key(|b| std::cmp::Reverse(b.id));
    if generations.is_empty() {
        return Ok((Vec::new(), 0, 0));
    }

    let current = current_generation_id()?;
    let mut to_retain: BTreeSet<u64> = generations.iter().take(keep).map(|g| g.id).collect();
    if let Some(current_id) = current {
        to_retain.insert(current_id);
        if let Some(previous) = generations
            .iter()
            .map(|g| g.id)
            .filter(|id| *id < current_id)
            .max()
        {
            to_retain.insert(previous);
        }
    }

    let old_generations = generations
        .iter()
        .filter(|g| !to_retain.contains(&g.id) && Some(g.id) != current)
        .filter(|g| generation_dir(g.id).map(|d| d.exists()).unwrap_or(false))
        .count();

    let referenced = collect_referenced_paths(&to_retain)?;
    let store_root = paths::store_root()?;
    let mut doomed = Vec::new();
    let mut bytes = 0u64;
    if store_root.exists() {
        for entry in fs::read_dir(&store_root)? {
            let path = entry?.path();
            if referenced.contains(&path.display().to_string()) {
                continue;
            }
            bytes += du(&path)?;
            doomed.push(
                path.file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned(),
            );
        }
    }
    Ok((doomed, old_generations, bytes))
}

pub(crate) fn delete_old_generations(
    generations: &[Generation],
    to_retain: &BTreeSet<u64>,
    current: Option<u64>,
) -> Result<usize> {
    let mut freed = 0;
    for g in generations {
        if to_retain.contains(&g.id) {
            continue;
        }
        // Never delete the current generation unless explicitly told to
        if Some(g.id) == current {
            continue;
        }
        let dir = generation_dir(g.id)?;
        if dir.exists() {
            fs::remove_dir_all(&dir)?;
            freed += 1;
        }
    }
    if freed > 0 {
        report(&format!("removed {freed} old generation(s)"));
    }
    Ok(freed)
}

pub(crate) fn prune_registry() -> Result<()> {
    let mut registry = read_registry().unwrap_or_default();
    let before = registry.len();
    registry.retain(|g| generation_dir(g.id).map(|d| d.exists()).unwrap_or(false));
    if registry.len() != before
        && let Err(e) = write_registry(&registry)
    {
        warn(&format!("could not write generations registry: {e:#}"));
    }
    Ok(())
}

pub(crate) fn collect_referenced_paths(to_retain: &BTreeSet<u64>) -> Result<BTreeSet<String>> {
    let mut referenced: BTreeSet<String> = BTreeSet::new();
    for id in to_retain {
        let dir = generation_dir(*id)?;
        let meta = dir.join("gen.nuon");
        if meta.exists()
            && let Ok(g) = read_generation_metadata(&dir)
        {
            referenced.extend(g.store_paths);
        }
    }
    // On the shared `/grm/store`, every other user's generations are GC roots too: this user's
    // `clean` must never reclaim a store path another user's generation still links to (§9).
    referenced.extend(foreign_referenced_paths());
    Ok(referenced)
}

/// Store paths referenced by *other users'* generations under the shared `/grm/profiles/*`. We
/// cannot know another user's retention policy, so every generation they have is treated as a
/// root. Returns an empty set for a `GRIMOIRE_ROOT`-isolated install (single owner) or when the
/// shared profiles root cannot be read. Best-effort by design — a foreign profile we cannot read
/// simply contributes no roots, and the worst case is retaining a path slightly longer.
fn foreign_referenced_paths() -> BTreeSet<String> {
    // An isolated install owns its whole store; only the fixed `/grm` store is shared across users.
    if std::env::var_os("GRIMOIRE_ROOT").is_some() {
        return BTreeSet::new();
    }
    let mine = paths::profiles_dir().ok();
    referenced_paths_under(Path::new("/grm/profiles"), mine.as_deref())
}

/// Collects the store paths named by every `gen-*/gen.nuon` under each user directory in
/// `profiles_root`, skipping `exclude` (the current user, whose retained generations are gathered
/// by the normal path). Pure traversal — unreadable entries are skipped rather than failing.
fn referenced_paths_under(profiles_root: &Path, exclude: Option<&Path>) -> BTreeSet<String> {
    let mut referenced = BTreeSet::new();
    let Ok(users) = fs::read_dir(profiles_root) else {
        return referenced;
    };
    for user in users.flatten() {
        let user_dir = user.path();
        if Some(user_dir.as_path()) == exclude || !user_dir.is_dir() {
            continue;
        }
        let Ok(generations) = fs::read_dir(&user_dir) else {
            continue;
        };
        for generation in generations.flatten() {
            let name = generation.file_name();
            let Some(name) = name.to_str() else { continue };
            if name == "current" || !name.starts_with("gen-") {
                continue;
            }
            let dir = generation.path();
            if dir.join("gen.nuon").exists()
                && let Ok(meta) = read_generation_metadata(&dir)
            {
                referenced.extend(meta.store_paths);
            }
        }
    }
    referenced
}

pub(crate) fn collect_unreferenced_stores(referenced: &BTreeSet<String>) -> Result<(usize, u64)> {
    let store_root = paths::store_root()?;
    if !store_root.exists() {
        return Ok((0, 0));
    }

    let mut freed = 0;
    let mut bytes = 0u64;
    for entry in fs::read_dir(&store_root)? {
        let entry = entry?;
        let path = entry.path();
        let path_str = path.display().to_string();
        if referenced.contains(&path_str) {
            continue;
        }
        let size = du(&path)?;
        fs::remove_dir_all(&path)?;
        report(&format!(
            "collected {} {}",
            accent(&path.file_name().unwrap_or_default().to_string_lossy()),
            faint(&format!("({:.2} MiB)", size as f64 / (1024.0 * 1024.0)))
        ));
        freed += 1;
        bytes += size;
    }
    Ok((freed, bytes))
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Rough disk usage of a directory in bytes: sums regular files, skipping symlinks (so it
/// never follows a generation symlink out of the tree into the store).
pub(crate) fn du(path: &Path) -> Result<u64> {
    let mut total = 0u64;
    for entry in walkdir::WalkDir::new(path) {
        let entry = entry?;
        if entry.file_type().is_file() {
            total += entry.metadata()?.len();
        }
    }
    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn foreign_generations_are_gc_roots() {
        let root = tempfile::tempdir().expect("tempdir");
        let profiles = root.path();

        // Two foreign users, each with a generation referencing a store path, plus the current
        // user (excluded) and assorted noise that must be ignored.
        let write_gen = |user: &str, gen_name: &str, store_paths: &[&str]| {
            let dir = profiles.join(user).join(gen_name);
            std::fs::create_dir_all(&dir).unwrap();
            let generation = Generation {
                id: 1,
                created: 0,
                packages: vec!["pkg".to_string()],
                store_paths: store_paths.iter().map(|s| s.to_string()).collect(),
            };
            write_generation_metadata(&dir, &generation).unwrap();
        };
        write_gen("alice", "gen-1", &["/grm/store/aaaa-pkg-1.0"]);
        write_gen("bob", "gen-3", &["/grm/store/bbbb-pkg-2.0"]);
        write_gen("me", "gen-1", &["/grm/store/cccc-mine-1.0"]);
        // Noise: a non-generation dir and a gen dir without gen.nuon are both ignored.
        std::fs::create_dir_all(profiles.join("alice").join("current")).unwrap();
        std::fs::create_dir_all(profiles.join("carol").join("gen-9")).unwrap();

        let referenced = referenced_paths_under(profiles, Some(&profiles.join("me")));
        assert!(referenced.contains("/grm/store/aaaa-pkg-1.0"));
        assert!(referenced.contains("/grm/store/bbbb-pkg-2.0"));
        assert!(
            !referenced.contains("/grm/store/cccc-mine-1.0"),
            "the excluded current user must not be folded in here"
        );
        assert_eq!(referenced.len(), 2);
    }

    #[test]
    fn missing_profiles_root_yields_no_roots() {
        let root = tempfile::tempdir().expect("tempdir");
        let absent = root.path().join("does-not-exist");
        assert!(referenced_paths_under(&absent, None).is_empty());
    }
}
