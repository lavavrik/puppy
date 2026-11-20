//! Custom ratatui widgets: sub-character progress bar, HTTP segment strip, torrent
//! piece map. All arithmetic is panic-free for degenerate sizes and totals.

mod pieces;
mod progress;
mod segments;

pub use pieces::PieceMap;
pub use progress::ProgressBar;
pub use segments::{ConnectionBars, SegmentStrip};

use ratatui::buffer::Buffer;
use ratatui::style::Style;

/// Writes one cell, silently ignoring positions outside the buffer. The cell is reset
/// first, so whatever was drawn underneath (a selected row's background or REVERSED
/// modifier) cannot leak into the widget.
pub(crate) fn put(buf: &mut Buffer, x: u16, y: u16, symbol: &str, style: Style) {
    if let Some(cell) = buf.cell_mut((x, y)) {
        cell.reset();
        cell.set_symbol(symbol).set_style(style);
    }
}

/// Glyph for the fraction `covered / len` of a cell that is complete:
/// `░` (<1/3), `▒` (<2/3), `▓` (<1), `█` (=1). An empty span counts as untouched.
pub(crate) fn shade(covered: u128, len: u128) -> &'static str {
    if len == 0 || covered.saturating_mul(3) < len {
        "░"
    } else if covered.saturating_mul(3) < len.saturating_mul(2) {
        "▒"
    } else if covered < len {
        "▓"
    } else {
        "█"
    }
}
