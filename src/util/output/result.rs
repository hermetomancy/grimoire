//! Result tier (stdout, except [`problem`] on stderr): the outcomes a command reports.
//!
//! [`report`] `✦` is the headline result; [`warn`] `!` a caution; [`problem`] `✗` a problem the
//! user must see (stderr, always shown); [`note`] a dim context line. [`accent`]/[`strong`]/
//! [`faint`] are inline emphasis embedded inside those messages. `report`/`warn`/`note` are
//! suppressed under `--quiet`; `problem` is a diagnostic and always prints.
#![allow(clippy::disallowed_macros)] // this module *is* the output layer

use std::io::IsTerminal;

use owo_colors::OwoColorize;

use super::progress::{clear_live_build_log, clear_spinner};
use super::{Verbosity, prefix, purple, red, stdout_styled, verbosity, yellow};

/// Emphasizes the subject of a result line (a package name and version, a generation id) on a
/// terminal. Plain text when stdout is piped or `NO_COLOR` is set.
pub fn strong(text: &str) -> String {
    if stdout_styled() {
        text.bold().to_string()
    } else {
        text.to_owned()
    }
}

/// Accents the subject of a `✦` headline result (the package and version that was installed, the
/// outcome of a switch) in bold green on a terminal. Plain text when stdout is piped or `NO_COLOR`
/// is set. [`strong`] is for subjects embedded in [`note`] context lines; `accent` is for the
/// headline outcomes themselves.
pub fn accent(text: &str) -> String {
    if stdout_styled() {
        text.bold().green().to_string()
    } else {
        text.to_owned()
    }
}

/// De-emphasizes trailing detail on a result line (`— prebuilt, checksum verified`) on a terminal.
/// Plain text when stdout is piped or `NO_COLOR` is set.
pub fn faint(text: &str) -> String {
    if stdout_styled() {
        text.dimmed().to_string()
    } else {
        text.to_owned()
    }
}

/// Prints a persistent secondary confirmation — a context or transition line that frames the `✦`
/// headline results without competing with them. Dimmed and unprefixed on a terminal (embed
/// [`strong`] subjects *inside* the message; they read through the dimming), plain when piped,
/// suppressed under `--quiet`.
pub fn note(message: &str) {
    if verbosity() == Verbosity::Quiet {
        return;
    }
    clear_spinner();
    clear_live_build_log();
    if stdout_styled() {
        // Hand-rolled SGR so embedded [`strong`] subjects survive the dimming: clear the dim flag
        // before an embedded bold starts, and re-open dim after its reset — otherwise the subject's
        // `\x1b[0m` would un-dim the rest of the line.
        let message = message
            .replace("\x1b[1m", "\x1b[22m\x1b[1m")
            .replace("\x1b[0m", "\x1b[0m\x1b[2m");
        println!("\x1b[2m{message}\x1b[0m");
    } else {
        println!("{message}");
    }
}

/// Prints the bare-minimum result line to stdout unless `--quiet` is set: a mutating command's
/// confirmation (installed/removed/built/…). Explicitly-requested data uses the data tier instead,
/// so it still prints under `--quiet`. Clears the spinner first so the two do not interleave.
pub fn report(message: &str) {
    if verbosity() == Verbosity::Quiet {
        return;
    }
    clear_spinner();
    clear_live_build_log();
    match prefix(std::io::stdout().is_terminal(), "✦", purple) {
        Some(p) => println!("{p} {message}"),
        None => println!("{message}"),
    }
}

/// Prints a cautionary result line to stdout unless `--quiet` is set: a `!` on a terminal, plain
/// otherwise. For surprises that deserve a glance but do not stop the command — a major-version
/// upgrade, a skipped held package.
pub fn warn(message: &str) {
    if verbosity() == Verbosity::Quiet {
        return;
    }
    clear_spinner();
    clear_live_build_log();
    match prefix(std::io::stdout().is_terminal(), "!", yellow) {
        Some(p) => println!("{p} {message}"),
        None => println!("{message}"),
    }
}

/// Writes an interactive prompt to stdout *without* a trailing newline and flushes, leaving the
/// cursor after it for the user's typed reply. For the rare command that reads a choice from stdin.
pub fn prompt(message: &str) {
    use std::io::Write;
    clear_spinner();
    clear_live_build_log();
    print!("{message}");
    let _ = std::io::stdout().flush();
}

/// Prints a problem to stderr: a red `✗` on a terminal, the byte-stable `grimoire: ` prefix when
/// piped. Problems are diagnostics the user explicitly asked for (a `doctor` finding, a non-fatal
/// failure), so they print regardless of verbosity and go to stderr, keeping stdout for results.
pub fn problem(message: &str) {
    clear_spinner();
    clear_live_build_log();
    if std::io::stderr().is_terminal() && std::env::var_os("NO_COLOR").is_none() {
        eprintln!("{} {message}", red("✗"));
    } else {
        eprintln!("grimoire: {message}");
    }
}
