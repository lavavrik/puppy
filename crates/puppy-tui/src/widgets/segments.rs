use puppy_core::types::SegmentView;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::widgets::Widget;

use super::progress::draw_bar;
use super::{put, shade};
use crate::format;
use crate::theme::{Theme, Token};

/// One cell per `total / width` bytes, shaded by how much of that span is downloaded.
pub struct SegmentStrip<'a> {
    pub total: u64,
    pub segments: &'a [SegmentView],
    pub theme: &'a Theme,
}

/// Bytes of `[s, e)` covered by the downloaded prefix of any segment.
fn covered(segments: &[SegmentView], total: u64, s: u64, e: u64) -> u128 {
    let mut sum: u128 = 0;
    for seg in segments {
        let lo = seg.start.min(total);
        let hi = seg.end.min(total);
        let len = hi.saturating_sub(lo);
        let filled_hi = lo + seg.done.min(len);
        let a = lo.max(s);
        let b = filled_hi.min(e);
        if b > a {
            sum += u128::from(b - a);
        }
    }
    sum.min(u128::from(e - s))
}

impl Widget for SegmentStrip<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let area = area.intersection(buf.area);
        if area.width == 0 || area.height == 0 || self.total == 0 {
            return;
        }
        let total = u128::from(self.total);
        let w = u128::from(area.width);
        for i in 0..area.width {
            let s = (u128::from(i) * total / w) as u64;
            if s >= self.total {
                break;
            }
            let e = ((u128::from(i) + 1) * total / w) as u64;
            let e = e.max(s + 1).min(self.total);
            let cov = covered(self.segments, self.total, s, e);
            let len = u128::from(e - s);
            let active = self.segments.iter().any(|g| {
                let cur = g.start.saturating_add(g.done);
                g.active && cur >= s && cur < e && cur < g.end.min(self.total)
            });
            let token = if active {
                Token::SegActive
            } else if cov == len {
                Token::SegDone
            } else {
                Token::SegTodo
            };
            put(
                buf,
                area.x + i,
                area.y,
                shade(cov, len),
                self.theme.style(token),
            );
        }
    }
}

/// One row per active connection: `conn N <bar> <speed>`.
pub struct ConnectionBars<'a> {
    pub segments: &'a [SegmentView],
    pub theme: &'a Theme,
    pub max_rows: usize,
}

impl Widget for ConnectionBars<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let area = area.intersection(buf.area);
        if area.width == 0 || area.height == 0 {
            return;
        }
        // Numbered 1..N among the active segments.
        let rows: Vec<(usize, &SegmentView)> = self
            .segments
            .iter()
            .filter(|g| g.active)
            .enumerate()
            .take(self.max_rows.min(usize::from(area.height)))
            .collect();
        let speeds: Vec<String> = rows.iter().map(|(_, g)| format::speed(g.bps)).collect();
        let sw = speeds.iter().map(|s| s.chars().count()).max().unwrap_or(0);
        let nw = rows
            .iter()
            .map(|(i, _)| (i + 1).to_string().len())
            .max()
            .unwrap_or(1);
        let label_w = "conn ".len() + nw;
        let text = self.theme.style(Token::Text);
        let dim = self.theme.style(Token::Dim);
        for (r, ((idx, g), speed)) in rows.iter().zip(&speeds).enumerate() {
            let y = area.y + r as u16;
            let label = format!("{:<label_w$}", format!("conn {}", idx + 1));
            let mut x = 0usize;
            let max = usize::from(area.width);
            let text_at = |x: &mut usize, s: &str, style: Style, buf: &mut Buffer| {
                for c in s.chars() {
                    if *x >= max {
                        return;
                    }
                    let mut b = [0u8; 4];
                    put(buf, area.x + *x as u16, y, c.encode_utf8(&mut b), style);
                    *x += 1;
                }
            };
            text_at(&mut x, &label, dim, buf);
            text_at(&mut x, " ", dim, buf);
            // Bar gets what is left after " <speed>".
            let bar_w = max.saturating_sub(x + 1 + sw);
            if bar_w > 0 {
                let len = g.end.saturating_sub(g.start);
                let ratio = if len == 0 {
                    0.0
                } else {
                    g.done.min(len) as f64 / len as f64
                };
                let eighths = (ratio * bar_w as f64 * 8.0 + 1e-9).floor() as u64;
                draw_bar(buf, area.x + x as u16, y, bar_w as u16, eighths, self.theme);
                x += bar_w;
                text_at(&mut x, " ", dim, buf);
            }
            let pad = sw.saturating_sub(speed.chars().count());
            x += pad.min(max.saturating_sub(x));
            text_at(&mut x, speed, text, buf);
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

    fn seg(start: u64, end: u64, done: u64, active: bool) -> SegmentView {
        SegmentView {
            start,
            end,
            done,
            bps: 0,
            active,
        }
    }

    fn strip(total: u64, segs: &[SegmentView], w: u16) -> Buffer {
        let t = theme();
        let area = Rect::new(0, 0, w, 1);
        let mut buf = Buffer::empty(area);
        SegmentStrip {
            total,
            segments: segs,
            theme: &t,
        }
        .render(area, &mut buf);
        buf
    }

    fn row(buf: &Buffer, w: u16) -> String {
        (0..w).map(|x| buf[(x, 0)].symbol().to_string()).collect()
    }

    #[test]
    fn segments_full_file_all_full_blocks() {
        let segs = [seg(0, 50, 50, false), seg(50, 100, 50, false)];
        let buf = strip(100, &segs, 10);
        assert_eq!(row(&buf, 10), "██████████");
        assert_eq!(buf[(3, 0)].fg, theme().p.seg_done);
    }

    #[test]
    fn segments_partial_glyphs() {
        // 4 cells of 25 bytes: fractions 0, 0.2, 0.52, 0.8 -> ░ ░ ▒ ▓ ; then full.
        let segs = [
            seg(0, 25, 0, false),
            seg(25, 50, 5, false),
            seg(50, 75, 13, false),
            seg(75, 100, 20, false),
        ];
        let buf = strip(100, &segs, 4);
        assert_eq!(row(&buf, 4), "░░▒▓");
        assert_eq!(buf[(0, 0)].fg, theme().p.seg_todo);
        let buf = strip(100, &[seg(0, 100, 100, false)], 4);
        assert_eq!(row(&buf, 4), "████");
    }

    #[test]
    fn segments_active_cursor_highlighted() {
        // Cursor at 50 + 10 = 60: lies in cell 2 of 4 (50..75).
        let segs = [seg(0, 50, 50, false), seg(50, 100, 10, true)];
        let buf = strip(100, &segs, 4);
        let t = theme();
        assert_eq!(buf[(0, 0)].fg, t.p.seg_done);
        assert_eq!(buf[(1, 0)].fg, t.p.seg_done);
        assert_eq!(buf[(2, 0)].fg, t.p.seg_active);
        assert_eq!(buf[(3, 0)].fg, t.p.seg_todo);
    }

    #[test]
    fn segments_zero_total_renders_empty_no_panic() {
        let buf = strip(0, &[seg(0, 10, 5, true)], 8);
        assert_eq!(row(&buf, 8), "        ");
        // Out-of-range and over-done segments, total smaller than width.
        let _ = strip(
            3,
            &[
                seg(10, 20, 99, true),
                seg(2, 1, 5, true),
                seg(0, 3, 99, true),
            ],
            8,
        );
        let _ = strip(u64::MAX, &[seg(0, u64::MAX, u64::MAX, true)], 7);
    }

    #[test]
    fn connection_bars_rows_and_speed() {
        let t = theme();
        let segs = [
            SegmentView {
                start: 0,
                end: 100,
                done: 100,
                bps: 0,
                active: false,
            },
            SegmentView {
                start: 100,
                end: 200,
                done: 50,
                bps: 1_677_722,
                active: true,
            },
            SegmentView {
                start: 200,
                end: 300,
                done: 0,
                bps: 1024,
                active: true,
            },
        ];
        let area = Rect::new(0, 0, 30, 3);
        let mut buf = Buffer::empty(area);
        ConnectionBars {
            segments: &segs,
            theme: &t,
            max_rows: 5,
        }
        .render(area, &mut buf);
        let line =
            |y: u16| -> String { (0..30).map(|x| buf[(x, y)].symbol().to_string()).collect() };
        assert!(line(0).starts_with("conn 1 "), "{:?}", line(0));
        assert!(line(0).trim_end().ends_with("1.6 MiB/s"), "{:?}", line(0));
        assert!(line(0).contains('█'));
        assert!(line(1).starts_with("conn 2 "));
        assert!(line(1).trim_end().ends_with("1.0 KiB/s"));
        assert_eq!(line(2).trim(), "");
        // max_rows limits.
        let mut buf = Buffer::empty(area);
        ConnectionBars {
            segments: &segs,
            theme: &t,
            max_rows: 1,
        }
        .render(area, &mut buf);
        assert_eq!(buf[(0, 1)].symbol(), " ");
    }
}
