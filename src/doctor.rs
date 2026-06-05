//! The `doctor` command: a read-only health check of the local Grimoire environment.
//!
//! It validates tome caches, installed-package state (package directories and profile links), and
//! lockfile presence, reporting counts to stdout and per-item problems to stderr.

use anyhow::{Context, Result};

use crate::{install, lock, paths, profile, tome, toolchain};

const CORE_USERLAND_TOOLS: &[&str] = &[
    "bash",
    "make",
    "coreutils",
    "sed",
    "grep",
    "gawk",
    "diffutils",
];

/// Reports Grimoire's environment and validates local state. Health results (counts, the
/// environment summary) go to stdout; per-item problems go to stderr (AGENTS.md §7). A clean
/// run reports zero problems; problems are diagnostics, not a hard error.
pub fn doctor() -> Result<()> {
    let root = paths::install_root().context("resolve install root")?;
    println!("grimoire: {}", env!("CARGO_PKG_VERSION"));
    println!("target: {}", paths::target_triple());
    println!("install root: {}", root.display());

    let mut problems = 0_usize;
    if let Some(msg) = paths::fixed_store_setup_instructions() {
        problems += 1;
        eprintln!("grimoire: {msg}");
    }
    problems += check_tomes()?;
    problems += check_packages()?;
    problems += check_source_build_readiness()?;
    check_lock(&mut problems)?;

    if problems == 0 {
        println!("health: ok");
    } else {
        println!("health: {problems} problem(s) found");
    }
    Ok(())
}

fn check_source_build_readiness() -> Result<usize> {
    let readiness = toolchain::source_build_readiness()?;
    println!(
        "source builds: host compiler boundary {}",
        if readiness.is_ready() {
            "ok"
        } else {
            "missing"
        }
    );
    report_managed_core_readiness()?;

    if readiness.is_ready() {
        return Ok(0);
    }

    eprintln!(
        "grimoire: source builds need a host compiler boundary for now; missing {}",
        readiness.missing_required.join(", ")
    );
    Ok(1)
}

fn report_managed_core_readiness() -> Result<()> {
    let missing = missing_core_tools()?;
    let installed = CORE_USERLAND_TOOLS.len() - missing.len();
    if missing.is_empty() {
        println!(
            "managed core userland: ready ({installed}/{})",
            CORE_USERLAND_TOOLS.len()
        );
    } else {
        println!(
            "managed core userland: incomplete ({installed}/{}, missing {})",
            CORE_USERLAND_TOOLS.len(),
            missing.join(", ")
        );
    }
    Ok(())
}

fn missing_core_tools() -> Result<Vec<String>> {
    let states = install::installed_states()?;
    Ok(CORE_USERLAND_TOOLS
        .iter()
        .filter(|name| !states.iter().any(|state| state.name == **name))
        .map(|name| (*name).to_owned())
        .collect())
}

fn check_tomes() -> Result<usize> {
    let tomes = tome::load_tomes()?;
    println!("tomes: {}", tomes.len());

    let mut problems = 0;
    for state in &tomes {
        let cache = tome::tome_cache_path(&state.name)?;
        if !cache.exists() {
            problems += 1;
            eprintln!(
                "grimoire: tome `{}` is not synced (run `grm tome update {}`)",
                state.name, state.name
            );
        } else if !cache.join("runes").exists() {
            problems += 1;
            eprintln!(
                "grimoire: tome `{}` cache is missing its runes directory",
                state.name
            );
        }
    }
    Ok(problems)
}

fn check_packages() -> Result<usize> {
    let states = install::installed_states()?;
    println!("installed packages: {}", states.len());

    let bin_dir = profile::current_profile_link()?.join("bin");
    let mut problems = 0;

    for state in &states {
        let package_dir = std::path::PathBuf::from(&state.store_path);
        if !package_dir.exists() {
            problems += 1;
            eprintln!(
                "grimoire: package `{}` {} is recorded but its files are missing ({})",
                state.name,
                state.version,
                package_dir.display()
            );
        }

        for bin in state.bins.keys() {
            let link = bin_dir.join(bin);
            if !link.exists() {
                problems += 1;
                eprintln!(
                    "grimoire: package `{}` is missing its `{}` profile link ({})",
                    state.name,
                    bin,
                    link.display()
                );
            }
        }
    }
    Ok(problems)
}

fn check_lock(problems: &mut usize) -> Result<()> {
    let path = lock::lock_path()?;
    if path.exists() {
        println!("lockfile: {}", path.display());
    } else if install::installed_states()?.is_empty() {
        println!("lockfile: none (no packages installed)");
    } else {
        *problems += 1;
        eprintln!("grimoire: lockfile is missing despite installed packages");
    }
    Ok(())
}
