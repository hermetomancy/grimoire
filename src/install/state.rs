//! Reading and flagging installed-package state: listing, hold/unhold, requested marking.

use anyhow::{Result, bail};

use crate::util::output::{self, Cell, report};

use super::world::InstalledWorld;
use super::*;

pub fn list(args: crate::cli::ListArgs) -> Result<()> {
    let world = InstalledWorld::load_default()?;
    let states = world.to_states();
    if states.is_empty() {
        output::line("no packages installed");
        return Ok(());
    }
    let linked = world.linked_immut();
    let bin_linked = world.bin_linked_immut();
    // The environment is what `list` answers for: the PATH-linked set. Store-only cache and
    // build-only toolchain packages (pinned but never linked) only appear under `--all`.
    // `--explicit` narrows the other way — to just the requested roots, the answer to "what did I
    // actually ask for" (the set `grm install` rebuilds).
    let shown: Vec<_> = states
        .iter()
        .filter(|state| {
            if args.explicit {
                state.requested
            } else {
                args.all || bin_linked.contains(&state.name)
            }
        })
        .collect();
    if shown.is_empty() {
        output::line("no explicitly-installed packages");
        return Ok(());
    }
    let hidden = states.len() - shown.len();
    let (total, mut held, mut store_only, mut build_only) = (shown.len(), 0, 0, 0);
    let rows = shown
        .iter()
        .map(|state| {
            let flag = if state.held {
                held += 1;
                Cell::caution("held")
            } else if !linked.contains(&state.name) {
                // Present in the store for reuse (build dep, residue) but not the environment.
                store_only += 1;
                Cell::faint("store-only")
            } else if !bin_linked.contains(&state.name) {
                // A managed toolchain package: pinned in the store but kept off the PATH.
                build_only += 1;
                Cell::faint("build-only")
            } else {
                Cell::plain("")
            };
            vec![
                Cell::strong(&state.name),
                Cell::plain(&state.version),
                Cell::faint(state.target.as_deref().unwrap_or("")),
                flag,
            ]
        })
        .collect();
    output::print_rows(rows);

    // A terminal gets a closing summary; piped output stays rows-only for scripts.
    if std::io::IsTerminal::is_terminal(&std::io::stdout()) {
        let mut summary = format!("{total} package{}", if total == 1 { "" } else { "s" });
        if args.explicit {
            summary.push_str(" explicitly installed");
            if held > 0 {
                summary.push_str(&format!(" — {held} held"));
            }
        } else {
            let linked_count = total - held - store_only - build_only;
            let mut parts = vec![format!("{linked_count} linked")];
            if held > 0 {
                parts.push(format!("{held} held"));
            }
            if build_only > 0 {
                parts.push(format!("{build_only} build-only"));
            }
            if store_only > 0 {
                parts.push(format!("{store_only} store-only"));
            }
            summary.push_str(&format!(" — {}", parts.join(", ")));
            if !args.all && hidden > 0 {
                summary.push_str(&format!(", {hidden} hidden (--all)"));
            }
        }
        output::line(&output::faint(&summary));
    }
    Ok(())
}

/// Marks `name` as held so `grm upgrade` skips it. Idempotent: holding a held package is a
/// no-op that still reports success. Fails when the package is not installed.
pub fn hold(args: crate::cli::MutatePackagesArgs) -> Result<()> {
    if args.packages.is_empty() {
        bail!("specify at least one package to hold");
    }
    let mut world = InstalledWorld::load_default()?;
    let mut changed = false;
    for package in &args.packages {
        if args.dry_run {
            dry_run_hold(package, true)?;
        } else {
            changed |= set_hold(&mut world, package, true, true)?;
        }
    }
    if changed {
        let mut tx = Transaction::new();
        world.commit(&mut tx)?;
        finalize_state(&mut tx, &world)?;
        tx.commit();
    }
    Ok(())
}

pub fn unhold(args: crate::cli::MutatePackagesArgs) -> Result<()> {
    if args.packages.is_empty() {
        bail!("specify at least one package to unhold");
    }
    let mut world = InstalledWorld::load_default()?;
    let mut changed = false;
    for package in &args.packages {
        if args.dry_run {
            dry_run_hold(package, false)?;
        } else {
            changed |= set_hold(&mut world, package, false, true)?;
        }
    }
    if changed {
        let mut tx = Transaction::new();
        world.commit(&mut tx)?;
        finalize_state(&mut tx, &world)?;
        tx.commit();
    }
    Ok(())
}

/// Validates the target like a real hold/release would, then says what would change.
fn dry_run_hold(name: &str, held: bool) -> Result<()> {
    let world = InstalledWorld::load_default()?;
    let Some(state) = world.get(name) else {
        bail!("package `{name}` is not installed");
    };
    if state.held == held {
        output::note(&format!(
            "{name} is already {}; nothing to do",
            if held { "held" } else { "not held" }
        ));
    } else {
        output::note(&format!(
            "would {} {name} {}",
            if held { "hold" } else { "release" },
            state.version
        ));
    }
    Ok(())
}

pub(crate) fn set_hold(
    world: &mut InstalledWorld,
    name: &str,
    held: bool,
    announce: bool,
) -> Result<bool> {
    let Some(state) = world.get(name) else {
        bail!("package `{name}` is not installed");
    };
    if state.held == held {
        if announce {
            report(&format!(
                "{name} is already {}",
                if held { "held" } else { "not held" }
            ));
        }
        return Ok(false);
    }
    world.update(name, |state| state.held = held);
    if announce {
        report(&format!(
            "{} {}",
            output::accent(name),
            if held { "held" } else { "released" }
        ));
    }
    Ok(true)
}

/// Marks `name` as explicitly requested (or demotes it back to a dependency). `name` is
/// resolved like a dependency — an exact package name, a bin, or a provided capability — so
/// `grm install awk` marks whichever package actually satisfied `awk`. Returns whether the
/// flag actually changed: a promotion can pull a store-only package into the linked set, so
/// the caller may need to rebuild the generation even when nothing was installed.
pub(crate) fn set_requested(
    world: &mut InstalledWorld,
    name: &str,
    requested: bool,
    announce: bool,
) -> Result<bool> {
    let Some(state) = world.resolve_dep(name).cloned() else {
        bail!("package `{name}` is not installed");
    };
    // Promoting a store-only package pulls it (and its closure) into the linked set without
    // re-realizing it, so the linked-conflict gate must run here just like it does for a
    // fresh linked install. An already-linked package was checked when it landed.
    if requested && !state.requested && !world.linked_immut().contains(&state.name) {
        refuse_linked_conflicts(world, &state.name, &state.conflicts, &state.replaces)?;
    }
    if state.requested == requested {
        if announce {
            report(&format!(
                "{} is already {}",
                state.name,
                if requested {
                    "requested"
                } else {
                    "a dependency"
                }
            ));
        }
        return Ok(false);
    }
    world.update(&state.name, |s| s.requested = requested);
    if announce {
        report(&format!(
            "{} marked as {}",
            state.name,
            if requested {
                "requested"
            } else {
                "a dependency"
            }
        ));
    }
    Ok(true)
}
