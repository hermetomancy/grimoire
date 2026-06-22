//! Data tier — aligned-column tables for list-style command data (`list`, `search`, `tome list`, …).
//!
//! On a terminal, rows print as space-padded columns with a two-space gutter, styled with the
//! shared result-line vocabulary. When stdout is piped, rows print as plain tab-separated cells —
//! the byte-stable format scripts and tests parse. Always prints (it is the requested data).
#![allow(clippy::disallowed_macros)] // this module *is* the output layer

use std::io::IsTerminal;

use owo_colors::OwoColorize;

enum CellStyle {
    Plain,
    /// The row's subject (a package or tome name): bold.
    Strong,
    /// De-emphasized detail (a target triple, a tome of origin): dimmed.
    Faint,
    /// A flag that deserves a glance (`held`): bold yellow.
    Caution,
}

pub struct Cell {
    text: String,
    style: CellStyle,
}

impl Cell {
    pub fn plain(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            style: CellStyle::Plain,
        }
    }

    pub fn strong(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            style: CellStyle::Strong,
        }
    }

    pub fn faint(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            style: CellStyle::Faint,
        }
    }

    pub fn caution(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            style: CellStyle::Caution,
        }
    }

    /// Applies the cell's style to `text` (already padded — styling after padding keeps ANSI codes
    /// out of the width computation). Plain when `NO_COLOR` is set.
    fn paint(&self, text: &str) -> String {
        if std::env::var_os("NO_COLOR").is_some() {
            return text.to_owned();
        }
        match self.style {
            CellStyle::Plain => text.to_owned(),
            CellStyle::Strong => text.bold().to_string(),
            CellStyle::Faint => text.dimmed().to_string(),
            CellStyle::Caution => text.bold().yellow().to_string(),
        }
    }
}

/// Prints rows to stdout: aligned and styled on a terminal, tab-separated plain cells when piped
/// (every cell, including empty trailing ones, so the piped format never shifts).
pub fn print_rows(rows: Vec<Vec<Cell>>) {
    if rows.is_empty() {
        return;
    }
    super::progress::clear_spinner();
    super::progress::clear_live_build_log();
    if !std::io::stdout().is_terminal() {
        for row in rows {
            let texts: Vec<&str> = row.iter().map(|cell| cell.text.as_str()).collect();
            println!("{}", texts.join("\t"));
        }
        return;
    }

    for line in aligned_lines(&rows) {
        println!("{line}");
    }
}

fn aligned_lines(rows: &[Vec<Cell>]) -> Vec<String> {
    let columns = rows.iter().map(Vec::len).max().unwrap_or(0);
    let mut widths = vec![0usize; columns];
    for row in rows {
        for (index, cell) in row.iter().enumerate() {
            widths[index] = widths[index].max(cell.text.chars().count());
        }
    }

    rows.iter()
        .map(|row| {
            // Empty trailing cells are dropped on a terminal so flagless rows end cleanly.
            let Some(last) = row.iter().rposition(|cell| !cell.text.is_empty()) else {
                return String::new();
            };
            let mut line = String::new();
            for (index, cell) in row.iter().enumerate().take(last + 1) {
                if index > 0 {
                    line.push_str("  ");
                }
                line.push_str(&cell.paint(&cell.text));
                if index < last {
                    let padding = widths[index].saturating_sub(cell.text.chars().count());
                    line.push_str(&" ".repeat(padding));
                }
            }
            line
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn columns_align_and_trailing_empties_drop() {
        let rows = vec![
            vec![Cell::plain("a"), Cell::plain("bb"), Cell::plain("c")],
            vec![Cell::plain("aaa"), Cell::plain("b"), Cell::plain("")],
        ];
        assert_eq!(aligned_lines(&rows), vec!["a    bb  c", "aaa  b"]);
    }

    #[test]
    fn width_uses_char_count_not_bytes() {
        let rows = vec![
            vec![Cell::plain("héllo"), Cell::plain("x")],
            vec![Cell::plain("hi"), Cell::plain("y")],
        ];
        assert_eq!(aligned_lines(&rows), vec!["héllo  x", "hi     y"]);
    }

    #[test]
    fn fully_empty_row_renders_blank() {
        let rows = vec![vec![Cell::plain(""), Cell::plain("")]];
        assert_eq!(aligned_lines(&rows), vec![""]);
    }
}
