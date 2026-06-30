//! Grimoire (`grm`): a git-native, cross-platform package manager with reproducible installs.
//!
//! A single self-contained binary that installs software from *tomes* — git repositories of
//! Nushell `.rn` package definitions. It installs a verified prebuilt archive when one matches
//! the target and builds from source otherwise, into a user-local root with no privilege
//! escalation. This crate is the binary; `main` parses the CLI and dispatches to each module's
//! command entry point (`install`, `build`, `tome`, `doctor`, `query`, …).
//!
//! All user-facing output flows through [`util::output`]; bare `println!`/`eprintln!` are denied
//! crate-wide (the `util::output` submodules opt back in). See `clippy.toml`.
#![deny(clippy::disallowed_macros)]

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
use cli::{Cli, Command, GenerationCommand, PkgCommand, TomeCommand};
use util::{output, output::Verbosity, process_lock};

fn main() -> Result<()> {
    let cli = Cli::parse();
    let verbosity = if cli.quiet {
        Verbosity::Quiet
    } else if cli.verbose {
        Verbosity::Verbose
    } else {
        Verbosity::Normal
    };
    output::set_verbosity(verbosity);
    // Tear the live spinner down before a panic message prints: the spinner thread redraws
    // stderr on a timer, and a panic mid-redraw would interleave escape sequences with the
    // panic report. The default hook still runs afterwards.
    let default_panic = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        output::finish();
        default_panic(info);
    }));
    // Tear down any live spinner before returning so it never lingers in front of an error report
    // (anyhow prints to stderr) or the shell prompt.
    let result = run(cli);
    output::finish();
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
        // Root shortcuts share their Args + handler with the matching `grm pkg <verb>`.
        Command::Install(args) => install::install(args),
        Command::Upgrade(args) => cmd::query::upgrade(args),
        Command::Remove(args) => install::remove(args),
        Command::List(args) => install::list(args),
        Command::Search(args) => cmd::query::search(args),
        Command::Info(args) => cmd::query::info(args),
        Command::Build(args) => build::build(args),

        Command::Pkg { command } => match command {
            PkgCommand::Install(args) => install::install(args),
            PkgCommand::Upgrade(args) => cmd::query::upgrade(args),
            PkgCommand::Remove(args) => install::remove(args),
            PkgCommand::List(args) => install::list(args),
            PkgCommand::Search(args) => cmd::query::search(args),
            PkgCommand::Info(args) => cmd::query::info(args),
            PkgCommand::Build(args) => build::build(args),
            PkgCommand::Hold(args) => install::hold(args),
            PkgCommand::Unhold(args) => install::unhold(args),
            PkgCommand::Files(args) => cmd::files::files(args),
            PkgCommand::Owns(args) => cmd::files::owns(args),
            PkgCommand::Provides(args) => cmd::files::provides(args),
            PkgCommand::Prefer(args) => cmd::prefer::prefer(args),
        },

        Command::Generation { command } => match command {
            GenerationCommand::List => cmd::generations::generations(),
            GenerationCommand::Switch(args) => switch_generation(args),
            GenerationCommand::Lock(args) => install::lock::export(&args.output),
            GenerationCommand::Restore(args) => install::restore(args),
        },

        Command::Tome { command } => match command {
            TomeCommand::Init(args) => tome::init(args),
            TomeCommand::Rune(args) => tome::rune(args),
            TomeCommand::Build(args) => tome::build(args),
            TomeCommand::Add(args) => tome::add(args),
            TomeCommand::Update(args) => tome::update(args),
            TomeCommand::Remove(args) => tome::remove(args),
            TomeCommand::List => tome::list(),
            TomeCommand::News(args) => tome::news::news_command(args.name, args.all),
            TomeCommand::Info(args) => tome::info(args),
            TomeCommand::Lint(args) => tome::lint(args),
            TomeCommand::Sign(args) => tome::sign(args),
        },
        Command::Addendum { command } => match command {
            cli::AddendumCommand::Add(args) => catalog::addendum::add(args),
            cli::AddendumCommand::Remove(args) => catalog::addendum::remove(args),
            cli::AddendumCommand::List => catalog::addendum::list(),
            cli::AddendumCommand::Update(args) => catalog::addendum::update(args),
        },

        Command::Doctor => cmd::doctor::doctor(),
        Command::Clean(args) => cmd::clean::clean(args),
        Command::Setup(args) => cmd::setup::setup(args),

        Command::StoreHash(args) => {
            let hermetic = build::effective_source_build_hermetic(false, false)?;
            for package in &args.packages {
                output::line(&store::closure::store_hash_with_mode(package, hermetic)?);
            }
            Ok(())
        }
        Command::Completions(args) => cmd::man::completions(args),
        Command::Man(args) => cmd::man::man(args),
    }
}

/// `grm generation switch`: re-point the profile to another generation (a specific ID, or the
/// previous one when none is given) and restore its recorded state. Nothing is rebuilt.
fn switch_generation(args: cli::SwitchArgs) -> Result<()> {
    if args.dry_run {
        return profile::dry_run_activation(args.generation);
    }
    let started = std::time::Instant::now();
    let id = match args.generation {
        // `activate_generation` returns false when that generation is already current; no message.
        Some(id) if !profile::activate_generation(id)? => return Ok(()),
        Some(id) => id,
        None => profile::switch_to_previous()?,
    };
    output::report(&format!(
        "{} {}",
        output::accent(&format!(
            "switched to generation {id} in {:.2}s",
            started.elapsed().as_secs_f64(),
        )),
        output::faint("— no rebuild"),
    ));
    Ok(())
}

fn mutates_install_root(command: &Command) -> bool {
    // `--dry-run` resolves and prints a plan without writing anything; it can run while another
    // `grm` holds the lock.
    match command {
        // Root package shortcuts mirror the pkg group.
        Command::Install(args) => !args.dry_run,
        Command::Upgrade(args) => !args.dry_run,
        Command::Remove(args) => !args.dry_run,
        Command::Build(_) => true,
        Command::List(_) | Command::Search(_) | Command::Info(_) => false,

        Command::Pkg { command } => match command {
            PkgCommand::Install(args) => !args.dry_run,
            PkgCommand::Upgrade(args) => !args.dry_run,
            PkgCommand::Remove(args) | PkgCommand::Hold(args) | PkgCommand::Unhold(args) => {
                !args.dry_run
            }
            // Bare `grm pkg prefer` only lists; setting or unsetting mutates state and may relink.
            PkgCommand::Prefer(args) => args.capability.is_some() && !args.dry_run,
            PkgCommand::Build(_) => true,
            PkgCommand::List(_)
            | PkgCommand::Search(_)
            | PkgCommand::Info(_)
            | PkgCommand::Files(_)
            | PkgCommand::Owns(_)
            | PkgCommand::Provides(_) => false,
        },

        Command::Generation { command } => match command {
            GenerationCommand::Switch(args) => !args.dry_run,
            GenerationCommand::Restore(args) => !args.dry_run,
            // `list` is a read; `lock` only writes a user-chosen output path, not install-root state.
            GenerationCommand::List | GenerationCommand::Lock(_) => false,
        },

        Command::Tome { command } => match command {
            TomeCommand::Add(args) => !args.dry_run,
            TomeCommand::Update(args) => !args.dry_run,
            TomeCommand::Remove(args) => !args.dry_run,
            // Every build writes the shared store and state/packages: it installs build deps and
            // built products store-only (§8), without activating a generation.
            // Both must hold the lock so a concurrent `grm clean` cannot reap store paths mid-build.
            TomeCommand::Build(_) => true,
            // Default `tome news` advances the seen marker; `--all` is a pure read.
            TomeCommand::News(args) => !args.all,
            _ => false,
        },
        Command::Addendum { .. } => false,

        Command::Clean(args) => !args.dry_run,
        Command::Doctor
        | Command::Setup(_)
        | Command::StoreHash(_)
        | Command::Completions(_)
        | Command::Man(_) => false,
    }
}
