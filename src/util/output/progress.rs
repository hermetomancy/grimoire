//! Progress tier (stderr): step lines and the transient spinner / live build-log pane.
//!
//! In `--verbose`, [`status`]/[`success`] print persistent `::`/`✓` lines. At the default level
//! they collapse into a single transient spinner that vanishes when the command finishes; under
//! `--quiet` they are dropped. The result still reaches stdout via the result tier.
#![allow(clippy::disallowed_macros)] // this module *is* the output layer

use std::collections::VecDeque;
use std::io::{IsTerminal, Write};
use std::sync::Mutex;
use std::thread;
use std::time::Duration;

use indicatif::{ProgressBar, ProgressStyle};
use owo_colors::OwoColorize;
use terminal_size::{Width, terminal_size};

use super::{Verbosity, prefix, purple, verbosity};

static CURRENT_STATUS: Mutex<Option<String>> = Mutex::new(None);

/// The single transient spinner used in [`Verbosity::Normal`]. Created lazily on the first
/// [`status`] call (only when stderr is a terminal) and cleared by the result tier / [`super::finish`].
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

fn dim_build_log_line(line: &str) -> String {
    if std::env::var_os("NO_COLOR").is_some() || !std::io::stderr().is_terminal() {
        line.to_owned()
    } else {
        line.dimmed().to_string()
    }
}

/// Emits a single build-output line into the live pane (normal) or as a persistent dim line
/// (verbose); dropped under `--quiet` and when stderr is not a terminal (normal).
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

/// Clears the transient spinner (if any) so it does not interleave with a line written elsewhere.
pub(super) fn clear_spinner() {
    if let Ok(mut guard) = SPINNER.lock()
        && let Some(bar) = guard.take()
    {
        bar.finish_and_clear();
    }
}

pub(super) fn clear_live_build_log() {
    if let Ok(mut log) = LIVE_BUILD_LOG.lock() {
        log.clear();
    }
}

/// Prints a granular progress step. In `--verbose` it is a persistent `::` line on stderr; at the
/// default level it is folded into the transient spinner; under `--quiet` it is dropped. The
/// command's result still goes to stdout via the result tier.
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
