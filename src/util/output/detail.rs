//! Data tier — key-value detail and section headings for `info`/`doctor`/`tome info`-style output.
//!
//! Both always print (they are the data the user asked for) and stay byte-stable when piped, so
//! scripts and tests keep parsing `key: value` and the plain heading text.
#![allow(clippy::disallowed_macros)] // this module *is* the output layer

use owo_colors::OwoColorize;

use super::progress::{clear_live_build_log, clear_spinner};
use super::result::faint;
use super::stdout_styled;

/// A `key: value` detail line: faint key, plain value. The canonical way to print one field of an
/// information block (replaces the per-command field helpers `info`/`doctor` used to hand-roll).
pub fn field(key: &str, value: &str) {
    clear_spinner();
    clear_live_build_log();
    println!("{} {value}", faint(&format!("{key}:")));
}

/// A section heading that separates blocks of [`field`]/[`super::print_rows`] output: a bold title
/// preceded by a blank line on a terminal, plain otherwise.
pub fn heading(title: &str) {
    clear_spinner();
    clear_live_build_log();
    if stdout_styled() {
        println!("\n{}", title.bold());
    } else {
        println!("\n{title}");
    }
}

/// A raw, unstyled stdout line, always printed — for verbatim machine-readable data a script
/// consumes (a store hash) or a preformatted block the caller has already laid out. Prefer
/// [`field`]/[`heading`]/[`super::print_rows`]/[`list_item`] for anything with structure to render.
pub fn line(text: &str) {
    clear_spinner();
    clear_live_build_log();
    println!("{text}");
}

/// One item of a bulleted list — the single-column counterpart to [`super::print_rows`]. On a
/// terminal: `  • item` with a dimmed bullet; piped: the bare item, one per line, so scripts and
/// tests keep parsing one value per line. Always prints (it is the requested data).
pub fn list_item(text: &str) {
    clear_spinner();
    clear_live_build_log();
    if stdout_styled() {
        println!("  {} {text}", "•".dimmed());
    } else {
        println!("{text}");
    }
}

/// One step of a dry-run plan: a change marker colored by kind — `+` add (green), `-` remove (red),
/// `~` change (yellow) — followed by the step text. On a terminal the marker is bold-colored; piped,
/// the bare `  + text` form is kept byte-for-byte so scripts and tests keep parsing plans. Always
/// prints (a plan is requested data).
pub fn plan_item(marker: char, text: &str) {
    clear_spinner();
    clear_live_build_log();
    if stdout_styled() {
        let painted = match marker {
            '+' => super::green(&marker.to_string()),
            '-' => super::red(&marker.to_string()),
            '~' => super::yellow(&marker.to_string()),
            _ => marker.to_string(),
        };
        println!("  {painted} {text}");
    } else {
        println!("  {marker} {text}");
    }
}
