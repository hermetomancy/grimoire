use anyhow::{Context, Result};

use crate::{install, lock, paths, tome};

/// Reports Grimoire's environment and validates local state. Health results (counts, the
/// environment summary) go to stdout; per-item problems go to stderr (AGENTS.md §7). A clean
/// run reports zero problems; problems are diagnostics, not a hard error.
pub fn doctor() -> Result<()> {
    let root = paths::install_root().context("resolve install root")?;
    println!("grimoire: {}", env!("CARGO_PKG_VERSION"));
    println!("target: {}", paths::target_triple());
    println!("install root: {}", root.display());

    let mut problems = 0_usize;
    problems += check_tomes()?;
    problems += check_packages()?;
    check_lock(&mut problems)?;

    if problems == 0 {
        println!("health: ok");
    } else {
        println!("health: {problems} problem(s) found");
    }
    Ok(())
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
                "grimoire: tome `{}` is not synced (run `grimoire tome update {}`)",
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

    let root = paths::install_root()?;
    let bin_dir = root.join("bin");
    let mut problems = 0;

    for state in &states {
        let package_dir = root.join("packages").join(&state.name).join(&state.version);
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
            let shim = shim_path(&bin_dir, bin);
            if !shim.exists() {
                problems += 1;
                eprintln!(
                    "grimoire: package `{}` is missing its `{}` shim ({})",
                    state.name,
                    bin,
                    shim.display()
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

#[cfg(unix)]
fn shim_path(bin_dir: &std::path::Path, name: &str) -> std::path::PathBuf {
    bin_dir.join(name)
}

#[cfg(windows)]
fn shim_path(bin_dir: &std::path::Path, name: &str) -> std::path::PathBuf {
    bin_dir.join(format!("{name}.cmd"))
}
