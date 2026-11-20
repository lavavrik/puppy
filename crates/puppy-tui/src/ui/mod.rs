//! Drawing the dashboard: [`draw`] lays out the title bar, the download list, the detail
//! pane and the footer (wide, narrow or too-small), and records mouse hit regions.
//!
//! Everything here is pure: it reads the [`App`] and writes into the frame's buffer.

mod detail;
mod footer;
mod list;

use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::layout::{Position, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType};
use unicode_width::UnicodeWidthStr;

use puppy_proto::WireState;

use crate::app::{App, Conn, Mode};
use crate::format;
use crate::theme::{Theme, Token};

/// Below this the dashboard shows only [`TOO_SMALL`].
pub const MIN_SIZE: (u16, u16) = (60, 16);
/// From this width on, the detail pane sits right of the list.
pub const WIDE_WIDTH: u16 = 110;
pub const TOO_SMALL: &str = "puppy needs a bigger terminal (60×16)";

/// Share of the body width the list gets in the wide layout.
const LIST_PERCENT: u16 = 58;
/// Narrow layout: the list keeps at least this many rows; the detail pane needs at
/// least `DETAIL_MIN` rows (else it is omitted) and gets at most `DETAIL_MAX`.
const LIST_MIN: u16 = 8;
const DETAIL_MIN: u16 = 7;
const DETAIL_MAX: u16 = 18;
const FOOTER_ROWS: u16 = 2;

/// Draws one frame. Takes `&mut` only to refill [`App::hits`] and to adjust
/// [`App::list_offset`] so the selection stays visible.
pub fn draw(app: &mut App, f: &mut Frame) {
    app.hits.clear();
    let area = f.area();
    let cursor = draw_into(app, f.buffer_mut(), area);
    if let Some(pos) = cursor {
        f.set_cursor_position(pos);
    }
}

/// The screen regions for a terminal of `area`.
struct Layout {
    title: Rect,
    list: Rect,
    detail: Option<Rect>,
    footer: Rect,
}

fn layout(area: Rect) -> Layout {
    let title = Rect::new(area.x, area.y, area.width, 1);
    let footer = Rect::new(area.x, area.bottom() - FOOTER_ROWS, area.width, FOOTER_ROWS);
    let body = Rect::new(
        area.x,
        area.y + 1,
        area.width,
        area.height - 1 - FOOTER_ROWS,
    );
    if area.width >= WIDE_WIDTH {
        let lw = body.width * LIST_PERCENT / 100;
        return Layout {
            title,
            list: Rect { width: lw, ..body },
            detail: Some(Rect {
                x: body.x + lw,
                width: body.width - lw,
                ..body
            }),
            footer,
        };
    }
    let mut dh = (body.height * 45 / 100).clamp(DETAIL_MIN, DETAIL_MAX);
    if body.height.saturating_sub(dh) < LIST_MIN {
        dh = body.height.saturating_sub(LIST_MIN);
    }
    if dh < DETAIL_MIN {
        return Layout {
            title,
            list: body,
            detail: None,
            footer,
        };
    }
    let lh = body.height - dh;
    Layout {
        title,
        list: Rect { height: lh, ..body },
        detail: Some(Rect {
            y: body.y + lh,
            height: dh,
            ..body
        }),
        footer,
    }
}

/// Draws everything; returns where the terminal cursor goes (filter input), if shown.
fn draw_into(app: &mut App, buf: &mut Buffer, area: Rect) -> Option<Position> {
    let (min_w, min_h) = MIN_SIZE;
    if area.width < min_w || area.height < min_h {
        too_small(&app.theme, buf, area);
        return None;
    }
    let l = layout(area);
    list::draw(app, buf, l.list);
    if let Some(d) = l.detail {
        detail::draw(app, buf, d);
    }
    footer::draw(app, buf, l.footer);
    // Popups and toasts draw last, over everything (Task 5).
    title_bar(app, buf, l.title)
}

fn too_small(theme: &Theme, buf: &mut Buffer, area: Rect) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let msg = format::truncate(TOO_SMALL, usize::from(area.width));
    let w = msg.width() as u16;
    let x = area.x + (area.width - w) / 2;
    let y = area.y + area.height / 2;
    text(buf, x, y, w, &msg, theme.style(Token::Warn));
}

/// ` puppy ` badge and the filter on the left; daemon version and connection state on
/// the right.
fn title_bar(app: &App, buf: &mut Buffer, area: Rect) -> Option<Position> {
    let t = &app.theme;
    let (badge, badge_tok) = match &app.conn {
        Conn::Connected => ("● connected".to_string(), Token::Ok),
        Conn::Connecting => ("◌ connecting…".to_string(), Token::Info),
        Conn::Reconnecting { attempt } => (format!("◌ reconnecting ({attempt})"), Token::Warn),
    };
    let version = app
        .daemon_version
        .as_deref()
        .map(|v| format!("daemon v{}", format::display_safe(v)));
    let mut right: Vec<(&str, Style)> = Vec::new();
    if let Some(v) = &version {
        right.push((v, t.style(Token::Dim)));
        right.push(("   ", Style::new()));
    }
    right.push((&badge, t.style(badge_tok)));
    right.push((" ", Style::new()));
    let rw: u16 = right.iter().map(|(s, _)| s.width() as u16).sum();

    let mut x = area.x;
    x += text(buf, x, area.y, area.width, " puppy ", t.style(Token::Badge));
    // Right side wins over the filter when space is short.
    let right_x = area.right().saturating_sub(rw).max(x);
    spans(buf, right_x, area.y, area.right() - right_x, &right);

    let filtering = app.mode == Mode::Filter;
    if !filtering && app.filter.is_empty() {
        return None;
    }
    x += 2;
    let room = right_x.saturating_sub(x + 2);
    let label = [("/", t.style(Token::Key)), (" ", Style::new())];
    x += spans(buf, x, area.y, room, &label);
    let room = right_x.saturating_sub(x + 2);
    let value = format::display_safe(&app.filter.value);
    let shown = text(buf, x, area.y, room, &value, t.style(Token::Text));
    if !filtering {
        return None;
    }
    // Cursor after the graphemes before it, clamped to what is shown.
    use unicode_segmentation::UnicodeSegmentation;
    let before: usize = value
        .graphemes(true)
        .take(app.filter.cursor)
        .map(UnicodeWidthStr::width)
        .sum();
    let before = u16::try_from(before).unwrap_or(u16::MAX);
    // A cut value ends in `…`: the cursor goes past it, never onto it.
    let cut = width(&value) > shown;
    let cx = if cut && before >= shown.saturating_sub(1) {
        x + shown
    } else {
        x + before.min(shown)
    };
    (cx < area.right()).then_some(Position::new(cx, area.y))
}

/// A rounded panel; the focused one gets the accent border.
pub(crate) fn panel<'a>(theme: &Theme, title: Vec<Span<'a>>, focused: bool) -> Block<'a> {
    let border = if focused {
        Token::BorderFocus
    } else {
        Token::Border
    };
    Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(theme.style(border))
        .title(Line::from(title))
}

/// Writes `s` (sanitized, truncated with `…` to `max_w` columns) at (`x`, `y`); returns
/// the columns used. Positions outside the buffer draw nothing.
pub(crate) fn text(buf: &mut Buffer, x: u16, y: u16, max_w: u16, s: &str, style: Style) -> u16 {
    let a = buf.area;
    if y < a.y || y >= a.bottom() || x < a.x || x >= a.right() || max_w == 0 {
        return 0;
    }
    let max_w = max_w.min(a.right() - x);
    let safe = format::display_safe(s);
    let cut = format::truncate(&safe, usize::from(max_w));
    let (end, _) = buf.set_stringn(x, y, &cut, usize::from(max_w), style);
    end - x
}

/// Writes spans left to right within `max_w` columns; the first that does not fit is
/// truncated with `…` and the rest dropped. Returns the columns used.
pub(crate) fn spans(buf: &mut Buffer, x: u16, y: u16, max_w: u16, parts: &[(&str, Style)]) -> u16 {
    let mut used = 0u16;
    for (s, style) in parts {
        let room = max_w.saturating_sub(used);
        if room == 0 {
            break;
        }
        let w = text(buf, x + used, y, room, s, *style);
        used += w;
        if width(s) > w {
            break;
        }
    }
    used
}

/// Writes groups of spans separated by `sep`, by priority: the first group always
/// shows (truncated if need be); later groups that do not fit whole are dropped, with
/// everything after them. Returns the columns used.
pub(crate) fn groups(
    buf: &mut Buffer,
    x: u16,
    y: u16,
    max_w: u16,
    items: &[Vec<(&str, Style)>],
    sep: (&str, Style),
) -> u16 {
    let mut used = 0u16;
    for (i, item) in items.iter().enumerate() {
        let w: u16 = item
            .iter()
            .map(|(s, _)| width(s))
            .fold(0, u16::saturating_add);
        if i == 0 {
            used = spans(buf, x, y, max_w, item);
            if used < w {
                break;
            }
            continue;
        }
        if used.saturating_add(width(sep.0)).saturating_add(w) > max_w {
            break;
        }
        used += spans(buf, x + used, y, max_w - used, &[sep]);
        used += spans(buf, x + used, y, max_w - used, item);
    }
    used
}

/// Display width of `s` as drawn (sanitized), saturating at `u16::MAX`.
pub(crate) fn width(s: &str) -> u16 {
    u16::try_from(format::display_safe(s).width()).unwrap_or(u16::MAX)
}

/// Writes `s` centered horizontally in `area` on row `y`.
pub(crate) fn centered(buf: &mut Buffer, area: Rect, y: u16, parts: &[(&str, Style)]) {
    let w: u16 = parts
        .iter()
        .map(|(s, _)| width(s))
        .fold(0, u16::saturating_add);
    let x = area.x + area.width.saturating_sub(w) / 2;
    spans(buf, x, y, area.right().saturating_sub(x), parts);
}

/// The list icon for a state and how it is coloured.
pub(crate) fn state_icon(state: &WireState) -> (&'static str, Token) {
    match state {
        WireState::Active => ("▶", Token::Ok),
        WireState::Paused => ("⏸", Token::Dim),
        WireState::Queued => ("…", Token::Dim),
        WireState::Completed => ("✓", Token::Ok),
        WireState::Failed {
            transient: true, ..
        } => ("↻", Token::Warn),
        WireState::Failed { .. } => ("✗", Token::Err),
        WireState::NeedsAttention { .. } => ("!", Token::Warn),
        WireState::Starting => ("◌", Token::Info),
        WireState::Unknown => ("?", Token::Dim),
    }
}

/// One word for a state (detail pane).
pub(crate) fn state_word(state: &WireState) -> &'static str {
    match state {
        WireState::Active => "active",
        WireState::Paused => "paused",
        WireState::Queued => "queued",
        WireState::Completed => "completed",
        WireState::Failed {
            transient: true, ..
        } => "failed, retrying",
        WireState::Failed { .. } => "failed",
        WireState::NeedsAttention { .. } => "needs attention",
        WireState::Starting => "starting",
        WireState::Unknown => "unknown",
    }
}
