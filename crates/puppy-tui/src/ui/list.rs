//! The download list: two lines per download, scrolled to keep the selection visible.

use puppy_proto::{DownloadSnapshot, EngineKindTag, WireAttention, WireState};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::Span;
use ratatui::widgets::Widget;

use super::{as_shown, centered, grey_out, groups, panel, spans, stale, state_icon, text, width};
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
/// The right-aligned percent slot after the bar (`100%`).
const PCT_W: u16 = 4;

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
    app.hits.push((area, HitTarget::List));
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
    // An odd height leaves one row: the next item's first line peeks in.
    let partial = inner.height % ROW_LINES == 1 && offset + cap < count;
    let shown = cap + usize::from(partial);
    let mut hits = Vec::with_capacity(shown);
    let stale = stale(app);
    for (k, d) in app.visible_iter().skip(offset).take(shown).enumerate() {
        let lines = if k == cap { 1 } else { ROW_LINES };
        let rect = Rect::new(inner.x, inner.y + k as u16 * ROW_LINES, inner.width, lines);
        ctx.row(
            buf,
            rect,
            &as_shown(d, stale),
            Some(offset + k) == sel_index,
        );
        hits.push((rect, HitTarget::ListRow(d.id)));
    }
    app.hits.extend(hits);

    if count > cap {
        scrollbar(&t, buf, area, inner, offset, cap, count);
    }
    if stale {
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
            // Accent edge marking the selection on every line.
            for y in rect.y..rect.bottom() {
                text(buf, rect.x, y, 1, "▌", self.style(Token::BorderFocus, true));
            }
        }
        let st = |tok| self.style(tok, selected);
        let y1 = rect.y;
        let right = rect.right();

        // Line 1: icon, id, name, [T] … progress bar, percent.
        let (icon, icon_tok) = state_icon(&d.state);
        let mut x = rect.x + 2;
        text(buf, x, y1, 1, icon, st(icon_tok));
        x += 2;
        let id = format!("{:>w$}", d.id, w = usize::from(self.id_w));
        x += text(buf, x, y1, right.saturating_sub(x), &id, st(Token::Dim)) + 1;
        let name_x = x;

        // Right end: [bar] [pct] and one column of padding.
        let slot_x = right.saturating_sub(PCT_W + 1);
        let w = rect.width;
        let mut bar_w = (w * BAR_PERCENT / 100).max(BAR_MIN);
        let fixed = (name_x - rect.x) + NAME_MIN + 2 + 1 + PCT_W + 1;
        if bar_w + fixed > w {
            bar_w = w.saturating_sub(fixed);
        }
        let (ratio, pct) = progress(d);
        let pct_tok = if is_moving(&d.state) {
            Token::Text
        } else {
            Token::Dim
        };
        let name_end = if slot_x > name_x + NAME_MIN {
            let pw = width(&pct);
            text(buf, slot_x + PCT_W - pw, y1, pw, &pct, st(pct_tok));
            if bar_w >= 6 {
                let bar_x = slot_x - 1 - bar_w;
                self.bar(buf, Rect::new(bar_x, y1, bar_w, 1), d, ratio, selected);
                bar_x - 2
            } else {
                slot_x - 1
            }
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

        // Line 2: what is going on, dim, under the name; low-priority parts drop first.
        if rect.height < ROW_LINES {
            return;
        }
        let items = self.second_line(d);
        let items: Vec<Vec<(&str, Style)>> = items
            .iter()
            .map(|(s, tok)| vec![(s.as_str(), st(*tok))])
            .collect();
        groups(
            buf,
            name_x,
            y1 + 1,
            right.saturating_sub(name_x + 1),
            &items,
            (" · ", st(Token::Dim)),
        );
    }

    /// The bar, tinted by state; on the selected row its track is the selection.
    fn bar(
        &self,
        buf: &mut Buffer,
        area: Rect,
        d: &DownloadSnapshot,
        ratio: Option<f64>,
        selected: bool,
    ) {
        let mut theme = *self.theme;
        let p = self.theme.p;
        // A muted fill must still stand out from the track (ANSI: both DarkGray).
        let muted = if p.dim == p.bar_todo {
            Theme::contrast(p.dim)
        } else {
            p.dim
        };
        theme.p.bar_done = match d.state {
            WireState::Active | WireState::Starting => p.bar_done,
            WireState::Failed { .. } => p.err,
            WireState::NeedsAttention { .. } => p.warn,
            // Complete, and still sharing.
            WireState::Seeding { .. } => p.ok,
            WireState::Paused
            | WireState::Queued
            | WireState::Completed
            | WireState::ChoosingFiles
            | WireState::Unknown => muted,
        };
        if selected {
            theme.p.bar_todo = p.selection_bg;
        }
        ProgressBar {
            ratio: if is_moving(&d.state) {
                ratio
            } else {
                ratio.or(Some(0.0))
            },
            label: None,
            theme: &theme,
            tick: self.ticks,
        }
        .render(area, buf);
    }

    /// Parts of the second line, most important first.
    fn second_line(&self, d: &DownloadSnapshot) -> Vec<(String, Token)> {
        let size = size_text(d);
        let mut parts: Vec<(String, Token)> = Vec::new();
        let mut push = |s: String, tok: Token| parts.push((s, tok));
        let torrent = d.kind == EngineKindTag::Torrent;
        match &d.state {
            WireState::Active => {
                if torrent {
                    push(
                        format!(
                            "↓ {}  ↑ {}",
                            format::speed(d.down_bps),
                            format::speed(d.up_bps)
                        ),
                        Token::Dim,
                    );
                } else {
                    push(format::speed(d.down_bps), Token::Dim);
                }
                push(format::eta(d.eta_secs), Token::Dim);
                push(size, Token::Dim);
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
            WireState::Seeding {
                ratio,
                seeded_secs,
                ratio_target,
                time_target_secs,
                ..
            } => {
                push(format!("↑ {}", format::speed(d.up_bps)), Token::Dim);
                push(seed_ratio(*ratio, *ratio_target), Token::Dim);
                if let Some(t) = time_target_secs {
                    let left = t.saturating_sub(*seeded_secs);
                    push(format!("{} left", format::eta(Some(left))), Token::Dim);
                }
                push(size, Token::Dim);
            }
            WireState::ChoosingFiles => {
                push("choose files · press c".into(), Token::Warn);
                if let Some(note) = &d.note {
                    push(note.clone(), Token::Dim);
                }
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
        if torrent
            && (is_moving(&d.state) || matches!(d.state, WireState::Seeding { .. }))
            && let Some(p) = d.peers
        {
            push(format!("{p} peers"), Token::Dim);
        }
        if let Some(l) = d.limit
            && !matches!(d.state, WireState::Completed | WireState::Seeding { .. })
        {
            push(format!("limit {}", format::speed(l)), Token::Dim);
        }
        parts
    }
}

fn is_moving(state: &WireState) -> bool {
    matches!(state, WireState::Active | WireState::Starting)
}

/// Fraction done (complete ⇒ 1) and the percent slot text (floored: never `100%`
/// before the end; `—` when the total is unknown).
fn progress(d: &DownloadSnapshot) -> (Option<f64>, String) {
    let ratio = match d.state {
        WireState::Completed | WireState::Seeding { .. } => Some(1.0),
        _ => format::percent(d.done_bytes, d.total_bytes),
    };
    let pct = match ratio {
        Some(r) => format!("{}%", (r * 100.0).floor() as u32),
        None => format::NONE.to_string(),
    };
    (ratio, pct)
}

/// `460.8 MiB / 1.0 GiB`; just the total once complete; just the bytes when the total
/// is unknown.
pub(super) fn size_text(d: &DownloadSnapshot) -> String {
    match (d.total_bytes, &d.state) {
        (Some(total), WireState::Completed | WireState::Seeding { .. }) => format::bytes(total),
        (Some(total), _) => format!(
            "{} / {}",
            format::bytes(d.done_bytes.min(total)),
            format::bytes(total)
        ),
        (None, _) => format::bytes(d.done_bytes),
    }
}

/// `ratio 0.42/1.0`, or just `ratio 0.42` without a ratio target.
pub(super) fn seed_ratio(ratio: f32, target: Option<f32>) -> String {
    match target {
        Some(t) => format!("ratio {ratio:.2}/{}", crate::app::forms::ratio_text(t)),
        None => format!("ratio {ratio:.2}"),
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
