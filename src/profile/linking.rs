//! Materializing a generation: linking declared bins (with `grm prefer` resolving contested
//! names) and share/ trees from the store. Every entry is a **symlink** to its absolute store
//! path. The store is the immutable, content-addressed source of truth and a generation is a
//! forest of pointers into it: GC roots come from each generation's recorded `store_paths`,
//! not link counts, so the symlinks never dangle under a correct GC, and cross-filesystem
//! installs work without a copy fallback. Absolute symlinks also make a binary's own
//! `@loader_path`/`current_exe` resolve back to the store, where `bin/` and `lib/` are
//! siblings — which a hard link into the bin-only generation dir would break (rust's `rustc`
//! finds `librustc_driver` and its sysroot that way).

use anyhow::{Context, Result, bail};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
};

use crate::{
    model::PackageState, model::preferences::Preferences, util::output::warn, util::paths,
};

/// Subdirectories scanned for human-facing artifacts (man pages, completions, desktop files)
/// that are not explicitly declared as bins.
pub(crate) const PROFILE_SHARE_SUBDIRS: &[&str] = &[
    "share/man",
    "share/bash-completion/completions",
    "share/zsh/site-functions",
    "share/fish/vendor_completions.d",
    "share/applications",
];

/// For each bin name declared by more than one package, applies the user's `grm prefer`
/// choice: the preferred package keeps the bin, every other claimant gets it added to its
/// skip set. A contested bin with no applicable preference is an error naming the contenders,
/// so the failure is order-independent and actionable.
pub(crate) fn contested_bin_skips(
    states: &[&PackageState],
) -> Result<BTreeMap<String, BTreeSet<String>>> {
    let mut owners: BTreeMap<&str, Vec<&PackageState>> = BTreeMap::new();
    for state in states {
        for bin_name in state.bins.keys() {
            owners.entry(bin_name).or_default().push(*state);
        }
    }
    let preferences = Preferences::load().unwrap_or_default();
    let mut skips: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for (bin_name, claimants) in owners {
        if claimants.len() < 2 {
            continue;
        }
        let preferred = preferences
            .providers
            .get(bin_name)
            .and_then(|p| claimants.iter().find(|s| s.name == *p));
        let Some(winner) = preferred else {
            let names: Vec<&str> = claimants.iter().map(|s| s.name.as_str()).collect();
            bail!(
                "bin `{bin_name}` is provided by multiple installed packages ({}); \
                 run `grm prefer {bin_name} <package>` to choose which one provides it",
                names.join(", ")
            );
        };
        for state in &claimants {
            if state.name != winner.name {
                skips
                    .entry(state.name.clone())
                    .or_default()
                    .insert(bin_name.to_owned());
            }
        }
    }
    Ok(skips)
}

pub(crate) fn link_package_into_generation(
    state: &PackageState,
    gen_dir: &Path,
    skip_bins: &BTreeSet<String>,
) -> Result<()> {
    let store_path = PathBuf::from(&state.store_path);
    let store_root = paths::store_root()?;
    if !store_path.starts_with(&store_root) {
        bail!(
            "package `{}` store path `{}` is outside the store root `{}`; refusing to link",
            state.name,
            store_path.display(),
            store_root.display()
        );
    }

    // Symlink declared bins into the generation's bin/ directory. The bin name in the profile
    // is the key from `state.bins`; the link target is the value resolved against the store.
    for (bin_name, bin_path) in &state.bins {
        // This package lost the contested bin to a `grm prefer` choice; the winner links it.
        if skip_bins.contains(bin_name) {
            continue;
        }
        let src = store_path.join(bin_path);
        if !src.exists() {
            warn(&format!(
                "declared bin `{bin_name}` points to missing file `{}` in {}",
                bin_path,
                store_path.display()
            ));
            continue;
        }
        let dst = gen_dir.join("bin").join(bin_name);
        // Presence of the link itself (symlink_metadata, not exists) so a present-but-broken
        // prior link is still reported as a collision rather than an opaque EEXIST below.
        if dst.symlink_metadata().is_ok() {
            // Backstop only: contested declared bins are resolved order-independently by
            // `contested_bin_skips` before linking starts.
            bail!(
                "bin `{bin_name}` from `{}` collides with an earlier package in this generation. \
                 To fix: run `grm prefer {bin_name} <package>` to choose a provider, or remove \
                 the other package.",
                state.name
            );
        }
        symlink_into_store(&src, &dst)?;
    }

    // Symlink human-facing artifacts under share/ (man pages, completions, etc.).
    for subdir in PROFILE_SHARE_SUBDIRS {
        let src = store_path.join(subdir);
        if !src.exists() {
            continue;
        }
        let dst = gen_dir.join(subdir);
        link_tree(&src, &dst)?;
    }
    Ok(())
}

/// Creates a symlink at `dst` pointing at the absolute store path `src`, creating parent
/// directories as needed. `src` is always an absolute path inside the immutable store, so the
/// link resolves the same content from any generation; for store entries that are themselves
/// symlinks (`gmake -> make`, relative man-page `.so` stubs) the link chain resolves through
/// to the real file.
fn symlink_into_store(src: &Path, dst: &Path) -> Result<()> {
    if let Some(parent) = dst.parent() {
        fs::create_dir_all(parent)?;
    }
    std::os::unix::fs::symlink(src, dst)
        .with_context(|| format!("symlink {} -> {}", dst.display(), src.display()))
}

/// Recursively symlinks every file under `src` (a store subtree) into `dst`, preserving the
/// directory structure. Each leaf — regular file or store symlink alike — becomes a symlink to
/// its absolute store path. Existing targets are replaced, so packages that contribute to the
/// same share/ tree (e.g. several packages' `share/man/man1`) merge.
pub(crate) fn link_tree(src: &Path, dst: &Path) -> Result<()> {
    for entry in walkdir::WalkDir::new(src) {
        let entry = entry?;
        let path = entry.path();
        if path == src {
            continue;
        }
        let relative = path
            .strip_prefix(src)
            .with_context(|| format!("strip prefix from {}", path.display()))?;
        let target = dst.join(relative);

        // walkdir does not follow symlinks, so a store symlink arrives here as a symlink entry
        // (not a dir) and is linked by absolute path like any other leaf.
        if entry.file_type().is_dir() {
            fs::create_dir_all(&target)?;
        } else {
            let _ = fs::remove_file(&target);
            symlink_into_store(path, &target)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;
    use tempfile::TempDir;

    #[test]
    fn link_tree_symlinks_each_leaf_to_its_absolute_store_path() {
        let store = TempDir::new().unwrap();
        let store = store.path();
        let gen_dir = TempDir::new().unwrap();
        let share = gen_dir.path().join("share");

        fs::create_dir_all(store.join("man1")).unwrap();
        fs::write(store.join("man1").join("tool.1"), b"page").unwrap();
        // A store symlink (e.g. a man-page alias) must be linked by absolute path and resolve
        // through to the real file, not recreated as a dangling relative link.
        symlink("tool.1", store.join("man1").join("alias.1")).unwrap();

        link_tree(store, &share).unwrap();

        let leaf = share.join("man1").join("tool.1");
        let meta = fs::symlink_metadata(&leaf).unwrap();
        assert!(meta.file_type().is_symlink(), "leaf must be a symlink");
        assert_eq!(
            fs::read_link(&leaf).unwrap(),
            store.join("man1").join("tool.1"),
            "leaf must point at its absolute store path"
        );

        let alias = share.join("man1").join("alias.1");
        assert!(
            fs::symlink_metadata(&alias)
                .unwrap()
                .file_type()
                .is_symlink(),
            "a store symlink must itself be linked, not dereferenced"
        );
        assert_eq!(
            fs::read_to_string(&alias).unwrap(),
            "page",
            "the alias must resolve through to the real file"
        );
    }
}
