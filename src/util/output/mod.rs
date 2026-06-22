//! All user-facing CLI output flows through this module — commands never call `println!` /
//! `eprintln!` directly (enforced by clippy's `disallowed-macros`; see `clippy.toml`). It owns
//! stdout and stderr so output stays cohesive and stream-correct, and selects styling once from
//! the global `--quiet`/`--verbose` flags. Three tiers:
//!
//! - **Result** (stdout): [`report`] `✦` headline outcomes, [`warn`] `!` cautions, [`problem`]
//!   `✗` problems (stderr, always shown), [`note`] dim context, and inline
//!   [`accent`]/[`strong`]/[`faint`] emphasis. `report`/`warn`/`note` are gated by `--quiet`.
//! - **Data** (stdout, always shown — it is what the user explicitly asked for): [`field`]
//!   `key: value` detail, [`heading`] section titles, and [`print_rows`] aligned tables.
//! - **Progress** (stderr): [`status`]/[`success`] step lines (verbose), folded into a transient
//!   spinner (normal), plus the live build-log pane ([`build_log_line`]).
//!
//! Progress and problems go to stderr so stdout carries only results/data (AGENTS.md §12.1). Color
//! and the `✦`/`!`/`✗`/`::`/`✓` decorations are emitted only when the target stream is a real
//! terminal and `NO_COLOR` is unset, so piped or captured output stays plain and byte-stable.
//!
//! Submodules: [`result`] (the result tier), [`detail`]/[`list`] (the data tier), [`progress`]
//! (the progress tier and its spinner/build-log machinery). Shared stream/color helpers live here;
//! they are private but visible to the submodules.

use std::io::IsTerminal;
use std::sync::atomic::{AtomicU8, Ordering};

use owo_colors::OwoColorize;

mod detail;
mod list;
mod progress;
mod result;

pub use detail::{field, heading, line};
pub use list::{Cell, print_rows};
pub use progress::{build_log_line, status, success};
pub use result::{accent, faint, note, problem, prompt, report, strong, warn};

/// How much a command prints. Ordered least-to-most verbose.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verbosity {
    Quiet = 0,
    Normal = 1,
    Verbose = 2,
}

static VERBOSITY: AtomicU8 = AtomicU8::new(Verbosity::Normal as u8);

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

/// Returns the active verbosity as a stable string for build contexts and tests.
pub fn verbosity_name() -> &'static str {
    match verbosity() {
        Verbosity::Quiet => "quiet",
        Verbosity::Normal => "normal",
        Verbosity::Verbose => "verbose",
    }
}

/// Tears down any live spinner / build-log pane. Call once after a command finishes (and before
/// printing explicitly-requested data) so transient progress never lingers in front of results or
/// an error report.
pub fn finish() {
    progress::clear_spinner();
    progress::clear_live_build_log();
}

/// Builds a decorated prefix for a line on the given stream, or `None` when the stream is not a
/// terminal so piped output stays plain. Honors `NO_COLOR` by dropping the color, keeping the symbol.
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

/// Whether stdout lines may carry inline styling: only on a real terminal with `NO_COLOR` unset,
/// so piped or captured output stays plain and byte-stable.
fn stdout_styled() -> bool {
    std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none()
}

fn purple(symbol: &str) -> String {
    symbol.bold().purple().to_string()
}

fn yellow(symbol: &str) -> String {
    symbol.bold().yellow().to_string()
}

fn red(symbol: &str) -> String {
    symbol.bold().red().to_string()
}
