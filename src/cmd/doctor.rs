//! The `doctor` command: a read-only health check of the local Grimoire environment.
//!
//! It validates tome caches, installed-package state (package directories and profile links), and
//! lockfile presence, reporting counts to stdout and per-item problems to stderr.

use anyhow::{Context, Result};
use std::{collections::BTreeMap, fs, path::Path};

use crate::{
    build::{self, toolchain},
    catalog::sync_common,
    install,
    install::lock,
    model::{PackageState, preferences::Preferences},
    nu::nuon_io,
    profile, tome,
    util::output::{self, field, problem},
    util::paths,
};

/// Reports Grimoire's environment and validates local state. Health results (counts, the
/// environment summary) go to stdout; per-item problems go to stderr (AGENTS.md §12.1). A clean
/// run reports zero problems; problems are diagnostics, not a hard error.
pub fn doctor() -> Result<()> {
    let root = paths::install_root().context("resolve install root")?;
    field("grimoire", env!("CARGO_PKG_VERSION"));
    field("target", &paths::target_triple());
    field("install root", &root.display().to_string());

    let mut problems = 0_usize;
    if let Some(msg) = paths::fixed_store_setup_instructions() {
        problems += 1;
        problem(&msg);
    }
    problems += check_install_root(&root);
    problems += check_current_symlink()?;
    problems += check_state_files()?;
    problems += check_state_generation_divergence()?;
    problems += check_tomes()?;
    problems += check_packages()?;
    check_address_drift()?;
    problems += check_duplicate_bins()?;
    problems += check_generation_snapshots()?;
    problems += check_stale_backups()?;
    problems += check_source_build_readiness()?;
    check_lock(&mut problems)?;

    if problems == 0 {
        field("health", &output::strong("ok"));
    } else {
        field("health", &format!("{problems} problem(s) found"));
    }
    Ok(())
}

/// Whitespace in the install root breaks source builds: autotools bakes unquoted absolute
/// tool paths into Makefiles, which split at the space.
fn check_install_root(root: &Path) -> usize {
    if root.to_string_lossy().chars().any(char::is_whitespace) {
        problem(&format!(
            "install root `{}` contains whitespace, which breaks source builds; \
             set GRIMOIRE_ROOT to a space-free path",
            root.display()
        ));
        return 1;
    }
    0
}

/// A `profiles/current` symlink pointing at a deleted generation leaves a dead PATH entry.
fn check_current_symlink() -> Result<usize> {
    let link = profile::current_profile_link()?;
    if fs::symlink_metadata(&link).is_ok() && fs::metadata(&link).is_err() {
        problem(&format!(
            "`{}` points at a generation that no longer exists; run `grm switch <id>`",
            link.display()
        ));
        return Ok(1);
    }
    Ok(0)
}

/// Reports state files that no longer parse, per file, instead of failing the whole check
/// the way `installed_states()` would.
fn check_state_files() -> Result<usize> {
    let state_dir = paths::install_root()?.join("state").join("packages");
    if !state_dir.exists() {
        return Ok(0);
    }
    let mut problems = 0;
    for entry in fs::read_dir(&state_dir)? {
        let path = entry?.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("nuon") {
            continue;
        }
        let parsed = nuon_io::read_nuon(&path).and_then(PackageState::from_value);
        if let Err(e) = parsed {
            problems += 1;
            problem(&format!(
                "package state file `{}` is corrupt: {e:#}",
                path.display()
            ));
        }
    }
    Ok(problems)
}

/// Activation restores state from the generation snapshot before flipping the symlink, so
/// state and the active generation should always agree; divergence means an interrupted
/// activation (crash between state restore and symlink flip) or a hand-edited state dir.
fn check_state_generation_divergence() -> Result<usize> {
    let Some(id) = profile::current_generation_id()? else {
        return Ok(0);
    };
    let gen_dir = profile::generation_dir(id)?;
    let Some(snapshot) = profile::read_state_snapshot(&gen_dir)? else {
        return Ok(0); // pre-snapshot generation; nothing to compare against
    };
    let installed: BTreeMap<String, String> = install::InstalledWorld::load_default()?
        .iter()
        .map(|s| (s.name.clone(), s.version.clone()))
        .collect();
    // Subset check, not equality: store-only installs (`grm tome build`) legitimately add
    // state entries that are not linked into the active generation. What must never happen
    // is the active generation describing packages that state lost or re-versioned.
    let diverged: Vec<String> = snapshot
        .into_iter()
        .filter(|s| installed.get(&s.name) != Some(&s.version))
        .map(|s| format!("{} {}", s.name, s.version))
        .collect();
    if !diverged.is_empty() {
        problem(&format!(
            "state/packages diverges from active generation {id} ({}); interrupted \
             activation? run `grm switch <id>` to converge",
            diverged.join(", ")
        ));
        return Ok(1);
    }
    Ok(0)
}

/// Two *linked* packages claiming the same bin name without a `grm prefer` choice will fail
/// the next generation rebuild; surface it before that happens. Store-only packages (cached
/// build deps) never link, so their bins cannot contest anything — rust-stage0 shipping
/// `rustc` beside linked rust is by design, not a problem.
fn check_duplicate_bins() -> Result<usize> {
    let world = install::InstalledWorld::load_default()?;
    let linked = world.linked_immut();
    let preferences = Preferences::load().unwrap_or_default();
    let mut owners: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for state in world.iter().filter(|state| linked.contains(&state.name)) {
        for bin in state.bins.keys() {
            owners.entry(bin).or_default().push(&state.name);
        }
    }
    let mut problems = 0;
    for (bin, claimants) in owners {
        if claimants.len() < 2 {
            continue;
        }
        let resolved = preferences
            .providers
            .get(bin)
            .is_some_and(|p| claimants.iter().any(|c| c == p));
        if !resolved {
            problems += 1;
            problem(&format!(
                "bin `{bin}` is provided by multiple packages ({}); run \
                 `grm prefer {bin} <package>` before the next install",
                claimants.join(", ")
            ));
        }
    }
    Ok(problems)
}

/// `.grimoire-old` directories are move-aside backups dropped on commit; one left behind
/// means an interrupted transaction, and it silently holds disk space.
/// A retained generation whose snapshot references missing store paths is a switch trap:
/// activation would restore state records for packages whose bits are gone. `grm clean`
/// always protects retained generations' store paths, so this only happens after manual
/// store surgery or an interrupted clean — flag it *before* a switch discovers it.
fn check_generation_snapshots() -> Result<usize> {
    let mut problems = 0;
    for generation in profile::list_generations()? {
        let dir = profile::generation_dir(generation.id)?;
        let Ok(Some(snapshot)) = profile::read_state_snapshot(&dir) else {
            continue; // pre-snapshot generation or unreadable; other checks cover corruption
        };
        let missing: Vec<&str> = snapshot
            .iter()
            .filter(|state| !Path::new(&state.store_path).exists())
            .map(|state| state.name.as_str())
            .collect();
        if !missing.is_empty() {
            problems += 1;
            problem(&format!(
                "generation {} snapshot references missing store path(s) for {}; \
                 rolling back to it would restore state for packages whose bits are gone",
                generation.id,
                missing.join(", ")
            ));
        }
    }
    Ok(problems)
}

fn check_stale_backups() -> Result<usize> {
    let mut dirs = vec![paths::store_root()?];
    let cache = paths::install_root()?.join("cache");
    for sub in ["tomes", "addenda"] {
        dirs.push(cache.join(sub));
    }
    let mut problems = 0;
    for dir in dirs {
        if !dir.exists() {
            continue;
        }
        for entry in fs::read_dir(&dir)? {
            let path = entry?.path();
            if path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.ends_with(".grimoire-old"))
            {
                problems += 1;
                problem(&format!(
                    "stale transaction backup `{}` (safe to delete)",
                    path.display()
                ));
            }
        }
    }
    let state_root = paths::install_root()?.join("state");
    for name in [".packages-old", ".packages-staging"] {
        let path = state_root.join(name);
        if path.exists() {
            problems += 1;
            problem(&format!(
                "stale state transaction directory `{}` (run `grm generation switch <id>` to repair or delete it if no switch is in progress)",
                path.display()
            ));
        }
    }
    Ok(problems)
}

fn check_source_build_readiness() -> Result<usize> {
    let readiness = toolchain::source_build_readiness()?;
    let mut problems = 0;
    field(
        "source builds",
        &format!(
            "host compiler boundary {}",
            if readiness.is_ready() {
                "ok"
            } else {
                "missing"
            }
        ),
    );
    report_managed_core_readiness()?;
    if std::env::consts::OS == "macos" {
        match toolchain::macos_sdk_path() {
            Some(path) => field("macOS SDK", &path),
            None => {
                problems += 1;
                problem("macOS SDK not found (`xcrun --show-sdk-path` failed)");
            }
        }
    }

    if readiness.is_ready() {
        return Ok(problems);
    }

    problem(&format!(
        "source builds need a host compiler boundary for now; missing {}",
        readiness.missing_required.join(", ")
    ));
    Ok(problems + 1)
}

/// build-env's dependency closure *is* the managed build requirement (the compiler toolchain plus
/// the userland floor), so doctor reports the same target-scoped contract the build code uses.
fn report_managed_core_readiness() -> Result<()> {
    let readiness = build::managed_floor_readiness(&paths::target_triple())?;
    field("managed core userland", &readiness.message());
    Ok(())
}

fn check_tomes() -> Result<usize> {
    let tomes = tome::load_tomes()?;
    field("tomes", &tomes.len().to_string());

    let mut problems = 0;
    for state in &tomes {
        let cache = sync_common::cache_path("tomes", &state.name)?;
        if !cache.exists() {
            problems += 1;
            problem(&format!(
                "tome `{}` is not synced (run `grm tome update {}`)",
                state.name, state.name
            ));
        } else if !cache.join("runes").exists() {
            problems += 1;
            problem(&format!(
                "tome `{}` cache is missing its runes directory",
                state.name
            ));
        }
    }
    Ok(problems)
}

fn check_packages() -> Result<usize> {
    let world = install::InstalledWorld::load_default()?;
    let states = world.to_states();
    // Only PATH-linked packages have profile bin links to verify; store-only cache and build-only
    // toolchain packages (pinned but never linked) do not.
    let bin_linked = world.bin_linked_immut();
    field("installed packages", &states.len().to_string());

    let bin_dir = profile::current_profile_link()?.join("bin");
    let mut problems = 0;

    for state in &states {
        let package_dir = std::path::PathBuf::from(&state.store_path);
        if !package_dir.exists() {
            problems += 1;
            problem(&format!(
                "package `{}` {} is recorded but its files are missing ({})",
                state.name,
                state.version,
                package_dir.display()
            ));
        }

        // Only expect profile links for PATH-linked packages; store-only installs (e.g. from
        // `tome build --all`) and build-only toolchain packages are not linked.
        if !bin_linked.contains(&state.name) {
            continue;
        }

        for bin in state.bins.keys() {
            let link = bin_dir.join(bin);
            if !link.exists() {
                problems += 1;
                problem(&format!(
                    "package `{}` is missing its `{}` profile link ({})",
                    state.name,
                    bin,
                    link.display()
                ));
            }
        }
    }
    Ok(problems)
}

/// Informational, not a problem: installed from-source packages whose expected content
/// address no longer matches what their rune would produce today — the rune was edited, a
/// runtime dependency re-addressed, or the build environment identity changed. The next
/// install or upgrade that needs them rebuilds at the new address; surfacing the pending
/// drift here makes that rebuild predictable instead of surprising ("X is up to date"
/// followed by a 40-minute source build).
fn check_address_drift() -> Result<()> {
    let world = install::InstalledWorld::load_default()?;
    let drifted = crate::store::closure::stale_installed(&world);
    if drifted.is_empty() {
        return Ok(());
    }
    let states = world.to_states();
    let by_name: std::collections::BTreeMap<&str, &crate::model::PackageState> =
        states.iter().map(|s| (s.name.as_str(), s)).collect();
    for stale in &drifted {
        let installed = by_name
            .get(stale.name.as_str())
            .map(|s| s.store_hash.as_str())
            .unwrap_or("?");
        let cause = match &stale.env_change {
            Some(diff) => format!(" (build environment changed: {diff})"),
            None => String::new(),
        };
        output::note(&format!(
            "{} drifted: installed {installed}, expected {}{cause}",
            output::strong(&stale.name),
            stale.expected
        ));
    }
    output::note(&format!(
        "{} package(s) will rebuild at their new address the next time an install or \
         upgrade needs them (rune, dependency, or build environment changed)",
        drifted.len()
    ));
    Ok(())
}

fn check_lock(problems: &mut usize) -> Result<()> {
    let path = lock::lock_path()?;
    if path.exists() {
        field("lockfile", &path.display().to_string());
    } else if install::InstalledWorld::load_default()?
        .iter()
        .next()
        .is_none()
    {
        field("lockfile", "none (no packages installed)");
    } else {
        *problems += 1;
        problem("lockfile is missing despite installed packages");
    }
    Ok(())
}
