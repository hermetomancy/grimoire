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
mod doctor;
mod fetch;
mod index;
mod install;
mod lock;
mod man;
mod model;
mod nu;
mod paths;
mod process_lock;
mod progress;
mod query;
mod solve;
mod tome;

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
        Command::List => install::list(),
        Command::Doctor => doctor::doctor(),
        Command::Search(args) => query::search(args),
        Command::Info(args) => query::info(args),
        Command::Upgrade(args) => query::upgrade(args),
        Command::Hold(args) => install::hold(args),
        Command::Unhold(args) => install::unhold(args),
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
        },
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
        Command::Remove(_) | Command::Clean | Command::Hold(_) | Command::Unhold(_) => true,
        Command::Tome { command } => matches!(
            command,
            TomeCommand::Add(_) | TomeCommand::Update(_) | TomeCommand::Remove(_)
        ),
        Command::Addendum { command } => matches!(
            command,
            cli::AddendumCommand::Add(_) | cli::AddendumCommand::Remove(_)
        ),
        Command::Build(_)
        | Command::List
        | Command::Doctor
        | Command::Search(_)
        | Command::Info(_)
        | Command::Completions(_)
        | Command::Man(_) => false,
    }
}
