use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::widgets::Widget;

use super::{put, shade};
use crate::theme::{Theme, Token};

/// A torrent piece map: the area filled row-major, each cell aggregating
/// `ceil(piece_count / cells)` pieces. `have` is the wire bitfield (MSB first).
pub struct PieceMap<'a> {
    pub piece_count: u32,
    pub have: &'a [u8],
    pub theme: &'a Theme,
}

fn has(have: &[u8], i: u64) -> bool {
    have.get((i / 8) as usize)
        .is_some_and(|b| b >> (7 - (i % 8) as u32) & 1 == 1)
}

impl Widget for PieceMap<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let area = area.intersection(buf.area);
        let cells = u64::from(area.width) * u64::from(area.height);
        let pc = u64::from(self.piece_count);
        if cells == 0 || pc == 0 {
            return;
        }
        let per = pc.div_ceil(cells);
        let used = pc.div_ceil(per);
        for c in 0..used.min(cells) {
            let from = c * per;
            let to = (from + per).min(pc);
            let got = (from..to).filter(|&i| has(self.have, i)).count() as u128;
            let len = u128::from(to - from);
            let token = if got == len {
                Token::SegDone
            } else {
                Token::SegTodo
            };
            let x = area.x + (c % u64::from(area.width)) as u16;
            let y = area.y + (c / u64::from(area.width)) as u16;
            put(buf, x, y, shade(got, len), self.theme.style(token));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::ThemeChoice;

    fn theme() -> Theme {
        Theme::detect(ThemeChoice::Dark, &|k| {
            (k == "COLORTERM").then(|| "truecolor".to_string())
        })
    }

    fn render(pc: u32, have: &[u8], w: u16, h: u16) -> Buffer {
        let t = theme();
        let area = Rect::new(0, 0, w, h);
        let mut buf = Buffer::empty(area);
        PieceMap {
            piece_count: pc,
            have,
            theme: &t,
        }
        .render(area, &mut buf);
        buf
    }

    fn row(buf: &Buffer, y: u16, w: u16) -> String {
        (0..w).map(|x| buf[(x, y)].symbol().to_string()).collect()
    }

    #[test]
    fn pieces_bitfield_msb_first() {
        let buf = render(4, &[0b1010_0000], 4, 1);
        assert_eq!(row(&buf, 0, 4), "█░█░");
        assert_eq!(buf[(0, 0)].fg, theme().p.seg_done);
        assert_eq!(buf[(1, 0)].fg, theme().p.seg_todo);
    }

    #[test]
    fn pieces_more_pieces_than_cells_aggregates() {
        // 8 pieces in 4 cells (2 each): 11, 10, 01(=1 of 2), 00.
        let buf = render(8, &[0b1110_0100], 4, 1);
        assert_eq!(row(&buf, 0, 4), "█▒▒░");
        // Row-major over two rows: 6 pieces in 2x2 -> per=2, 3 cells used.
        let buf = render(6, &[0xFF], 2, 2);
        assert_eq!(row(&buf, 0, 2), "██");
        assert_eq!(row(&buf, 1, 2), "█ ");
        // `have` shorter than needed: missing bits are 0.
        let buf = render(16, &[0xFF], 2, 1);
        assert_eq!(row(&buf, 0, 2), "█░");
    }

    #[test]
    fn widgets_never_panic_zero_area() {
        use super::super::{ConnectionBars, ProgressBar, SegmentStrip};
        use puppy_core::types::SegmentView;
        let t = theme();
        let segs = [SegmentView {
            start: 0,
            end: 10,
            done: 5,
            bps: 1,
            active: true,
        }];
        for (w, h) in [(0, 0), (0, 3), (5, 0), (1, 1)] {
            let area = Rect::new(0, 0, w, h);
            let mut buf = Buffer::empty(area);
            ProgressBar {
                ratio: Some(0.5),
                label: Some("x"),
                theme: &t,
                tick: 9,
            }
            .render(area, &mut buf);
            ProgressBar {
                ratio: None,
                label: Some("x"),
                theme: &t,
                tick: 9,
            }
            .render(area, &mut buf);
            SegmentStrip {
                total: 10,
                segments: &segs,
                theme: &t,
            }
            .render(area, &mut buf);
            ConnectionBars {
                segments: &segs,
                theme: &t,
                max_rows: 3,
            }
            .render(area, &mut buf);
            PieceMap {
                piece_count: 0,
                have: &[],
                theme: &t,
            }
            .render(area, &mut buf);
            PieceMap {
                piece_count: 9,
                have: &[],
                theme: &t,
            }
            .render(area, &mut buf);
        }
        // Area larger than the buffer is clipped.
        let mut buf = Buffer::empty(Rect::new(0, 0, 3, 1));
        PieceMap {
            piece_count: 9,
            have: &[0xFF],
            theme: &t,
        }
        .render(Rect::new(0, 0, 50, 5), &mut buf);
    }
}
