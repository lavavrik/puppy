//! The footer: global stats (line 1) and the hotkeys valid right now (line 2), each
//! hint recorded as a clickable hit region.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::widgets::{Sparkline, Widget};

use super::{grey_out, groups, spans, stale, width};
use crate::app::{Action, AddField, App, HitTarget, Mode, Popup};
use crate::format;
use crate::theme::Token;

/// The global throughput sparkline at the right of the stats line.
const SPARK_MAX: u16 = 24;
/// What a hint click maps to for `Esc` (see [`KeyAction::primary_char`]).
///
/// [`KeyAction::primary_char`]: crate::app::KeyAction::primary_char
const ESC: char = '\u{1b}';

pub(super) fn draw(app: &mut App, buf: &mut Buffer, area: Rect) {
    if area.height < 2 || area.width < 2 {
        return;
    }
    stats(app, buf, Rect { height: 1, ..area });
    let hints = Rect {
        y: area.y + 1,
        height: 1,
        ..area
    };
    hint_line(app, buf, hints);
}

fn stats(app: &App, buf: &mut Buffer, r: Rect) {
    let t = &app.theme;
    let tot = &app.totals;
    let (text, dim, accent) = (
        t.style(Token::Text),
        t.style(Token::Dim),
        t.style(Token::Accent),
    );
    // While the daemon is away the rates are unknown.
    let stale = stale(app);
    let (down, up) = if stale {
        (format::speed(0), format::speed(0))
    } else {
        (format::speed(tot.down_bps), format::speed(tot.up_bps))
    };
    let active = format!("{} active", tot.active);
    let queued = format!("{} queued", tot.queued);
    let done = format!("{} done", tot.completed);
    let paused = format!("{} paused", tot.paused);
    let failed = format!("{} failed", tot.failed);
    let limit = match app
        .settings
        .as_ref()
        .map(|s| (s.global_down_limit, s.global_up_limit))
    {
        Some((None, None)) | None => "limit: none".to_string(),
        Some((Some(d), None)) => format!("limit: ↓ {}", format::speed(d)),
        Some((None, Some(u))) => format!("limit: ↑ {}", format::speed(u)),
        Some((Some(d), Some(u))) => {
            format!("limit: ↓ {} ↑ {}", format::speed(d), format::speed(u))
        }
    };
    let max = app
        .settings
        .as_ref()
        .map(|s| format!("max {}", s.max_active));

    // Whole parts drop from the right when the line is short.
    let mut items: Vec<Vec<(&str, Style)>> = vec![
        vec![
            ("↓ ", accent),
            (&down, text),
            ("  ", Style::new()),
            ("↑ ", accent),
            (&up, text),
        ],
        vec![(&active, text)],
        vec![(&queued, text)],
    ];
    if tot.paused > 0 {
        items.push(vec![(&paused, text)]);
    }
    items.push(vec![(&done, text)]);
    if tot.failed > 0 {
        items.push(vec![(&failed, t.style(Token::Err))]);
    }
    items.push(vec![(&limit, dim)]);
    if let Some(m) = &max {
        items.push(vec![(m, dim)]);
    }
    let used = 1 + groups(
        buf,
        r.x + 1,
        r.y,
        r.width.saturating_sub(2),
        &items,
        (" · ", dim),
    );

    // Global throughput sparkline on the right when there is room for it.
    let global = app.history.global();
    let room = r.width.saturating_sub(used + 3);
    let sw = room.min(SPARK_MAX);
    if sw >= 8 && !global.is_empty() {
        let n = global.len();
        let shown = usize::from(sw - 1).min(n);
        let samples = global.iter().skip(n - shown).copied();
        let peak = samples.clone().max().unwrap_or(0).max(1);
        let area = Rect::new(r.right() - sw, r.y, sw - 1, 1);
        Sparkline::default()
            .data(samples)
            .max(peak)
            .style(accent)
            .render(area, buf);
    }
    if stale {
        grey_out(t, buf, r);
    }
}

/// A hint: the key as shown, what it does, what a click on it means.
struct Hint {
    key: &'static str,
    label: &'static str,
    click: Option<char>,
}

const fn hint(key: &'static str, label: &'static str, click: Option<char>) -> Hint {
    Hint { key, label, click }
}

fn hint_line(app: &mut App, buf: &mut Buffer, r: Rect) {
    let t = app.theme;
    let mut x = r.x + 1;
    let right = r.right().saturating_sub(1);
    let hints: Vec<Hint> = match &app.mode {
        Mode::Normal => normal_hints(app),
        Mode::Filter => {
            x += spans(
                buf,
                x,
                r.y,
                right.saturating_sub(x),
                &[("type to filter", t.style(Token::KeyLabel))],
            );
            x += spans(
                buf,
                x,
                r.y,
                right.saturating_sub(x),
                &[(" · ", t.style(Token::Dim))],
            );
            vec![hint("Enter", "keep", None), hint("Esc", "clear", Some(ESC))]
        }
        Mode::Popup(p) => popup_hints(p),
    };
    let filter_mode = app.mode == Mode::Filter;
    for (i, h) in hints.iter().enumerate() {
        let sep = if i == 0 {
            ""
        } else if filter_mode {
            " · "
        } else {
            "  "
        };
        let w = width(sep) + width(h.key) + 1 + width(h.label);
        if x + w > right + 1 {
            break;
        }
        let parts = [
            (sep, t.style(Token::Dim)),
            (h.key, t.style(Token::Key)),
            (" ", Style::new()),
            (h.label, t.style(Token::KeyLabel)),
        ];
        spans(buf, x, r.y, w, &parts);
        if let Some(c) = h.click {
            let hx = x + width(sep);
            app.hits.push((
                Rect::new(hx, r.y, w - width(sep), 1),
                HitTarget::FooterKey(c),
            ));
        }
        x += w;
    }
}

/// `a add  p pause  r resume  d delete  R restart  l limit  / filter  ? help  q quit`,
/// keeping only what is valid for the (visible) selection.
fn normal_hints(app: &App) -> Vec<Hint> {
    let sel = app
        .selected
        .and_then(|id| app.visible_iter().find(|d| d.id == id));
    let can = |a: Action| sel.is_some_and(|d| app.can(a, d));
    let mut v = vec![hint("a", "add", Some('a'))];
    for (action, key, label, c) in [
        (Action::Pause, "p", "pause", 'p'),
        (Action::Resume, "r", "resume", 'r'),
        (Action::Delete, "d", "delete", 'd'),
        (Action::Restart, "R", "restart", 'R'),
        (Action::Limit, "l", "limit", 'l'),
    ] {
        if can(action) {
            v.push(hint(key, label, Some(c)));
        }
    }
    if !app.filter.is_empty() {
        v.push(hint("Esc", "clear filter", Some(ESC)));
    }
    v.extend([
        hint("/", "filter", Some('/')),
        hint("?", "help", Some('?')),
        hint("q", "quit", Some('q')),
    ]);
    v
}

/// What the open popup's keys do.
fn popup_hints(p: &Popup) -> Vec<Hint> {
    let cancel = hint("Esc", "cancel", Some(ESC));
    match p {
        Popup::Add(f) => {
            let mut v = vec![hint("Tab", "next", None)];
            if f.focus == AddField::Paused {
                v.push(hint("Space", "toggle", None));
            }
            v.extend([hint("Enter", "add", None), cancel]);
            v
        }
        Popup::Limit(_) => vec![hint("Enter", "set", None), cancel],
        Popup::GlobalLimits(_) => vec![
            hint("Tab", "next", None),
            hint("Enter", "set", None),
            cancel,
        ],
        Popup::ConfirmDelete { .. } => vec![
            hint("f", "files", Some('f')),
            hint("Enter", "delete", None),
            cancel,
        ],
        Popup::Help => vec![hint("j/k", "scroll", None), hint("Esc", "close", Some(ESC))],
    }
}
