//! Toasts: short notices stacked at the bottom right, above the footer, newest at the
//! bottom.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::widgets::{Block, BorderType, Clear, Widget};

use super::{spans, width, wrap};
use crate::app::{App, ToastKind};
use crate::theme::Token;

/// A toast is at most this wide, borders included.
pub const TOAST_MAX_WIDTH: u16 = 50;
/// Lines of text per toast.
const TOAST_LINES: usize = 2;
/// Narrower regions get no toasts.
const TOAST_MIN_WIDTH: u16 = 20;

/// Draws the toasts in `region` (the body: between the title bar and the footer). Toasts
/// that do not fit, or would overlap `avoid` (an open popup), are skipped, oldest first.
pub(super) fn draw(app: &App, buf: &mut Buffer, region: Rect, avoid: Option<Rect>) {
    if region.width < TOAST_MIN_WIDTH + 4 || region.height < 5 {
        return;
    }
    let t = &app.theme;
    let max_w = TOAST_MAX_WIDTH.min(region.width - 4);
    // Text room: borders and one column of padding each side.
    let room = usize::from(max_w - 4);
    // Inside the panels' right and bottom borders, which stay visible.
    let mut bottom = region.bottom() - 1;
    for toast in app.toasts.iter().rev() {
        let (mark, token) = match toast.kind {
            ToastKind::Info => ("", Token::Info),
            ToastKind::Error => ("✗ ", Token::Err),
        };
        let mark_w = usize::from(width(mark));
        let lines = wrap(&toast.text, room - mark_w, TOAST_LINES);
        let text_w = lines.iter().map(|l| width(l)).max().unwrap_or(0) + width(mark);
        let w = text_w + 4;
        let h = lines.len() as u16 + 2;
        if bottom < region.y + 1 + h {
            break;
        }
        let rect = Rect::new(region.right() - 2 - w, bottom - h, w, h);
        if avoid.is_some_and(|p| p.intersects(rect)) {
            break;
        }
        let style = t.style(token);
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(style);
        let inner = block.inner(rect);
        Clear.render(rect, buf);
        block.render(rect, buf);
        for (i, line) in lines.iter().enumerate() {
            // Continuation lines align with the text after the mark.
            let lead = match (i, mark.is_empty()) {
                (_, true) => "",
                (0, false) => mark,
                _ => "  ",
            };
            spans(
                buf,
                inner.x + 1,
                inner.y + i as u16,
                inner.width.saturating_sub(2),
                &[(lead, style), (line, style)],
            );
        }
        bottom -= h;
    }
}
