//! The detail pane: everything about the selected download, its speed history and its
//! segment strip (HTTP) or piece map (torrent).

use puppy_core::types::{EngineDetail, SegmentView};
use puppy_proto::{DownloadSnapshot, EngineKindTag, WireState};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::text::Span;
use ratatui::widgets::{Sparkline, Widget};

use super::list::size_text;
use super::{centered, panel, spans, state_icon, state_word, text, width};
use crate::app::App;
use crate::format;
use crate::theme::{Theme, Token};
use crate::widgets::{ConnectionBars, PieceMap, SegmentStrip};

/// Speed samples shown in the sparkline.
const SPARK_SAMPLES: usize = 60;
/// Key/value labels: `Downloaded` plus a space.
const LABEL_W: u16 = 11;
/// From this inner width on, the key/value grid uses two columns.
const TWO_COLUMNS: u16 = 60;
/// Rows the piece map may use at most.
const PIECE_ROWS: u16 = 8;

pub(super) fn draw(app: &App, buf: &mut Buffer, area: Rect) {
    let t = &app.theme;
    let block = panel(
        t,
        vec![Span::styled(" Details ", t.style(Token::Title))],
        false,
    );
    let inner = block.inner(area);
    block.render(area, buf);
    if inner.width < 3 || inner.height == 0 {
        return;
    }
    // One column of padding on both sides.
    let body = Rect::new(inner.x + 1, inner.y, inner.width - 2, inner.height);
    let selection = app
        .selected
        .and_then(|id| app.visible_iter().find(|d| d.id == id));
    let Some(d) = selection else {
        let y = body.y + body.height.saturating_sub(1) / 2;
        centered(buf, body, y, &[("Nothing selected", t.style(Token::Dim))]);
        return;
    };
    let mut c = Cursor {
        area: body,
        y: body.y,
    };
    header(t, buf, &mut c, d);
    c.gap();
    fields(t, buf, &mut c, d);
    c.gap();
    speed(app, buf, &mut c, d);
    c.gap();
    let detail = app
        .detail
        .as_ref()
        .filter(|(id, _)| *id == d.id)
        .map(|(_, e)| e);
    engine(t, buf, &mut c, d, detail);
}

/// Hands out rows top-down until the pane is full.
struct Cursor {
    area: Rect,
    y: u16,
}

impl Cursor {
    fn left(&self) -> u16 {
        self.area.bottom().saturating_sub(self.y)
    }

    /// The next `h` rows, or `None` when they do not fit.
    fn take(&mut self, h: u16) -> Option<Rect> {
        if h == 0 || self.left() < h {
            return None;
        }
        let r = Rect::new(self.area.x, self.y, self.area.width, h);
        self.y += h;
        Some(r)
    }

    fn line(&mut self) -> Option<Rect> {
        self.take(1)
    }

    /// A blank separator row (skipped at the very bottom).
    fn gap(&mut self) {
        if self.left() > 1 {
            self.y += 1;
        }
    }
}

fn header(t: &Theme, buf: &mut Buffer, c: &mut Cursor, d: &DownloadSnapshot) {
    if let Some(r) = c.line() {
        let style = t.style(Token::Text).add_modifier(Modifier::BOLD);
        text(buf, r.x, r.y, r.width, &d.name, style);
    }
    if let Some(r) = c.line() {
        let where_ = match &d.path {
            Some(p) => p.display().to_string(),
            None => d.source.clone(),
        };
        text(buf, r.x, r.y, r.width, &where_, t.style(Token::Dim));
    }
}

fn fields(t: &Theme, buf: &mut Buffer, c: &mut Cursor, d: &DownloadSnapshot) {
    let size = d
        .total_bytes
        .map_or_else(|| "unknown".to_string(), format::bytes);
    let downloaded = match format::percent(d.done_bytes, d.total_bytes) {
        Some(r) if !matches!(d.state, WireState::Completed) => {
            format!(
                "{} ({}%)",
                format::bytes(d.done_bytes),
                (r * 100.0).floor() as u32
            )
        }
        _ => size_text(d),
    };
    let speed = if d.kind == EngineKindTag::Torrent {
        format!(
            "↓ {}  ↑ {}",
            format::speed(d.down_bps),
            format::speed(d.up_bps)
        )
    } else {
        format::speed(d.down_bps)
    };
    let (icon, icon_tok) = state_icon(&d.state);
    let state = format!("{icon} {}", state_word(&d.state));
    let limit = d.limit.map_or_else(|| "none".to_string(), format::speed);
    let pairs: [(&str, &str, Token); 6] = [
        ("Size", &size, Token::Text),
        ("Downloaded", &downloaded, Token::Text),
        ("Speed", &speed, Token::Text),
        ("ETA", &format::eta(d.eta_secs), Token::Text),
        ("State", &state, icon_tok),
        ("Limit", &limit, Token::Text),
    ];
    let w = c.area.width;
    if w >= TWO_COLUMNS {
        let col = w / 2;
        for pair in pairs.chunks(2) {
            let Some(r) = c.line() else { return };
            for (k, (key, value, tok)) in pair.iter().enumerate() {
                let x = r.x + k as u16 * col;
                field(t, buf, x, r.y, col - 1, key, value, *tok);
            }
        }
    } else {
        for (key, value, tok) in pairs {
            let Some(r) = c.line() else { return };
            field(t, buf, r.x, r.y, r.width, key, value, tok);
        }
    }
    if let Some(r) = c.line() {
        field(t, buf, r.x, r.y, r.width, "Source", &d.source, Token::Text);
    }
}

#[allow(clippy::too_many_arguments)]
fn field(t: &Theme, buf: &mut Buffer, x: u16, y: u16, w: u16, key: &str, value: &str, tok: Token) {
    text(buf, x, y, w.min(LABEL_W), key, t.style(Token::Dim));
    if w > LABEL_W {
        text(buf, x + LABEL_W, y, w - LABEL_W, value, t.style(tok));
    }
}

/// `Speed` heading with the peak on the right, then a sparkline of recent samples.
fn speed(app: &App, buf: &mut Buffer, c: &mut Cursor, d: &DownloadSnapshot) {
    let t = &app.theme;
    let Some(head) = c.line() else { return };
    let samples = app.history.get(d.id);
    let n = samples.map_or(0, |s| s.len());
    let shown = SPARK_SAMPLES.min(usize::from(head.width)).min(n);
    let recent = samples
        .into_iter()
        .flat_map(|s| s.iter().skip(n - shown))
        .copied();
    let peak = recent.clone().max().unwrap_or(0);
    let peak_text = format!("peak {}", format::speed(peak));
    heading(t, buf, head, "Speed history", &peak_text);
    // Three rows when the engine section below still gets some room.
    let rows = match c.left() {
        l if l >= 10 => 3,
        l if l >= 6 => 2,
        _ => 1,
    };
    let Some(r) = c.take(rows) else { return };
    if shown == 0 {
        text(
            buf,
            r.x,
            r.y,
            r.width,
            "no samples yet",
            t.style(Token::Dim),
        );
        return;
    }
    // Newest sample at the right edge.
    let w = shown as u16;
    Sparkline::default()
        .data(recent)
        .max(peak.max(1))
        .style(t.style(Token::Accent))
        .render(
            Rect {
                x: r.right() - w,
                width: w,
                ..r
            },
            buf,
        );
}

/// A section heading: bold label left, dim note right.
fn heading(t: &Theme, buf: &mut Buffer, r: Rect, label: &str, note: &str) {
    let used = text(
        buf,
        r.x,
        r.y,
        r.width,
        label,
        t.style(Token::Text).add_modifier(Modifier::BOLD),
    );
    let nw = width(note);
    if used + 2 + nw <= r.width {
        text(buf, r.right() - nw, r.y, nw, note, t.style(Token::Dim));
    }
}

fn engine(
    t: &Theme,
    buf: &mut Buffer,
    c: &mut Cursor,
    d: &DownloadSnapshot,
    detail: Option<&EngineDetail>,
) {
    let dim = t.style(Token::Dim);
    match detail {
        None => {
            let fetched = matches!(d.kind, EngineKindTag::Http | EngineKindTag::Torrent)
                && matches!(
                    d.state,
                    WireState::Active | WireState::Starting | WireState::Paused
                );
            if fetched && let Some(r) = c.line() {
                text(buf, r.x, r.y, r.width, "No detail yet", dim);
            }
        }
        Some(EngineDetail::Unknown) => {
            if let Some(r) = c.line() {
                text(buf, r.x, r.y, r.width, "No detail available", dim);
            }
        }
        Some(EngineDetail::Http {
            segments,
            connections,
            resumable,
        }) => http(t, buf, c, d, segments, *connections, *resumable),
        Some(EngineDetail::Torrent {
            piece_count,
            have,
            peers,
            seeds,
            leechers,
            ratio,
        }) => {
            let Some(head) = c.line() else { return };
            let got = (0..*piece_count)
                .filter(|&i| {
                    have.get((i / 8) as usize)
                        .is_some_and(|b| b >> (7 - i % 8) & 1 == 1)
                })
                .count();
            heading(t, buf, head, "Pieces", &format!("{got} / {piece_count}"));
            if let Some(r) = c.line() {
                let peers = format!("{peers} peers");
                let swarm = format!(" ({seeds} seeds, {leechers} leechers)");
                let ratio = format!("ratio {ratio:.2}");
                spans(
                    buf,
                    r.x,
                    r.y,
                    r.width,
                    &[
                        (&peers, t.style(Token::Text)),
                        (&swarm, dim),
                        (" · ", dim),
                        (&ratio, t.style(Token::Text)),
                    ],
                );
            }
            let rows = c.left().min(PIECE_ROWS);
            if let Some(r) = c.take(rows) {
                PieceMap {
                    piece_count: *piece_count,
                    have,
                    theme: t,
                }
                .render(r, buf);
            }
        }
    }
}

fn http(
    t: &Theme,
    buf: &mut Buffer,
    c: &mut Cursor,
    d: &DownloadSnapshot,
    segments: &[SegmentView],
    connections: u16,
    resumable: bool,
) {
    let Some(head) = c.line() else { return };
    let plural = if connections == 1 { "" } else { "s" };
    let resume = if resumable {
        "resumable"
    } else {
        "not resumable"
    };
    let note = format!("{connections} connection{plural} · {resume}");
    heading(t, buf, head, "Segments", &note);
    let total = d
        .total_bytes
        .unwrap_or_else(|| segments.iter().map(|s| s.end).max().unwrap_or(0));
    if let Some(r) = c.line() {
        SegmentStrip {
            total,
            segments,
            theme: t,
        }
        .render(r, buf);
    }
    c.gap();
    let rows = c.left();
    if let Some(r) = c.take(rows) {
        ConnectionBars {
            segments,
            theme: t,
            max_rows: usize::from(rows),
        }
        .render(r, buf);
    }
}
