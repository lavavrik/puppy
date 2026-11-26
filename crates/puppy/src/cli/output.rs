//! Output that is safe to pipe: a closed pipe (`puppy ls | head -1`) is noted, not fatal,
//! and tables are aligned by display width.

use puppy_tui::format::{display_safe, truncate};
use serde::Serialize;
use std::fmt::Display;
use std::io::{ErrorKind, IsTerminal, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use unicode_width::UnicodeWidthStr;

/// Width of the gap between table columns.
const GAP: usize = 2;
/// A flexible column is never squeezed below this many columns.
const MIN_FLEX: usize = 12;

/// Whether a write failed because the reader went away.
pub(crate) fn is_closed_pipe(e: &std::io::Error) -> bool {
    // Windows: ERROR_NO_DATA (232) is "the pipe is being closed".
    e.kind() == ErrorKind::BrokenPipe || (cfg!(windows) && e.raw_os_error() == Some(232))
}

/// Set once a write found stdout closed (`puppy ls | head -1`).
static STDOUT_CLOSED: AtomicBool = AtomicBool::new(false);

/// Whether stdout has been closed by its reader. Long-running commands stop on it.
pub fn stdout_closed() -> bool {
    STDOUT_CLOSED.load(Ordering::Relaxed)
}

/// Writes `line` and a newline to stdout. A closed pipe is not an error: it sets
/// [`stdout_closed`] and later output is dropped, so the command still finishes its
/// side effects and exits with its real code.
pub fn out(line: impl Display) {
    if stdout_closed() {
        return;
    }
    let mut stdout = std::io::stdout().lock();
    if let Err(e) = writeln!(stdout, "{line}") {
        if is_closed_pipe(&e) {
            STDOUT_CLOSED.store(true, Ordering::Relaxed);
            return;
        }
        // Another stdout failure (e.g. disk full): nothing useful can be printed
        // there; say so on stderr and fail.
        err(format_args!("puppy: cannot write to stdout: {e}"));
        std::process::exit(1);
    }
}

/// Writes `line` and a newline to stderr; failures are ignored.
pub fn err(line: impl Display) {
    let _ = writeln!(std::io::stderr().lock(), "{line}");
}

/// Prints `v` as one line of JSON on stdout.
pub fn json<T: Serialize>(v: &T) {
    match serde_json::to_string(v) {
        Ok(s) => out(s),
        Err(e) => err(format_args!("puppy: cannot encode JSON: {e}")),
    }
}

/// How wide a table column is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ColWidth {
    Fixed(usize),
    /// Takes the width left over by the other columns.
    Flex,
}

/// Terminal width: `COLUMNS`, else the terminal's when stdout is one, else `None`
/// (piped: nothing is truncated).
fn terminal_width() -> Option<usize> {
    if let Some(n) = std::env::var("COLUMNS")
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .filter(|n| *n > 0)
    {
        return Some(n);
    }
    if std::io::stdout().is_terminal()
        && let Ok((cols, _)) = crossterm::terminal::size()
        && cols > 0
    {
        return Some(cols as usize);
    }
    None
}

/// Prints a table (header line, then rows) with columns aligned by display width.
pub fn table(headers: &[&str], rows: &[Vec<String>], widths: &[ColWidth]) {
    for line in render_table(headers, rows, widths, terminal_width()) {
        out(line);
    }
}

fn cell(text: &str, width: usize, pad: bool) -> String {
    let text = display_safe(text);
    let mut s = truncate(&text, width);
    if pad {
        let used = s.width();
        s.extend(std::iter::repeat_n(' ', width.saturating_sub(used)));
    }
    s
}

/// The table's lines for a terminal `term_width` columns wide (`None`: unbounded, flex
/// columns fit their widest cell). Trailing spaces are trimmed.
pub fn render_table(
    headers: &[&str],
    rows: &[Vec<String>],
    widths: &[ColWidth],
    term_width: Option<usize>,
) -> Vec<String> {
    let n = headers.len().min(widths.len());
    let fixed: usize = widths[..n]
        .iter()
        .map(|w| match w {
            ColWidth::Fixed(w) => *w,
            ColWidth::Flex => 0,
        })
        .sum();
    let flex_cols = widths[..n].iter().filter(|w| **w == ColWidth::Flex).count();
    let flex = match term_width {
        Some(w) => {
            let left = w.saturating_sub(fixed + GAP * n.saturating_sub(1));
            left.checked_div(flex_cols).unwrap_or(0).max(MIN_FLEX)
        }
        None => headers
            .iter()
            .copied()
            .chain(rows.iter().flatten().map(String::as_str))
            .map(|c| display_safe(c).width())
            .max()
            .unwrap_or(0)
            .max(MIN_FLEX),
    };
    let widths: Vec<usize> = widths[..n]
        .iter()
        .map(|w| match w {
            ColWidth::Fixed(w) => *w,
            ColWidth::Flex => flex,
        })
        .collect();
    let line = |cells: &mut dyn Iterator<Item = &str>| {
        let mut s = String::new();
        for (i, w) in widths.iter().enumerate() {
            if i > 0 {
                s.push_str(&" ".repeat(GAP));
            }
            s.push_str(&cell(cells.next().unwrap_or(""), *w, true));
        }
        s.truncate(s.trim_end().len());
        s
    };
    let mut lines = vec![line(&mut headers.iter().copied())];
    lines.extend(rows.iter().map(|r| line(&mut r.iter().map(String::as_str))));
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows(v: &[&[&str]]) -> Vec<Vec<String>> {
        v.iter()
            .map(|r| r.iter().map(|s| s.to_string()).collect())
            .collect()
    }

    #[test]
    fn aligns_by_display_width_and_truncates() {
        let lines = render_table(
            &["ID", "NAME", "X"],
            &rows(&[&["1", "日本語ファイル名.iso", "a"], &["22", "plain", "b"]]),
            &[ColWidth::Fixed(3), ColWidth::Fixed(10), ColWidth::Fixed(1)],
            Some(100),
        );
        assert_eq!(lines[0], "ID   NAME        X");
        // Wide chars count two columns: 4 of them and the ellipsis fill 9 of the 10.
        assert_eq!(lines[1], "1    日本語フ…   a");
        assert_eq!(lines[2], "22   plain       b");
        for l in &lines[1..] {
            assert_eq!(l.width(), lines[0].width(), "{l}");
        }
    }

    #[test]
    fn flex_takes_the_rest() {
        let lines = render_table(
            &["A", "B"],
            &rows(&[&["x", "0123456789abcdef"]]),
            &[ColWidth::Fixed(3), ColWidth::Flex],
            Some(22),
        );
        // 22 - 3 - 2 gap = 17 for the flex column.
        assert_eq!(lines[1], "x    0123456789abcdef");
        // Squeezed: the flex column keeps at least MIN_FLEX.
        let lines = render_table(
            &["A", "B"],
            &rows(&[&["x", "0123456789abcdef"]]),
            &[ColWidth::Fixed(3), ColWidth::Flex],
            Some(12),
        );
        assert_eq!(lines[1], "x    0123456789a…");
    }

    #[test]
    fn unbounded_width_never_truncates() {
        let long = "n".repeat(150);
        let lines = render_table(
            &["A", "B"],
            &rows(&[&["x", &long]]),
            &[ColWidth::Fixed(3), ColWidth::Flex],
            None,
        );
        assert_eq!(lines[1], format!("x    {long}"));
    }
}
