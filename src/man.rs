//! Shell completion and man-page generation.
//!
//! Both commands derive their output from the `Cli` clap definition, so the generated
//! completions and man pages stay in sync with the actual CLI as it evolves — no separate
//! source of truth to drift.

use anyhow::{Context, Result};
use clap::CommandFactory;
use clap_complete::generate;
use clap_mangen::Man;
use std::fs;

use crate::cli::{Cli, CompletionsArgs, ManArgs};
use crate::progress::report;

pub fn completions(args: CompletionsArgs) -> Result<()> {
    let mut cmd = Cli::command();
    generate(args.shell, &mut cmd, "grm", &mut std::io::stdout());
    Ok(())
}

pub fn man(args: ManArgs) -> Result<()> {
    fs::create_dir_all(&args.output)
        .with_context(|| format!("create man output directory {}", args.output.display()))?;
    let cmd = Cli::command();
    render_page(&cmd, "grm.1", &args)?;
    let mut count = 1usize;
    for sub in cmd.get_subcommands() {
        // Skip clap's auto-generated `help` subcommand; users don't expect a man page for it.
        if sub.get_name() == "help" {
            continue;
        }
        let file = format!("grm-{}.1", sub.get_name());
        render_page(sub, &file, &args)?;
        count += 1;
    }
    report(&format!(
        "wrote {count} man pages into {}",
        args.output.display()
    ));
    Ok(())
}

fn render_page(cmd: &clap::Command, file: &str, args: &ManArgs) -> Result<()> {
    let path = args.output.join(file);
    let man = Man::new(cmd.clone());
    let mut buffer: Vec<u8> = Vec::new();
    man.render(&mut buffer)
        .with_context(|| format!("render man page for `{}`", cmd.get_name()))?;
    fs::write(&path, buffer).with_context(|| format!("write {}", path.display()))?;
    Ok(())
}
