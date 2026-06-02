//! Grimoire (`grm`): a git-native, cross-platform package manager with reproducible installs.
//!
//! A single self-contained binary that installs software from *tomes* — git repositories of
//! Nushell `.rn` package definitions. It installs a verified prebuilt archive when one matches
//! the target and builds from source otherwise, into a user-local root with no privilege
//! escalation. This crate is the binary; `main` parses the CLI and dispatches to each module's
//! command entry point (`install`, `build`, `tome`, `doctor`, `query`, …).

mod archive;
mod build;
mod cli;
mod doctor;
mod fetch;
mod index;
mod install;
mod lock;
mod model;
mod nu;
mod paths;
mod progress;
mod query;
mod solve;
mod tome;

use anyhow::Result;
use clap::Parser;
use cli::{AddendumCommand, Cli, Command, TomeCommand};
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
    match cli.command {
        Command::Build(args) => build::build(args),
        Command::Install(args) => install::install(args),
        Command::Remove(args) => install::remove(args),
        Command::List => install::list(),
        Command::Doctor => doctor::doctor(),
        Command::Search(args) => query::search(args),
        Command::Info(args) => query::info(args),
        Command::Upgrade(args) => query::upgrade(args),
        Command::Tome { command } => match command {
            TomeCommand::Init(args) => tome::init(args),
            TomeCommand::Rune(args) => tome::rune(args),
            TomeCommand::Build(args) => tome::build(args),
            TomeCommand::Add(args) => tome::add(args),
            TomeCommand::Update(args) => tome::update(args),
            TomeCommand::Remove(args) => tome::remove(args),
            TomeCommand::List => tome::list(),
        },
        Command::Addendum { command } => addendum(command),
    }
}

fn addendum(command: AddendumCommand) -> Result<()> {
    match command {
        AddendumCommand::Add(args) => {
            println!(
                "would add addendum from {} at {}",
                args.git_url, args.ref_name
            );
        }
        AddendumCommand::Remove(args) => {
            println!("would remove addendum {}", args.name);
        }
        AddendumCommand::List => {
            println!("addendum state is not wired yet");
        }
    }
    Ok(())
}
