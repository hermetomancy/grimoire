//! Grimoire (`grm`): a git-native, cross-platform package manager with reproducible installs.
//!
//! A single self-contained binary that installs software from *tomes* — git repositories of
//! Nushell `.rn` package definitions. It installs a verified prebuilt archive when one matches
//! the target and builds from source otherwise, into a user-local root with no privilege
//! escalation. This crate is the binary; `main` parses the CLI and dispatches to each module's
//! command entry point (`install`, `build`, `tome`, `doctor`, `query`, …).

mod archive;
mod build;
mod catalog;
mod cli;
mod cmd;
mod fetch;
mod install;
mod model;
mod nu;
mod profile;
mod solve;
mod store;
mod tome;
mod util;

use anyhow::Result;
use clap::Parser;
use cli::{Cli, Command, TomeCommand};
use util::{process_lock, progress, progress::Verbosity};

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
    // Tear the live spinner down before a panic message prints: the spinner thread redraws
    // stderr on a timer, and a panic mid-redraw would interleave escape sequences with the
    // panic report. The default hook still runs afterwards.
    let default_panic = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        progress::finish();
        default_panic(info);
    }));
    // Tear down any live spinner before returning so it never lingers in front of an error report
    // (anyhow prints to stderr) or the shell prompt.
    let result = run(cli);
    progress::finish();
    result
}

fn run(cli: Cli) -> Result<()> {
    // Commands that mutate shared install-root state are serialised by an OS-level advisory
    // lock; read-only commands and authoring commands that only touch a user-chosen path
    // (`tome init`, `tome rune`) skip it. `tome build` takes it — it writes the store and state.
    // See `process_lock` for the lifetime and crash semantics.
    let _lock = if mutates_install_root(&cli.command) {
        Some(process_lock::acquire()?)
    } else {
        None
    };

    match cli.command {
        Command::Build(args) => build::build(args),
        Command::Install(args) => install::install(args),
        Command::Remove(args) => install::remove(args),
        Command::Clean(args) => cmd::clean::clean(args),
        Command::Setup(args) => cmd::setup::setup(args),
        Command::List(args) => install::list(args),
        Command::Doctor => cmd::doctor::doctor(),
        Command::Search(args) => cmd::query::search(args),
        Command::Info(args) => cmd::query::info(args),
        Command::Upgrade(args) => cmd::query::upgrade(args),
        Command::Hold(args) => install::hold(args),
        Command::Unhold(args) => install::unhold(args),
        Command::Restore(args) => install::restore(args),
        Command::Files(args) => cmd::files::files(args),
        Command::Owns(args) => cmd::files::owns(args),
        Command::Provides(args) => cmd::files::provides(args),
        Command::Prefer(args) => cmd::prefer::prefer(args),
        Command::Switch(args) => {
            if args.dry_run {
                return profile::dry_run_activation(args.generation);
            }
            let started = std::time::Instant::now();
            match args.generation {
                Some(id) => {
                    if profile::activate_generation(id)? {
                        progress::report(&format!(
                            "{} {}",
                            progress::accent(&format!(
                                "switched to generation {id} in {:.2}s",
                                started.elapsed().as_secs_f64(),
                            )),
                            progress::faint("— nothing was rebuilt, nothing was lost"),
                        ));
                    }
                }
                None => {
                    let id = profile::switch_to_previous()?;
                    progress::report(&format!(
                        "{} {}",
                        progress::accent(&format!(
                            "switched to generation {id} in {:.2}s",
                            started.elapsed().as_secs_f64(),
                        )),
                        progress::faint("— nothing was rebuilt, nothing was lost"),
                    ));
                }
            }
            Ok(())
        }
        Command::Generations => cmd::generations::generations(),
        Command::Tome { command } => match command {
            TomeCommand::Init(args) => tome::init(args),
            TomeCommand::Rune(args) => tome::rune(args),
            TomeCommand::Build(args) => tome::build(args),
            TomeCommand::Add(args) => tome::add(args),
            TomeCommand::Update(args) => tome::update(args),
            TomeCommand::Remove(args) => tome::remove(args),
            TomeCommand::List => tome::list(),
            TomeCommand::News(args) => tome::news::news_command(args.name, args.all),
        },
        Command::Addendum { command } => match command {
            cli::AddendumCommand::Add(args) => catalog::addendum::add(args),
            cli::AddendumCommand::Remove(args) => catalog::addendum::remove(args),
            cli::AddendumCommand::List => catalog::addendum::list(),
            cli::AddendumCommand::Update(args) => catalog::addendum::update(args),
        },
        Command::StoreHash(args) => {
            for package in &args.packages {
                println!("{}", store::closure::store_hash(package)?);
            }
            Ok(())
        }
        Command::Completions(args) => cmd::man::completions(args),
        Command::Man(args) => cmd::man::man(args),
    }
}

fn mutates_install_root(command: &Command) -> bool {
    match command {
        // `--dry-run` resolves and prints a plan without writing anything; it can run while
        // another `grm` holds the lock.
        Command::Install(args) => !args.dry_run,
        Command::Upgrade(args) => !args.dry_run,
        // Bare `grm prefer` only lists; setting or unsetting mutates state and may relink.
        Command::Prefer(args) => args.capability.is_some() && !args.dry_run,
        Command::Remove(args) | Command::Hold(args) | Command::Unhold(args) => !args.dry_run,
        Command::Clean(args) => !args.dry_run,
        Command::Restore(args) => !args.dry_run,
        Command::Switch(args) => !args.dry_run,
        Command::Tome { command } => match command {
            TomeCommand::Add(args) => !args.dry_run,
            TomeCommand::Update(args) => !args.dry_run,
            TomeCommand::Remove(args) => !args.dry_run,
            // Every build writes the shared store and state/packages: a single-package build
            // installs its build deps store-only (§8), and `--all` installs the built packages.
            // Both must hold the lock so a concurrent `grm clean` cannot reap store paths mid-build.
            TomeCommand::Build(_) => true,
            // Default `tome news` advances the seen marker; `--all` is a pure read.
            TomeCommand::News(args) => !args.all,
            _ => false,
        },
        Command::Addendum { command } => match command {
            cli::AddendumCommand::Add(args) => !args.dry_run,
            cli::AddendumCommand::Update(args) => !args.dry_run,
            cli::AddendumCommand::Remove(args) => !args.dry_run,
            cli::AddendumCommand::List => false,
        },
        Command::Build(_) => true,
        Command::List(_)
        | Command::Files(_)
        | Command::Owns(_)
        | Command::Provides(_)
        | Command::Doctor
        | Command::Search(_)
        | Command::Info(_)
        | Command::Generations
        | Command::StoreHash(_)
        | Command::Completions(_)
        | Command::Man(_)
        | Command::Setup(_) => false,
    }
}
