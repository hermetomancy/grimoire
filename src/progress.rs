//! Output verbosity and styling, shared by every command and selected once from the global
//! `--quiet` and `--verbose` flags. Three levels:
//!
//! - **Verbose** — granular step-by-step progress on stderr ([`status`]/[`success`]) printed as
//!   persistent pacman-style lines on top of the normal output.
//! - **Normal** (default) — granular progress is collapsed into a single transient spinner on
//!   stderr that shows the current step and vanishes when the command finishes; only the
//!   bare-minimum result lines ([`report`]) reach stdout.
//! - **Quiet** — suppresses progress and result confirmations; explicitly requested data (the
//!   `println!` output of `list`/`search`/`info`/`doctor`) and errors still print.
//!
//! Progress goes to stderr so stdout carries only command results/data (AGENTS.md §7). Color and
//! the `::`/`✓` decorations are only emitted when the target stream is a real terminal and
//! `NO_COLOR` is unset, so piped or captured output stays plain and byte-stable. The spinner uses
//! [`indicatif`], which draws to stderr and auto-hides when stderr is not a terminal.

use std::io::IsTerminal;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU8, Ordering};
use std::time::Duration;

use indicatif::{ProgressBar, ProgressStyle};
use owo_colors::OwoColorize;

/// How much a command prints. Ordered least-to-most verbose.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verbosity {
    Quiet = 0,
    Normal = 1,
    Verbose = 2,
}

static VERBOSITY: AtomicU8 = AtomicU8::new(Verbosity::Normal as u8);

/// The single transient spinner used in [`Verbosity::Normal`]. Created lazily on the first
/// [`status`] call (only when stderr is a terminal) and cleared by [`report`]/[`finish`].
static SPINNER: Mutex<Option<ProgressBar>> = Mutex::new(None);

/// Sets the process-wide verbosity from the parsed CLI flags. Called once in `main` before any
/// command runs; the output helpers read it from there.
pub fn set_verbosity(verbosity: Verbosity) {
    VERBOSITY.store(verbosity as u8, Ordering::Relaxed);
}

fn verbosity() -> Verbosity {
    match VERBOSITY.load(Ordering::Relaxed) {
        0 => Verbosity::Quiet,
        2 => Verbosity::Verbose,
        _ => Verbosity::Normal,
    }
}

/// Builds a decorated prefix for a line on the given stream, or `None` when the stream is not a
/// terminal so piped output stays plain. Honors `NO_COLOR` by dropping the color while keeping the
/// symbol.
fn prefix(is_tty: bool, symbol: &str, paint: impl FnOnce(&str) -> String) -> Option<String> {
    if !is_tty {
        return None;
    }
    if std::env::var_os("NO_COLOR").is_some() {
        Some(symbol.to_owned())
    } else {
        Some(paint(symbol))
    }
}

fn spinner_style() -> ProgressStyle {
    ProgressStyle::with_template("{spinner:.cyan} {msg}")
        .unwrap_or_else(|_| ProgressStyle::default_spinner())
}

/// Updates (creating if necessary) the transient Normal-mode spinner. No-op when stderr is not a
/// terminal, so non-interactive runs print nothing.
fn set_spinner_message(message: &str) {
    if !std::io::stderr().is_terminal() {
        return;
    }
    let Ok(mut guard) = SPINNER.lock() else {
        return;
    };
    let bar = guard.get_or_insert_with(|| {
        let bar = ProgressBar::new_spinner();
        bar.set_style(spinner_style());
        bar.enable_steady_tick(Duration::from_millis(80));
        bar
    });
    bar.set_message(message.to_owned());
}

/// Clears the transient spinner (if any) so it does not interleave with a line written to stdout.
fn clear_spinner() {
    if let Ok(mut guard) = SPINNER.lock() {
        if let Some(bar) = guard.take() {
            bar.finish_and_clear();
        }
    }
}

/// Prints a granular progress step. In `--verbose` it is a persistent `::` line on stderr; at the
/// default level it is folded into the transient spinner; under `--quiet` it is dropped. The
/// command's result still goes to stdout via [`report`].
pub fn status(message: &str) {
    match verbosity() {
        Verbosity::Quiet => {}
        Verbosity::Verbose => match prefix(std::io::stderr().is_terminal(), "::", |s| {
            s.bold().blue().to_string()
        }) {
            Some(p) => eprintln!("{p} {message}"),
            None => eprintln!("{message}"),
        },
        Verbosity::Normal => set_spinner_message(message),
    }
}

/// Prints a step-completed confirmation as a `✓` line on stderr, shown only when `--verbose` is
/// set. At the default level the spinner already conveys progress, so this is a no-op.
pub fn success(message: &str) {
    if verbosity() == Verbosity::Verbose {
        match prefix(std::io::stderr().is_terminal(), "✓", |s| {
            s.bold().green().to_string()
        }) {
            Some(p) => eprintln!("{p} {message}"),
            None => eprintln!("{message}"),
        }
    }
}

/// Prints a bare-minimum result line to stdout unless `--quiet` is set. Use for a mutating
/// command's confirmation (installed/removed/built/…). Explicitly requested data uses plain
/// `println!` instead, so it still prints under `--quiet`. Clears the spinner first so the two do
/// not interleave.
pub fn report(message: &str) {
    if verbosity() == Verbosity::Quiet {
        return;
    }
    clear_spinner();
    match prefix(std::io::stdout().is_terminal(), "::", |s| {
        s.bold().green().to_string()
    }) {
        Some(p) => println!("{p} {message}"),
        None => println!("{message}"),
    }
}

/// Tears down any live spinner. Call once after a command finishes (and before printing
/// explicitly-requested data) so the transient progress line never lingers in front of results or
/// an error report.
pub fn finish() {
    clear_spinner();
}
