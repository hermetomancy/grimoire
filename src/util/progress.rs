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
//! Progress goes to stderr so stdout carries only command results/data (AGENTS.md §12.1). Color and
//! the `::`/`✓` decorations are only emitted when the target stream is a real terminal and
//! `NO_COLOR` is unset, so piped or captured output stays plain and byte-stable. The spinner uses
//! [`indicatif`], which draws to stderr and auto-hides when stderr is not a terminal.

use std::collections::VecDeque;
use std::io::{IsTerminal, Write};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU8, Ordering};
use std::thread;
use std::time::Duration;

use indicatif::{ProgressBar, ProgressStyle};
use owo_colors::OwoColorize;
use terminal_size::{Width, terminal_size};

/// How much a command prints. Ordered least-to-most verbose.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verbosity {
    Quiet = 0,
    Normal = 1,
    Verbose = 2,
}

static VERBOSITY: AtomicU8 = AtomicU8::new(Verbosity::Normal as u8);
static CURRENT_STATUS: Mutex<Option<String>> = Mutex::new(None);

/// The single transient spinner used in [`Verbosity::Normal`]. Created lazily on the first
/// [`status`] call (only when stderr is a terminal) and cleared by [`report`]/[`finish`].
static SPINNER: Mutex<Option<ProgressBar>> = Mutex::new(None);
static LIVE_BUILD_LOG: Mutex<LiveBuildLog> = Mutex::new(LiveBuildLog::new());

struct LiveBuildLog {
    lines: VecDeque<String>,
    active: bool,
    spinner_frame: usize,
}

impl LiveBuildLog {
    const HEIGHT: usize = 5;
    const SPINNER_FRAMES: [&'static str; 29] = [
        "⠁", "⠁", "⠉", "⠙", "⠚", "⠒", "⠂", "⠂", "⠒", "⠲", "⠴", "⠤", "⠄", "⠄", "⠤", "⠠", "⠠", "⠤",
        "⠦", "⠖", "⠒", "⠐", "⠐", "⠒", "⠓", "⠋", "⠉", "⠈", "⠈",
    ];

    const fn new() -> Self {
        Self {
            lines: VecDeque::new(),
            active: false,
            spinner_frame: 0,
        }
    }

    fn push(&mut self, line: &str) {
        if !self.active {
            self.active = true;
            self.spinner_frame = 0;
            clear_spinner();
            self.write_status_line();
            self.reserve();
            start_live_spinner();
        }
        while self.lines.len() >= Self::HEIGHT {
            self.lines.pop_front();
        }
        self.lines.push_back(line.to_owned());
        self.redraw();
    }

    fn reserve(&self) {
        let mut stderr = std::io::stderr().lock();
        for _ in 0..Self::HEIGHT {
            let _ = writeln!(stderr);
        }
        let _ = stderr.flush();
    }

    fn tick_spinner(&mut self) {
        if !self.active {
            return;
        }
        self.spinner_frame = (self.spinner_frame + 1) % Self::SPINNER_FRAMES.len();
        self.redraw_status();
    }

    fn write_status_line(&self) {
        let Some(line) = self.status_line() else {
            return;
        };
        let mut stderr = std::io::stderr().lock();
        let _ = writeln!(stderr, "{line}");
        let _ = stderr.flush();
    }

    fn redraw_status(&self) {
        let Some(line) = self.status_line() else {
            return;
        };
        let mut stderr = std::io::stderr().lock();
        let _ = write!(stderr, "\x1b[{}F\r\x1b[2K{line}", Self::HEIGHT + 1);
        let _ = write!(stderr, "\x1b[{}E", Self::HEIGHT + 1);
        let _ = stderr.flush();
    }

    fn status_line(&self) -> Option<String> {
        let message = CURRENT_STATUS
            .lock()
            .ok()
            .and_then(|current| current.clone())?;
        let width = terminal_width();
        let frame = Self::SPINNER_FRAMES[self.spinner_frame];
        let message = truncate_line(&message, width, 3);
        Some(format!("{} {message}", spinner_frame(frame)))
    }

    fn redraw(&mut self) {
        let mut stderr = std::io::stderr().lock();
        let width = terminal_width();
        let _ = write!(stderr, "\x1b[{}F", Self::HEIGHT);
        let blank_count = Self::HEIGHT.saturating_sub(self.lines.len());
        for _ in 0..blank_count {
            let _ = writeln!(stderr, "\r\x1b[2K");
        }
        for line in &self.lines {
            let _ = writeln!(
                stderr,
                "\r\x1b[2K  {}",
                dim_build_log_line(&truncate_line(line, width, 3))
            );
        }
        let _ = stderr.flush();
    }

    fn clear(&mut self) {
        if !self.active {
            return;
        }
        let mut stderr = std::io::stderr().lock();
        let _ = write!(stderr, "\x1b[{}F\x1b[J", Self::HEIGHT + 1);
        let _ = stderr.flush();
        self.lines.clear();
        self.active = false;
        self.spinner_frame = 0;
    }
}

fn terminal_width() -> usize {
    terminal_size()
        .map(|(Width(width), _)| width as usize)
        .unwrap_or(80)
}

fn truncate_line(line: &str, width: usize, reserved_columns: usize) -> String {
    let clean = line.replace(['\r', '\n', '\t'], " ").replace('\x1b', " ");
    let max = width.saturating_sub(reserved_columns);
    if clean.chars().count() <= max {
        return clean;
    }
    if max <= 3 {
        return ".".repeat(max);
    }
    let mut out = clean.chars().take(max - 3).collect::<String>();
    out.push_str("...");
    out
}

fn start_live_spinner() {
    let _ = thread::Builder::new()
        .name("grimoire-live-build-spinner".to_string())
        .spawn(|| {
            loop {
                thread::sleep(Duration::from_millis(80));
                let Ok(mut log) = LIVE_BUILD_LOG.lock() else {
                    break;
                };
                if !log.active {
                    break;
                }
                log.tick_spinner();
            }
        });
}

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

pub fn build_log_line(line: &str) {
    match verbosity() {
        Verbosity::Quiet => {}
        Verbosity::Verbose => {
            clear_spinner();
            eprintln!("  {line}");
        }
        Verbosity::Normal => {
            if !std::io::stderr().is_terminal() {
                return;
            }
            clear_spinner();
            if let Ok(mut log) = LIVE_BUILD_LOG.lock() {
                log.push(line);
            }
        }
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
    ProgressStyle::with_template("{spinner:.bold.magenta} {msg}")
        .unwrap_or_else(|_| ProgressStyle::default_spinner())
}

fn spinner_frame(frame: &str) -> String {
    if std::env::var_os("NO_COLOR").is_some() || !std::io::stderr().is_terminal() {
        frame.to_owned()
    } else {
        frame.bold().magenta().to_string()
    }
}

fn purple(symbol: &str) -> String {
    symbol.bold().purple().to_string()
}

fn dim_build_log_line(line: &str) -> String {
    if std::env::var_os("NO_COLOR").is_some() || !std::io::stderr().is_terminal() {
        line.to_owned()
    } else {
        line.dimmed().to_string()
    }
}

/// Updates (creating if necessary) the transient Normal-mode spinner. No-op when stderr is not a
/// terminal, so non-interactive runs print nothing.
fn set_spinner_message(message: &str) {
    if let Ok(mut current) = CURRENT_STATUS.lock() {
        *current = Some(message.to_owned());
    }
    if !std::io::stderr().is_terminal() {
        return;
    }
    clear_live_build_log();
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
    if let Ok(mut guard) = SPINNER.lock()
        && let Some(bar) = guard.take()
    {
        bar.finish_and_clear();
    }
}

fn clear_live_build_log() {
    if let Ok(mut log) = LIVE_BUILD_LOG.lock() {
        log.clear();
    }
}

/// Prints a granular progress step. In `--verbose` it is a persistent `::` line on stderr; at the
/// default level it is folded into the transient spinner; under `--quiet` it is dropped. The
/// command's result still goes to stdout via [`report`].
pub fn status(message: &str) {
    match verbosity() {
        Verbosity::Quiet => {}
        Verbosity::Verbose => match prefix(std::io::stderr().is_terminal(), "::", purple) {
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
        match prefix(std::io::stderr().is_terminal(), "✓", purple) {
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
    clear_live_build_log();
    match prefix(std::io::stdout().is_terminal(), "::", purple) {
        Some(p) => println!("{p} {message}"),
        None => println!("{message}"),
    }
}

/// Tears down any live spinner. Call once after a command finishes (and before printing
/// explicitly-requested data) so the transient progress line never lingers in front of results or
/// an error report.
pub fn finish() {
    clear_spinner();
    clear_live_build_log();
}
