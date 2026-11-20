//! The download list: two lines per download, scrolled to keep the selection visible.

use puppy_proto::{DownloadSnapshot, EngineKindTag, WireAttention, WireState};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::Span;
use ratatui::widgets::Widget;

use super::{centered, panel, spans, state_icon, text, width};
use crate::app::{App, Conn, HitTarget};
use crate::format;
use crate::theme::{Theme, Token};
use crate::widgets::ProgressBar;

/// Lines per download.
const ROW_LINES: u16 = 2;
/// Progress bar: this share of the row, at least `BAR_MIN` columns.
const BAR_PERCENT: u16 = 30;
const BAR_MIN: u16 = 10;
/// Columns the name keeps before the bar shrinks.
const NAME_MIN: u16 = 12;

pub(super) fn draw(app: &mut App, buf: &mut Buffer, area: Rect) {
    let t = app.theme;
    // One pass, no allocation per download: count, find the selection.
    let mut count = 0usize;
    let mut sel_index = None;
    for d in app.visible_iter() {
        if Some(d.id) == app.selected {
            sel_index = Some(count);
        }
        count += 1;
    }
    let total = app.downloads.len();

    let counter = if count == total {
        format!(" {total} ")
    } else {
        format!(" {count}/{total} ")
    };
    let mut block = panel(
        &t,
        vec![
            Span::styled(" Downloads", t.style(Token::Title)),
            Span::styled(counter, t.style(Token::Dim)),
        ],
        true,
    );
    let inner = block.inner(area);
    let cap = usize::from(inner.height / ROW_LINES);
    if count > cap
        && cap > 0
        && let Some(i) = sel_index
    {
        block = block.title_bottom(
            Span::styled(format!(" {} of {count} ", i + 1), t.style(Token::Dim))
                .into_right_aligned_line(),
        );
    }
    block.render(area, buf);
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    if count == 0 {
        empty(app, buf, inner);
        return;
    }
    if cap == 0 {
        return;
    }

    // Scroll so the selection is visible and no space is wasted below the last row.
    let mut offset = app.list_offset.min(count.saturating_sub(cap));
    if let Some(i) = sel_index {
        if i < offset {
            offset = i;
        } else if i >= offset + cap {
            offset = i + 1 - cap;
        }
    }
    app.list_offset = offset;

    let id_w = app
        .downloads
        .iter()
        .map(|d| d.id)
        .max()
        .unwrap_or(0)
        .to_string()
        .len() as u16;
    let ctx = RowCtx {
        theme: &t,
        id_w,
        now_unix: app.now_unix,
        ticks: app.ticks,
    };
    let mut hits = Vec::with_capacity(cap);
    for (k, d) in app.visible_iter().skip(offset).take(cap).enumerate() {
        let rect = Rect::new(
            inner.x,
            inner.y + k as u16 * ROW_LINES,
            inner.width,
            ROW_LINES,
        );
        ctx.row(buf, rect, d, Some(offset + k) == sel_index);
        hits.push((rect, HitTarget::ListRow(d.id)));
    }
    app.hits.extend(hits);

    if count > cap {
        scrollbar(&t, buf, area, inner, offset, cap, count);
    }
    if matches!(app.conn, Conn::Reconnecting { .. }) {
        grey_out(&t, buf, inner);
    }
}

/// Centered hint when there is nothing to list.
fn empty(app: &App, buf: &mut Buffer, inner: Rect) {
    let t = &app.theme;
    let y = inner.y + inner.height.saturating_sub(1) / 2;
    let dim = t.style(Token::Dim);
    if !app.downloads.is_empty() {
        let needle = format::truncate(
            &format::display_safe(app.filter.value.trim()),
            usize::from(inner.width / 2),
        );
        let quoted = format!("“{needle}”");
        centered(
            buf,
            inner,
            y,
            &[
                ("No matches for ", dim),
                (&quoted, t.style(Token::Text)),
                (" — ", dim),
                ("Esc", t.style(Token::Key)),
                (" clears", dim),
            ],
        );
        return;
    }
    match app.conn {
        Conn::Connected => centered(
            buf,
            inner,
            y,
            &[
                ("No downloads yet — press ", dim),
                ("a", t.style(Token::Key)),
                (" to add one", dim),
            ],
        ),
        Conn::Connecting => centered(buf, inner, y, &[("Connecting to the daemon…", dim)]),
        Conn::Reconnecting { .. } => {
            centered(buf, inner, y, &[("Waiting for the daemon…", dim)]);
        }
    }
}

/// Thumb over the list's right border.
fn scrollbar(
    t: &Theme,
    buf: &mut Buffer,
    area: Rect,
    inner: Rect,
    offset: usize,
    cap: usize,
    count: usize,
) {
    let track = usize::from(inner.height);
    if track == 0 || area.width == 0 {
        return;
    }
    let thumb = (track * cap / count).clamp(1, track);
    let span = count - cap;
    let pos = (track - thumb) * offset / span.max(1);
    let x = area.right() - 1;
    for k in pos..pos + thumb {
        if let Some(cell) = buf.cell_mut((x, inner.y + k as u16)) {
            cell.set_symbol("┃").set_style(t.style(Token::BorderFocus));
        }
    }
}

/// The list while the daemon is away: everything in the dim colour.
fn grey_out(t: &Theme, buf: &mut Buffer, inner: Rect) {
    for y in inner.y..inner.bottom() {
        for x in inner.x..inner.right() {
            if let Some(cell) = buf.cell_mut((x, y)) {
                if t.mono {
                    cell.modifier.insert(Modifier::DIM);
                } else {
                    cell.fg = t.p.dim;
                }
            }
        }
    }
}

struct RowCtx<'a> {
    theme: &'a Theme,
    /// Columns for the right-aligned id.
    id_w: u16,
    now_unix: u64,
    ticks: u64,
}

impl RowCtx<'_> {
    fn style(&self, token: Token, selected: bool) -> Style {
        if selected {
            self.theme.on_selected(token)
        } else {
            self.theme.style(token)
        }
    }

    fn row(&self, buf: &mut Buffer, rect: Rect, d: &DownloadSnapshot, selected: bool) {
        let t = self.theme;
        if selected {
            buf.set_style(rect, t.style(Token::Selected));
        }
        let st = |tok| self.style(tok, selected);
        let (y1, y2) = (rect.y, rect.y + 1);
        let right = rect.right();

        // Line 1: icon, id, name, [T] … progress bar.
        let (icon, icon_tok) = state_icon(&d.state);
        let mut x = rect.x + 1;
        text(buf, x, y1, 1, icon, st(icon_tok));
        x += 2;
        let id = format!("{:>w$}", d.id, w = usize::from(self.id_w));
        x += text(buf, x, y1, right.saturating_sub(x), &id, st(Token::Dim)) + 1;
        let name_x = x;

        let w = rect.width;
        let mut bar_w = (w * BAR_PERCENT / 100).max(BAR_MIN);
        let fixed = name_x - rect.x + NAME_MIN + 3;
        if bar_w + fixed > w {
            bar_w = w.saturating_sub(fixed);
        }
        let name_end = if bar_w >= 6 {
            let bar_x = right - 1 - bar_w;
            self.bar(buf, Rect::new(bar_x, y1, bar_w, 1), d, selected);
            bar_x - 2
        } else {
            right - 1
        };
        let badge = if d.kind == EngineKindTag::Torrent {
            " [T]"
        } else {
            ""
        };
        let name_room = name_end.saturating_sub(name_x + width(badge));
        let name_style = if selected {
            st(Token::Text).add_modifier(Modifier::BOLD)
        } else {
            st(Token::Text)
        };
        let used = text(buf, name_x, y1, name_room, &d.name, name_style);
        spans(
            buf,
            name_x + used,
            y1,
            name_end.saturating_sub(name_x + used),
            &[(badge, st(Token::Info))],
        );

        // Line 2: what is going on, dim, under the name.
        let parts = self.second_line(d);
        let parts: Vec<(&str, Style)> = parts
            .iter()
            .map(|(s, tok)| (s.as_str(), st(*tok)))
            .collect();
        spans(buf, name_x, y2, right.saturating_sub(name_x + 1), &parts);
    }

    fn bar(&self, buf: &mut Buffer, area: Rect, d: &DownloadSnapshot, selected: bool) {
        let ratio = match d.state {
            WireState::Completed => Some(1.0),
            _ => format::percent(d.done_bytes, d.total_bytes),
        };
        let label = match ratio {
            // Floor: never "100%" before the end.
            Some(r) => format!("{}%", (r * 100.0).floor() as u32),
            None => format::bytes(d.done_bytes),
        };
        let moving = matches!(d.state, WireState::Active | WireState::Starting);
        let theme = if selected {
            self.theme.for_selection()
        } else {
            *self.theme
        };
        ProgressBar {
            ratio: if moving { ratio } else { ratio.or(Some(0.0)) },
            label: Some(&label),
            theme: &theme,
            tick: self.ticks,
        }
        .render(area, buf);
    }

    fn second_line(&self, d: &DownloadSnapshot) -> Vec<(String, Token)> {
        const SEP: (&str, Token) = (" · ", Token::Dim);
        let size = size_text(d);
        let mut parts: Vec<(String, Token)> = Vec::new();
        let mut push = |s: String, tok: Token| {
            if !parts.is_empty() {
                parts.push((SEP.0.to_string(), SEP.1));
            }
            parts.push((s, tok));
        };
        match &d.state {
            WireState::Active => {
                push(format::speed(d.down_bps), Token::Dim);
                push(format::eta(d.eta_secs), Token::Dim);
                push(size, Token::Dim);
                if d.kind == EngineKindTag::Torrent {
                    push(format!("↑ {}", format::speed(d.up_bps)), Token::Dim);
                }
            }
            WireState::Starting => {
                push("starting…".into(), Token::Dim);
                push(size, Token::Dim);
            }
            WireState::Queued => {
                match d.queue_position {
                    Some(p) => push(format!("queued #{}", p + 1), Token::Dim),
                    None => push("queued".into(), Token::Dim),
                }
                push(size, Token::Dim);
            }
            WireState::Paused => {
                push("paused".into(), Token::Dim);
                push(size, Token::Dim);
            }
            WireState::Completed => {
                push("completed".into(), Token::Dim);
                push(size, Token::Dim);
            }
            WireState::Failed {
                reason,
                transient: true,
            } => {
                let retry = match d.next_retry_unix {
                    Some(at) => format!(
                        "retry in {}",
                        format::eta(Some(at.saturating_sub(self.now_unix)))
                    ),
                    None => "retrying".into(),
                };
                push(retry, Token::Warn);
                push(reason.clone(), Token::Dim);
            }
            WireState::Failed { reason, .. } => push(reason.clone(), Token::Err),
            WireState::NeedsAttention { reason } => {
                push(attention_text(reason), Token::Warn);
                push("R restarts".into(), Token::Dim);
            }
            WireState::Unknown => push("unknown state (newer daemon?)".into(), Token::Dim),
        }
        if d.kind == EngineKindTag::Torrent
            && matches!(d.state, WireState::Active | WireState::Starting)
            && let Some(p) = d.peers
        {
            push(format!("{p} peers"), Token::Dim);
        }
        if let Some(l) = d.limit
            && !matches!(d.state, WireState::Completed)
        {
            push(format!("limit {}", format::speed(l)), Token::Dim);
        }
        parts
    }
}

/// `460.8 MiB / 1.0 GiB`; just the total once complete; just the bytes when the total
/// is unknown.
pub(super) fn size_text(d: &DownloadSnapshot) -> String {
    match (d.total_bytes, &d.state) {
        (Some(total), WireState::Completed) => format::bytes(total),
        (Some(total), _) => format!(
            "{} / {}",
            format::bytes(d.done_bytes.min(total)),
            format::bytes(total)
        ),
        (None, _) => format::bytes(d.done_bytes),
    }
}

pub(super) fn attention_text(reason: &WireAttention) -> String {
    match reason {
        WireAttention::RemoteChanged => "remote file changed".into(),
        WireAttention::NotResumable => "server can't resume".into(),
        WireAttention::ValidationFailed { detail } => format!("validation failed: {detail}"),
        WireAttention::Unknown => "needs attention".into(),
    }
}
