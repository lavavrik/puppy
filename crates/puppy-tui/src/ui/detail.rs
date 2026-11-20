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
use super::{centered, groups, panel, state_icon, state_word, text, width};
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
/// Rows the piece map may use: at least `MAP_MIN`, at most `PIECE_ROWS`.
const MAP_MIN: u16 = 2;
const PIECE_ROWS: u16 = 8;
/// Connection bar rows at most.
const CONN_ROWS: u16 = 8;
/// Blank columns between the two grid columns.
const GUTTER: u16 = 3;

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
    let detail = app
        .detail
        .as_ref()
        .filter(|(id, _)| *id == d.id)
        .map(|(_, e)| e);
    let engine = Engine::of(d, detail);
    let two_col = body.width >= TWO_COLUMNS;
    let plan = Plan::new(body.height, two_col, &engine, body.width);
    let mut c = Cursor {
        area: body,
        y: body.y,
    };
    header(t, buf, &mut c, d, plan.path);
    c.gap(plan.gaps[0]);
    fields(t, buf, &mut c, d, two_col);
    c.gap(plan.gaps[1]);
    if plan.spark > 0 {
        speed(app, buf, &mut c, d, plan.spark);
        c.gap(plan.gaps[2]);
    }
    engine.draw(t, buf, &mut c, d, &plan);
}

/// What the engine section shows.
enum Engine<'a> {
    /// Nothing (detail is not fetched in this state).
    Nothing,
    Placeholder(&'static str),
    Http {
        segments: &'a [SegmentView],
        connections: u16,
        resumable: bool,
    },
    Torrent {
        piece_count: u32,
        have: &'a [u8],
        peers: u32,
        seeds: u32,
        leechers: u32,
        ratio: f32,
    },
}

/// Rows each optional part gets, decided up front so the engine section always shows:
/// with too little room the blank gaps go first, then the path line, then sparkline
/// rows, then connection bars.
struct Plan {
    path: bool,
    /// After the header, after the fields, after the sparkline, before the bars.
    gaps: [bool; 4],
    /// Sparkline rows (0 ⇒ no speed section at all).
    spark: u16,
    /// Connection bar rows (HTTP).
    conn: u16,
    /// Torrent: the swarm summary line.
    summary: bool,
    /// Torrent: piece map rows.
    map: u16,
}

impl Plan {
    fn new(height: u16, two_col: bool, engine: &Engine, width: u16) -> Self {
        let grid = if two_col { 4 } else { 6 };
        let engine_min = match engine {
            Engine::Nothing => 0,
            Engine::Placeholder(_) => 1,
            Engine::Http { .. } => 2,
            Engine::Torrent { .. } => 1 + MAP_MIN,
        };
        let mut avail = height.saturating_sub(1 + grid + engine_min);
        let mut plan = Plan {
            path: false,
            gaps: [false; 4],
            spark: 0,
            conn: 0,
            summary: false,
            map: 0,
        };
        match engine {
            Engine::Http { segments, .. } => {
                let active = segments.iter().filter(|s| s.active).count();
                plan.conn = take(
                    &mut avail,
                    u16::try_from(active).unwrap_or(u16::MAX).min(CONN_ROWS),
                );
            }
            Engine::Torrent { piece_count, .. } => {
                let rows = u32::from(width.max(1));
                let needed = piece_count.div_ceil(rows);
                let needed = u16::try_from(needed).unwrap_or(u16::MAX);
                let extra = needed.clamp(MAP_MIN, PIECE_ROWS) - MAP_MIN;
                plan.map = MAP_MIN + take(&mut avail, extra);
            }
            _ => {}
        }
        // Heading plus up to three rows; a heading alone is pointless.
        if avail >= 2 {
            plan.spark = take(&mut avail, 4) - 1;
        }
        plan.path = take(&mut avail, 1) == 1;
        plan.summary = matches!(engine, Engine::Torrent { .. }) && take(&mut avail, 1) == 1;
        let has_engine = !matches!(engine, Engine::Nothing);
        let wanted = [
            true,
            plan.spark > 0 || has_engine,
            plan.spark > 0 && has_engine,
            plan.conn > 0,
        ];
        for (gap, want) in plan.gaps.iter_mut().zip(wanted) {
            *gap = want && take(&mut avail, 1) == 1;
        }
        plan
    }
}

/// Up to `want` of the `avail` rows.
fn take(avail: &mut u16, want: u16) -> u16 {
    let got = want.min(*avail);
    *avail -= got;
    got
}

impl<'a> Engine<'a> {
    fn of(d: &DownloadSnapshot, detail: Option<&'a EngineDetail>) -> Self {
        match detail {
            None => {
                let fetched = matches!(d.kind, EngineKindTag::Http | EngineKindTag::Torrent)
                    && matches!(
                        d.state,
                        WireState::Active | WireState::Starting | WireState::Paused
                    );
                if fetched {
                    Engine::Placeholder("No detail yet")
                } else {
                    Engine::Nothing
                }
            }
            Some(EngineDetail::Unknown) => Engine::Placeholder("No detail available"),
            Some(EngineDetail::Http {
                segments,
                connections,
                resumable,
            }) => Engine::Http {
                segments,
                connections: *connections,
                resumable: *resumable,
            },
            Some(EngineDetail::Torrent {
                piece_count,
                have,
                peers,
                seeds,
                leechers,
                ratio,
            }) => Engine::Torrent {
                piece_count: *piece_count,
                have,
                peers: *peers,
                seeds: *seeds,
                leechers: *leechers,
                ratio: *ratio,
            },
        }
    }

    fn draw(&self, t: &Theme, buf: &mut Buffer, c: &mut Cursor, d: &DownloadSnapshot, plan: &Plan) {
        let dim = t.style(Token::Dim);
        match *self {
            Engine::Nothing => {}
            Engine::Placeholder(msg) => {
                if let Some(r) = c.line() {
                    text(buf, r.x, r.y, r.width, msg, dim);
                }
            }
            Engine::Http {
                segments,
                connections,
                resumable,
            } => {
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
                c.gap(plan.gaps[3]);
                if let Some(r) = c.take(plan.conn) {
                    ConnectionBars {
                        segments,
                        theme: t,
                        max_rows: usize::from(plan.conn),
                    }
                    .render(r, buf);
                }
            }
            Engine::Torrent {
                piece_count,
                have,
                peers,
                seeds,
                leechers,
                ratio,
            } => {
                let Some(head) = c.line() else { return };
                let got = (0..piece_count)
                    .filter(|&i| {
                        have.get((i / 8) as usize)
                            .is_some_and(|b| b >> (7 - i % 8) & 1 == 1)
                    })
                    .count();
                let note = format!("{got} / {piece_count}");
                heading(t, buf, head, "Pieces", &note);
                if plan.summary
                    && let Some(r) = c.line()
                {
                    let peers = format!("{peers} peers");
                    let swarm = format!(" ({seeds} seeds, {leechers} leechers)");
                    let ratio = format!("ratio {ratio:.2}");
                    let text_st = t.style(Token::Text);
                    groups(
                        buf,
                        r.x,
                        r.y,
                        r.width,
                        &[
                            vec![(peers.as_str(), text_st), (swarm.as_str(), dim)],
                            vec![(ratio.as_str(), text_st)],
                        ],
                        (" · ", dim),
                    );
                }
                if let Some(r) = c.take(plan.map.min(c.left())) {
                    PieceMap {
                        piece_count,
                        have,
                        theme: t,
                    }
                    .render(r, buf);
                }
            }
        }
    }
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

    /// A blank separator row, when the plan has room for it.
    fn gap(&mut self, on: bool) {
        if on && self.left() > 1 {
            self.y += 1;
        }
    }
}

fn header(t: &Theme, buf: &mut Buffer, c: &mut Cursor, d: &DownloadSnapshot, path: bool) {
    if let Some(r) = c.line() {
        let style = t.style(Token::Text).add_modifier(Modifier::BOLD);
        text(buf, r.x, r.y, r.width, &d.name, style);
    }
    // The directory: the file name is the title already.
    let dir = d
        .path
        .as_ref()
        .and_then(|p| p.parent())
        .map(|p| p.display().to_string())
        .filter(|p| !p.is_empty());
    if path
        && let Some(dir) = dir
        && let Some(r) = c.line()
    {
        let safe = format::display_safe(&dir);
        let shown = format::truncate_middle(&safe, usize::from(r.width));
        text(buf, r.x, r.y, r.width, &shown, t.style(Token::Dim));
    }
}

fn fields(t: &Theme, buf: &mut Buffer, c: &mut Cursor, d: &DownloadSnapshot, two_col: bool) {
    let downloaded = match format::percent(d.done_bytes, d.total_bytes) {
        Some(r) if !matches!(d.state, WireState::Completed) => {
            format!("{} ({}%)", size_text(d), (r * 100.0).floor() as u32)
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
    let eta = format::eta(d.eta_secs);
    let source = format::display_safe(&d.source);
    let pairs: [(&str, &str, Token); 6] = [
        ("Downloaded", &downloaded, Token::Text),
        ("Speed", &speed, Token::Text),
        ("ETA", &eta, Token::Text),
        ("State", &state, icon_tok),
        ("Limit", &limit, Token::Text),
        ("Source", &source, Token::Text),
    ];
    let w = c.area.width;
    if two_col {
        // Downloaded and Source get whole rows; the short values pair up.
        let col = w / 2;
        let rows: [&[(&str, &str, Token)]; 4] =
            [&pairs[0..1], &pairs[1..3], &pairs[3..5], &pairs[5..6]];
        for pair in rows {
            let Some(r) = c.line() else { return };
            if let [(key, value, tok)] = pair {
                field(t, buf, r.x, r.y, w, key, value, *tok);
                continue;
            }
            for (k, (key, value, tok)) in pair.iter().enumerate() {
                let x = r.x + k as u16 * col;
                let cw = if k == 0 { col - GUTTER } else { w - col };
                field(t, buf, x, r.y, cw, key, value, *tok);
            }
        }
    } else {
        for (key, value, tok) in pairs {
            let Some(r) = c.line() else { return };
            field(t, buf, r.x, r.y, r.width, key, value, tok);
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn field(t: &Theme, buf: &mut Buffer, x: u16, y: u16, w: u16, key: &str, value: &str, tok: Token) {
    text(buf, x, y, w.min(LABEL_W), key, t.style(Token::Dim));
    if w > LABEL_W {
        let room = w - LABEL_W;
        // URLs keep their tail (the file name) visible.
        let shown = format::truncate_middle(value, usize::from(room));
        text(buf, x + LABEL_W, y, room, &shown, t.style(tok));
    }
}

/// `Speed history` heading with the peak on the right, then `rows` of sparkline.
fn speed(app: &App, buf: &mut Buffer, c: &mut Cursor, d: &DownloadSnapshot, rows: u16) {
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
    let Some(r) = c.take(rows.min(c.left())) else {
        return;
    };
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
