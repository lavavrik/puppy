use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::widgets::Widget;
use unicode_width::UnicodeWidthChar;

use super::put;
use crate::theme::{Theme, Token};

const EIGHTHS: [&str; 9] = [" ", "▏", "▎", "▍", "▌", "▋", "▊", "▉", "█"];
const BLOCK: u16 = 6;

/// A smooth progress bar with eighth-block resolution and an optional centered label.
pub struct ProgressBar<'a> {
    /// `None` ⇒ indeterminate.
    pub ratio: Option<f64>,
    pub label: Option<&'a str>,
    pub theme: &'a Theme,
    /// Animates the indeterminate block.
    pub tick: u64,
}

/// Draws a bar of `width` cells at (`x`, `y`) filled to `eighths` eighths.
/// Returns how many cells are fully filled.
pub(crate) fn draw_bar(
    buf: &mut Buffer,
    x: u16,
    y: u16,
    width: u16,
    eighths: u64,
    theme: &Theme,
) -> u16 {
    let done = theme.style(Token::BarDone);
    let todo = theme.style(Token::BarTodo);
    let todo_bg = Style::new().bg(todo.fg.unwrap_or_default());
    let todo_bg = if todo.fg.is_some() { todo_bg } else { todo };
    let full = (eighths / 8).min(width as u64) as u16;
    let part = (eighths % 8) as usize;
    for i in 0..width {
        let xx = x.saturating_add(i);
        if i < full {
            if theme.mono {
                put(
                    buf,
                    xx,
                    y,
                    " ",
                    Style::new().add_modifier(Modifier::REVERSED),
                );
            } else {
                put(buf, xx, y, "█", done);
            }
        } else if i == full && part > 0 {
            put(buf, xx, y, EIGHTHS[part], done.patch(todo_bg));
        } else {
            put(buf, xx, y, " ", todo_bg);
        }
    }
    full
}

impl Widget for ProgressBar<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let area = area.intersection(buf.area);
        if area.width == 0 || area.height == 0 {
            return;
        }
        let (w, y, x0) = (area.width, area.y, area.x);
        let theme = self.theme;
        // Cells drawn as "filled" (the label then uses the inverse contrast).
        let filled: Box<dyn Fn(u16) -> bool> = match self.ratio {
            Some(r) => {
                let r = if r.is_nan() { 0.0 } else { r.clamp(0.0, 1.0) };
                let eighths = (r * f64::from(w) * 8.0 + 1e-9).floor() as u64;
                let full = draw_bar(buf, x0, y, w, eighths, theme);
                Box::new(move |i| i < full)
            }
            None => {
                let (start, len) = if w < BLOCK {
                    (0, w)
                } else {
                    let span = u64::from(w - BLOCK);
                    let period = 2 * span;
                    let pos = if period == 0 { 0 } else { self.tick % period };
                    let off = if pos <= span { pos } else { period - pos };
                    (off as u16, BLOCK)
                };
                draw_bar(buf, x0, y, w, 0, theme);
                for i in start..start + len {
                    if theme.mono {
                        put(
                            buf,
                            x0 + i,
                            y,
                            " ",
                            Style::new().add_modifier(Modifier::REVERSED),
                        );
                    } else {
                        put(buf, x0 + i, y, "█", theme.style(Token::BarDone));
                    }
                }
                Box::new(move |i| i >= start && i < start + len)
            }
        };
        if let Some(label) = self.label {
            draw_label(buf, area, label, theme, &*filled);
        }
    }
}

fn draw_label(
    buf: &mut Buffer,
    area: Rect,
    label: &str,
    theme: &Theme,
    filled: &dyn Fn(u16) -> bool,
) {
    let max = usize::from(area.width);
    let mut chars: Vec<(char, usize)> = Vec::new();
    let mut total = 0usize;
    for c in label.chars() {
        let cw = c.width().unwrap_or(0);
        if cw == 0 {
            continue;
        }
        if total + cw > max {
            break;
        }
        total += cw;
        chars.push((c, cw));
    }
    let mut col = (max - total) / 2;
    let todo_bg = theme.style(Token::BarTodo).fg;
    let done_bg = theme.style(Token::BarDone).fg;
    for (c, cw) in chars {
        let i = col as u16;
        let style = if theme.mono {
            if filled(i) {
                Style::new().add_modifier(Modifier::REVERSED)
            } else {
                Style::new()
            }
        } else if filled(i) {
            // Inverse contrast: the bar's background colour as text over the fill.
            let mut s = Style::new().add_modifier(Modifier::BOLD);
            s.fg = todo_bg;
            s.bg = done_bg;
            s
        } else {
            let mut s = theme.style(Token::Text);
            s.bg = todo_bg;
            s
        };
        let mut b = [0u8; 4];
        put(buf, area.x + i, area.y, c.encode_utf8(&mut b), style);
        // Blank the continuation cell of a wide glyph.
        for k in 1..cw {
            put(buf, area.x + i + k as u16, area.y, "", style);
        }
        col += cw;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::ThemeChoice;

    pub(crate) fn theme() -> Theme {
        Theme::detect(ThemeChoice::Dark, &|k| {
            (k == "COLORTERM").then(|| "truecolor".to_string())
        })
    }

    fn row(buf: &Buffer, w: u16) -> String {
        (0..w).map(|x| buf[(x, 0)].symbol().to_string()).collect()
    }

    fn render(ratio: Option<f64>, label: Option<&str>, w: u16, tick: u64) -> Buffer {
        let t = theme();
        let area = Rect::new(0, 0, w, 1);
        let mut buf = Buffer::empty(area);
        ProgressBar {
            ratio,
            label,
            theme: &t,
            tick,
        }
        .render(area, &mut buf);
        buf
    }

    #[test]
    fn progress_eighths() {
        let buf = render(Some(0.55), None, 10, 0);
        assert_eq!(row(&buf, 10), "█████▌    ");
        assert_eq!(buf[(0, 0)].fg, theme().p.bar_done);
        assert_eq!(buf[(5, 0)].bg, theme().p.bar_todo);
        assert_eq!(buf[(9, 0)].bg, theme().p.bar_todo);
    }

    #[test]
    fn progress_zero_and_full() {
        assert_eq!(row(&render(Some(0.0), None, 6, 0), 6), "      ");
        assert_eq!(row(&render(Some(1.0), None, 6, 0), 6), "██████");
        assert_eq!(row(&render(Some(7.0), None, 6, 0), 6), "██████");
        assert_eq!(row(&render(Some(-1.0), None, 6, 0), 6), "      ");
        assert_eq!(row(&render(Some(f64::NAN), None, 6, 0), 6), "      ");
    }

    #[test]
    fn progress_label_centered() {
        let buf = render(Some(0.5), Some("50%"), 10, 0);
        assert_eq!(row(&buf, 10), "███50%    ");
        // Over the fill: inverse contrast (fg = bar bg colour); over the rest: normal text.
        let t = theme();
        assert_eq!(buf[(4, 0)].fg, t.p.bar_todo);
        assert_eq!(buf[(4, 0)].bg, t.p.bar_done);
        assert_eq!(buf[(5, 0)].fg, t.p.fg);
        assert_eq!(buf[(5, 0)].bg, t.p.bar_todo);
        // Overlong labels are truncated, not panicking.
        let buf = render(Some(0.5), Some("a very long label"), 5, 0);
        assert_eq!(row(&buf, 5), "a ver");
    }

    #[test]
    fn progress_mono_uses_reversed() {
        let t = Theme::detect(ThemeChoice::Dark, &|k| {
            (k == "NO_COLOR").then(|| "1".into())
        });
        let area = Rect::new(0, 0, 4, 1);
        let mut buf = Buffer::empty(area);
        ProgressBar {
            ratio: Some(0.5),
            label: None,
            theme: &t,
            tick: 0,
        }
        .render(area, &mut buf);
        assert!(buf[(0, 0)].modifier.contains(Modifier::REVERSED));
        assert!(!buf[(3, 0)].modifier.contains(Modifier::REVERSED));
    }

    #[test]
    fn progress_indeterminate_moves_with_tick() {
        let w = 10;
        let at = |t| row(&render(None, None, w, t), w);
        assert_eq!(at(0), "██████    ");
        assert_eq!(at(2), "  ██████  ");
        assert_eq!(at(4), "    ██████");
        assert_eq!(at(6), "  ██████  ");
        assert_eq!(at(8), at(0));
        assert_eq!(row(&render(None, None, 4, 3), 4), "████");
        assert_eq!(at(3).matches('█').count(), 6);
    }
}
