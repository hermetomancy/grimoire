//! Profile and generation management.
//!
//! A profile is the user-facing view into the store. Each generation is a real directory tree
//! containing hard links into store paths. The active generation is selected by a single symlink:
//! `profiles/current -> gen-N`.
//!
//! Because Grimoire binaries bake absolute store paths (RPATH, install_name, pkg-config prefix),
//! generations only need to surface executables and human-facing artifacts: `bin/`, `share/man/`,
//! shell completions, and desktop files. Everything else stays in the store and is found via baked
//! absolute paths.

use anyhow::{Context, Result, bail};
use std::{collections::BTreeSet, fs, path::PathBuf};

use crate::{
    model::PackageState,
    util::paths,
    util::progress::{note, status, strong, success, warn},
};

mod gc;
mod generations;
mod linking;

pub use gc::*;
pub use generations::*;
pub(crate) use linking::*;

/// Metadata for a single generation, stored as `gen.nuon` inside the generation directory.
#[derive(Debug, Clone)]
pub struct Generation {
    pub id: u64,
    pub created: u64,
    pub packages: Vec<String>,
    pub store_paths: Vec<String>,
}

/// The directory that holds the actual generation trees (hard links into the store).
pub fn profiles_dir() -> Result<PathBuf> {
    paths::profiles_dir()
}

/// The user-facing profile directory that holds the `current` symlink.
pub fn user_profiles_dir() -> Result<PathBuf> {
    paths::user_profiles_dir()
}

/// The symlink that points to the active generation directory.
pub fn current_profile_link() -> Result<PathBuf> {
    Ok(user_profiles_dir()?.join("current"))
}

/// The directory for a specific generation.
pub fn generation_dir(id: u64) -> Result<PathBuf> {
    Ok(profiles_dir()?.join(format!("gen-{id}")))
}

/// Returns the list of package names in the currently active generation, if one exists.
pub fn current_generation_packages() -> Result<Option<Vec<String>>> {
    let Some(id) = current_generation_id()? else {
        return Ok(None);
    };
    let gen_dir = generation_dir(id)?;
    if !gen_dir.exists() {
        return Ok(None);
    }
    let metadata = read_generation_metadata(&gen_dir)?;
    Ok(Some(metadata.packages))
}

/// Returns the ID of the currently active generation, if one exists.
pub fn current_generation_id() -> Result<Option<u64>> {
    let link = current_profile_link()?;
    if !link.exists() {
        return Ok(None);
    }
    let target = fs::read_link(&link)
        .with_context(|| format!("read current profile link {}", link.display()))?;
    parse_generation_id(
        target
            .file_name()
            .and_then(|n| n.to_str())
            .context("current profile link target has no name")?,
    )
    .map(Some)
}

/// Whether the active generation no longer describes `states`' linked set. Package state
/// commits per-transaction but the generation builds once at the end of a command, so a
/// failure in between (a contested bin refusing the link) leaves state saying "installed"
/// while the environment disagrees. The no-op install path checks this instead of trusting
/// state alone, so re-running the command converges rather than reporting success.
pub fn current_generation_is_stale(states: &[PackageState]) -> Result<bool> {
    let linked_names = crate::install::linked_set(states);
    let mut want: Vec<(&str, &str)> = states
        .iter()
        .filter(|state| linked_names.contains(&state.name))
        .map(|state| (state.name.as_str(), state.store_path.as_str()))
        .collect();
    want.sort_unstable();

    let Some(id) = current_generation_id()? else {
        return Ok(!want.is_empty());
    };
    let gen_dir = generation_dir(id)?;
    if !gen_dir.exists() {
        return Ok(true);
    }
    let metadata = read_generation_metadata(&gen_dir)?;
    let mut have: Vec<(&str, &str)> = metadata
        .packages
        .iter()
        .map(String::as_str)
        .zip(metadata.store_paths.iter().map(String::as_str))
        .collect();
    have.sort_unstable();
    Ok(want != have)
}

/// Creates a new generation from the given package states and atomically activates it.
///
/// This is the install/remove/upgrade path: `state/packages/` is already the authoritative
/// source the generation was built from, so activation only flips the symlink — no snapshot
/// restore is needed (or wanted; it would pointlessly rewrite every state file).
pub fn rebuild_and_activate(states: &[PackageState]) -> Result<u64> {
    let id = create_generation(states)?;
    switch_symlink(id)?;
    note(&format!(
        "generation {} is now current",
        strong(&id.to_string())
    ));
    Ok(id)
}

/// Creates a new generation directory from the given package states and returns its ID.
///
/// The generation is built by hard-linking profile-relevant files (`bin/`, `share/man/`, etc.)
/// from each package's store path into the generation directory.
pub fn create_generation(states: &[PackageState]) -> Result<u64> {
    fs::create_dir_all(profiles_dir()?)?;

    let next_id = next_generation_id()?;
    let gen_dir = generation_dir(next_id)?;
    if gen_dir.exists() {
        fs::remove_dir_all(&gen_dir)?;
    }
    fs::create_dir_all(&gen_dir)?;

    status(&format!("building generation {next_id}"));

    // A generation describes the *environment*: the linked set — requested/held packages
    // and their runtime closure. Store-only packages (cached build deps, residue from a
    // failed install) are cache, not environment: they never reach `bin/` or `share/`,
    // never contest a bin name, and deliberately do not appear in `store_paths` or the
    // state snapshot — so `grm clean` can reclaim their dirs once their state is swept,
    // instead of every retained generation pinning the cache forever. Activation preserves
    // live cache records untouched (see `restore_state_snapshot`).
    let linked_names = crate::install::linked_set(states);
    let linked: Vec<&PackageState> = states
        .iter()
        .filter(|state| linked_names.contains(&state.name))
        .collect();

    // Resolve contested bin names across the whole linked set before linking anything, so
    // the outcome does not depend on package iteration order.
    let skip_bins = contested_bin_skips(&linked)?;
    let no_skips = BTreeSet::new();

    for state in &linked {
        let store_path = PathBuf::from(&state.store_path);
        if !store_path.exists() {
            warn(&format!(
                "store path {} does not exist, skipping",
                store_path.display()
            ));
            continue;
        }
        let skips = skip_bins.get(state.name.as_str()).unwrap_or(&no_skips);
        link_package_into_generation(state, &gen_dir, skips)?;
    }

    let generation = Generation {
        id: next_id,
        created: unix_now(),
        packages: linked.iter().map(|s| s.name.clone()).collect(),
        store_paths: linked.iter().map(|s| s.store_path.clone()).collect(),
    };
    write_generation_metadata(&gen_dir, &generation)?;
    // The linked state snapshot is what makes activation *semantic*: rollback/switch restore
    // `state/packages/` from it, so the rolled-back-to generation really is the system state.
    let linked_states: Vec<PackageState> = linked.iter().map(|s| (*s).clone()).collect();
    write_state_snapshot(&gen_dir, &linked_states)?;

    let mut registry = read_registry().unwrap_or_default();
    registry.push(generation);
    if let Err(e) = write_registry(&registry) {
        warn(&format!("could not write generations registry: {e}"));
    }

    success(&format!("created generation {next_id}"));
    Ok(next_id)
}

/// Semantically activates a generation: restores `state/packages/` and the lockfile from the
/// generation's state snapshot, then atomically flips the `current` symlink. After this, the
/// activated generation *is* the system state — queries report its packages and the next
/// mutating command builds on it instead of silently resurrecting the abandoned set.
///
/// The state restore lands before the symlink flip: the flip is the user-visible commit
/// point, and a crash between the two leaves state describing the target generation — which
/// the next mutating command or `grm rollback <id>` converges, and `grm doctor` flags.
///
/// Returns `true` when the profile actually switched, `false` when `id` was already active
/// (the repair path), so callers can word their result line accordingly.
pub fn activate_generation(id: u64) -> Result<bool> {
    let gen_dir = generation_dir(id)?;
    if !gen_dir.exists() {
        bail!("generation {id} does not exist");
    }
    let already_active = current_generation_id()? == Some(id);
    if !already_active {
        note(&format!(
            "switching profile to generation {}…",
            strong(&id.to_string())
        ));
    }
    if !restore_state_snapshot(&gen_dir)? {
        warn(&format!(
            "generation {id} has no state snapshot (created by an older grimoire); \
             switching the profile view only"
        ));
    }
    if already_active {
        // Re-activating the current generation is the repair path for an interrupted
        // activation: the state restore above converges state with the symlink.
        note(&format!(
            "generation {} is already active",
            strong(&id.to_string())
        ));
        return Ok(false);
    }
    switch_symlink(id)?;
    Ok(true)
}

/// Atomically repoints `profiles/current` at the given generation. The low-level half of
/// activation: callers are responsible for state/  staying in sync (see
/// [`activate_generation`] and [`rebuild_and_activate`]).
fn switch_symlink(id: u64) -> Result<()> {
    let gen_dir = generation_dir(id)?;
    let current = current_profile_link()?;
    let parent = current
        .parent()
        .context("current profile link should have a parent")?;
    fs::create_dir_all(parent)?;

    let tmp = parent.join(format!(".current-{id}"));
    std::os::unix::fs::symlink(&gen_dir, &tmp)
        .with_context(|| format!("stage current symlink -> {}", gen_dir.display()))?;
    fs::rename(&tmp, &current).with_context(|| format!("activate generation {id}"))?;
    Ok(())
}

/// Rolls back to the previous generation (the newest generation older than the current one).
/// Returns the ID of the generation rolled back to.
/// `rollback --dry-run`: names the generation activation would switch to and diffs its
/// snapshot against current state, without touching the profile.
pub fn dry_run_activation(generation: Option<u64>) -> Result<()> {
    let target = match generation {
        Some(id) => id,
        None => {
            let current =
                current_generation_id()?.context("no active generation to roll back from")?;
            let mut generations = list_generations()?;
            generations.sort_by_key(|b| std::cmp::Reverse(b.id));
            generations
                .into_iter()
                .find(|g| g.id < current)
                .context("no previous generation to roll back to")?
                .id
        }
    };
    let gen_dir = generation_dir(target)?;
    if !gen_dir.exists() {
        bail!("generation {target} does not exist");
    }
    if current_generation_id()? == Some(target) {
        println!("generation {target} is already active; activation would only converge state");
        return Ok(());
    }
    println!("would switch to generation {target}:");
    let Some(snapshot) = read_state_snapshot(&gen_dir)? else {
        println!("  (no state snapshot — profile view would switch, state would be untouched)");
        return Ok(());
    };
    let current: std::collections::BTreeMap<String, String> = crate::install::installed_states()?
        .into_iter()
        .map(|s| (s.name, s.version))
        .collect();
    let target_set: std::collections::BTreeMap<String, String> =
        snapshot.into_iter().map(|s| (s.name, s.version)).collect();
    for (name, version) in &target_set {
        match current.get(name) {
            None => println!("  + {name} {version}"),
            Some(now) if now != version => println!("  ~ {name} {now} → {version}"),
            _ => {}
        }
    }
    for name in current.keys() {
        if !target_set.contains_key(name) {
            println!("  - {name}");
        }
    }
    Ok(())
}

pub fn rollback() -> Result<u64> {
    let current = current_generation_id()?.context("no active generation to roll back from")?;
    let mut generations = list_generations()?;
    generations.sort_by_key(|b| std::cmp::Reverse(b.id));

    let previous = generations
        .into_iter()
        .find(|g| g.id < current)
        .context("no previous generation to roll back to")?;

    activate_generation(previous.id)?;
    Ok(previous.id)
}
