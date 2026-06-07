//! Grimoire (`grm`): a git-native, cross-platform package manager with reproducible installs.
//!
//! A single self-contained binary that installs software from *tomes* — git repositories of
//! Nushell `.rn` package definitions. It installs a verified prebuilt archive when one matches
//! the target and builds from source otherwise, into a user-local root with no privilege
//! escalation. This crate is the binary; `main` parses the CLI and dispatches to each module's
//! command entry point (`install`, `build`, `tome`, `doctor`, `query`, …).

mod addendum;
mod archive;
mod build;
mod clean;
mod cli;
mod closure;
mod doctor;
mod fetch;
mod fs_util;
mod index;
mod install;
mod lock;
mod man;
mod model;
mod nu;
mod paths;
mod process_lock;
mod profile;
mod progress;
mod query;
mod setup;
mod signing;
mod solve;
mod store;
mod sync_common;
mod tome;
mod toolchain;

use anyhow::Result;
use clap::Parser;
use cli::{Cli, Command, TomeCommand};
use progress::Verbosity;

fn main() -> Result<()> {
    let cli = Cli::parse();
    let verbosity = if cli.quiet {
        Verbosity::Quiet
    } else if cli.verbose {
        Verbosity::Verbose
    } else {
        Verbosity::Normal
    };
    progress::set_verbosity(verbosity);
    // Tear down any live spinner before returning so it never lingers in front of an error report
    // (anyhow prints to stderr) or the shell prompt.
    let result = run(cli);
    progress::finish();
    result
}

fn run(cli: Cli) -> Result<()> {
    // Commands that mutate shared install-root state are serialised by an OS-level advisory
    // lock; read-only commands and commands that operate on user-chosen paths (build, tome
    // init/rune/build) skip it. See `process_lock` for the lifetime and crash semantics.
    let _lock = if mutates_install_root(&cli.command) {
        Some(process_lock::acquire()?)
    } else {
        None
    };

    match cli.command {
        Command::Build(args) => build::build(args),
        Command::Install(args) => install::install(args),
        Command::Remove(args) => install::remove(args),
        Command::Clean => clean::clean(),
        Command::Setup => setup::setup(),
        Command::List => install::list(),
        Command::Doctor => doctor::doctor(),
        Command::Search(args) => query::search(args),
        Command::Info(args) => query::info(args),
        Command::Upgrade(args) => query::upgrade(args),
        Command::Hold(args) => install::hold(args),
        Command::Unhold(args) => install::unhold(args),
        Command::Rollback => {
            let id = profile::rollback()?;
            println!("rolled back to generation {id}");
            Ok(())
        }
        Command::Switch(args) => {
            profile::activate_generation(args.id)?;
            println!("switched to generation {}", args.id);
            Ok(())
        }
        Command::Generations => {
            let gens = profile::list_generations()?;
            let current = profile::current_generation_id()?;
            for g in gens {
                let marker = if current == Some(g.id) { "*" } else { " " };
                println!(
                    "{} gen-{:<4} {} ({} packages)",
                    marker,
                    g.id,
                    format_timestamp(g.created),
                    g.packages.len()
                );
            }
            Ok(())
        }
        Command::CollectGarbage(args) => profile::gc(args.keep),
        Command::DeleteGeneration(args) => {
            profile::delete_generation(args.id)?;
            println!("deleted generation {}", args.id);
            Ok(())
        }
        Command::Tome { command } => match command {
            TomeCommand::Init(args) => tome::init(args),
            TomeCommand::Rune(args) => tome::rune(args),
            TomeCommand::Build(args) => tome::build(args),
            TomeCommand::Add(args) => tome::add(args),
            TomeCommand::Update(args) => tome::update(args),
            TomeCommand::Remove(args) => tome::remove(args),
            TomeCommand::List => tome::list(),
        },
        Command::Addendum { command } => match command {
            cli::AddendumCommand::Add(args) => addendum::add(args),
            cli::AddendumCommand::Remove(args) => addendum::remove(args),
            cli::AddendumCommand::List => addendum::list(),
            cli::AddendumCommand::Update(args) => addendum::update(args),
        },
        Command::StoreHash(args) => {
            for package in &args.packages {
                println!("{}", closure::store_hash(package)?);
            }
            Ok(())
        }
        Command::Completions(args) => man::completions(args),
        Command::Man(args) => man::man(args),
    }
}

fn mutates_install_root(command: &Command) -> bool {
    match command {
        // `--dry-run` resolves and prints a plan without writing anything; it can run while
        // another `grm` holds the lock.
        Command::Install(args) => !args.dry_run,
        Command::Upgrade(args) => !args.dry_run,
        Command::Remove(_)
        | Command::Clean
        | Command::Hold(_)
        | Command::Unhold(_)
        | Command::Rollback
        | Command::Switch(_)
        | Command::CollectGarbage(_)
        | Command::DeleteGeneration(_) => true,
        Command::Tome { command } => match command {
            TomeCommand::Add(_) | TomeCommand::Update(_) | TomeCommand::Remove(_) => true,
            TomeCommand::Build(args) => args.all,
            _ => false,
        },
        Command::Addendum { command } => matches!(
            command,
            cli::AddendumCommand::Add(_)
                | cli::AddendumCommand::Update(_)
                | cli::AddendumCommand::Remove(_)
        ),
        Command::Build(_) => true,
        Command::List
        | Command::Doctor
        | Command::Search(_)
        | Command::Info(_)
        | Command::Generations
        | Command::StoreHash(_)
        | Command::Completions(_)
        | Command::Man(_)
        | Command::Setup => false,
    }
}

/// Formats a Unix timestamp as `YYYY-MM-DD HH:MM:SS UTC`.
fn format_timestamp(ts: u64) -> String {
    // Simple conversion from Unix seconds to calendar date. Not leap-second aware,
    // but accurate enough for human-readable generation listings.
    const DAYS_IN_MONTH: [u64; 12] = [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    let mut days = ts / 86400;
    let mut rem = ts % 86400;
    let hh = rem / 3600;
    rem %= 3600;
    let mm = rem / 60;
    let ss = rem % 60;

    let mut year = 1970u64;
    // A 400-year Gregorian cycle has exactly 146097 days. Process in large
    // chunks so that timestamps near u64::MAX do not loop billions of times.
    const DAYS_IN_400_YEARS: u64 = 146097;
    let cycles = days / DAYS_IN_400_YEARS;
    year += cycles * 400;
    days -= cycles * DAYS_IN_400_YEARS;
    loop {
        let is_leap = (year % 4 == 0 && year % 100 != 0) || (year % 400 == 0);
        let year_days = if is_leap { 366 } else { 365 };
        if days < year_days {
            break;
        }
        days -= year_days;
        year += 1;
    }

    let is_leap = (year % 4 == 0 && year % 100 != 0) || (year % 400 == 0);
    let mut month = 1u64;
    for (i, &dim) in DAYS_IN_MONTH.iter().enumerate() {
        let dim = if i == 1 && is_leap { 29 } else { dim };
        if days < dim {
            month = (i + 1) as u64;
            break;
        }
        days -= dim;
        month = (i + 2) as u64;
    }
    let day = days + 1;

    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02} UTC",
        year, month, day, hh, mm, ss
    )
}
