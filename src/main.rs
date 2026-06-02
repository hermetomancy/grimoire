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
mod resolve;
mod tome;

use anyhow::Result;
use clap::Parser;
use cli::{AddendumCommand, Cli, Command, TomeCommand};

fn main() -> Result<()> {
    let cli = Cli::parse();
    run(cli)
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
