//! Modal popups (add, per-download limit, global limits, confirm delete, help), drawn
//! centered over the dashboard above the footer, whose hints they provide.
//!
//! Each form popup is a list of [`Row`]s; spacer rows are dropped (bottom first) when
//! the screen is too short, so every field, error and button stays visible.

use ratatui::buffer::Buffer;
use ratatui::layout::{Position, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Widget};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use super::{FOOTER_ROWS, panel, spans, text, width, wrap};
use crate::app::{
    AddField, AddForm, App, GlobalLimitsForm, HitTarget, LimitForm, Popup, SeedForm, TextInput,
};
use crate::format;
use crate::theme::{Theme, Token};

/// Popups are at most this wide (and leave 2 columns each side).
pub const POPUP_MAX_WIDTH: u16 = 70;
/// Horizontal padding between the border and the content.
const PAD: u16 = 2;
/// Lines an inline error may wrap to.
const ERROR_LINES: usize = 3;
pub const RATE_EXAMPLES: &str = "e.g. 2M, 500k, none";
pub const RATIO_EXAMPLES: &str = "e.g. 1.5, 0 = off, empty = default";
pub const TIME_EXAMPLES: &str = "e.g. 2h, 30m, 0 = off, empty = default";

/// The help table: a section title (empty keys) or keys and what they do.
const HELP: &[(&str, &str)] = &[
    ("", "Navigate"),
    ("j, ↓", "select next"),
    ("k, ↑", "select previous"),
    ("g, Home", "first download"),
    ("G, End", "last download"),
    ("/", "filter by name or source"),
    ("Esc", "close popup / clear filter"),
    ("", "Downloads"),
    ("a", "add a URL, magnet link or .torrent"),
    ("p", "pause"),
    ("r", "resume (also retries a failed one)"),
    ("d, Delete", "delete (asks first)"),
    ("R", "restart (failed / needs attention)"),
    ("l", "speed limit for the selection"),
    ("c", "choose a torrent's files"),
    ("s", "seeding: ratio and time limits"),
    ("", "File picker"),
    ("Space, click", "toggle a file or folder"),
    ("a, n", "all / none (only the shown files when filtering)"),
    ("←, →", "fold / unfold a folder"),
    ("/", "filter by path"),
    ("Enter, y", "download the selected files"),
    ("", "Queue & limits"),
    ("K, Shift+↑", "move up in the queue"),
    ("J, Shift+↓", "move down in the queue"),
    ("+, -", "more / fewer active downloads"),
    ("L", "global speed limits"),
    ("", "App"),
    ("?", "this help"),
    ("q, Ctrl+C", "quit (the daemon keeps running)"),
    ("", "In popups"),
    ("Tab, Shift+Tab", "next / previous field"),
    ("Space", "toggle a checkbox"),
    ("Enter", "confirm"),
    ("Esc", "cancel"),
    ("", "Mouse & paste"),
    ("click", "select a download"),
    ("double-click", "pause / resume"),
    ("wheel", "scroll the list"),
    ("click a hint", "run that key"),
    ("paste", "add the pasted link"),
];

/// Number of help rows (for clamping the scroll).
pub fn help_rows() -> usize {
    // Blank line before every section but the first.
    HELP.len() + HELP.iter().filter(|(k, _)| k.is_empty()).count() - 1
}

/// One line of a form popup.
enum Row<'a> {
    /// A spacer: dropped first when the popup does not fit.
    Blank,
    Label {
        text: &'a str,
        focused: bool,
    },
    Input {
        input: &'a TextInput,
        placeholder: Option<String>,
        focused: bool,
    },
    /// One wrapped line of an inline error; `first` carries the `✗` mark.
    Error {
        text: String,
        first: bool,
    },
    /// Free text, by parts.
    Text(Vec<(String, Style)>),
    Checkbox {
        checked: bool,
        label: &'a str,
        key: Option<&'a str>,
        focused: bool,
    },
    /// Right-aligned buttons: label, style.
    Buttons(Vec<(&'a str, Style)>),
}

/// Draws the popup for `popup` over the region above the footer; returns where the
/// terminal cursor goes (the focused text input).
pub(super) fn draw(app: &mut App, buf: &mut Buffer, area: Rect) -> Option<Position> {
    let crate::app::Mode::Popup(popup) = &app.mode else {
        return None;
    };
    let region = Rect {
        height: area.height.saturating_sub(FOOTER_ROWS),
        ..area
    };
    let w = POPUP_MAX_WIDTH.min(area.width.saturating_sub(4));
    if w < 2 * PAD + 6 || region.height < 5 {
        return None;
    }
    let t = app.theme;
    let content_w = w - 2 - 2 * PAD;
    let default_dir = app
        .settings
        .as_ref()
        .map(|s| s.download_dir.display().to_string());
    let name_of = |id| {
        app.downloads
            .iter()
            .find(|d| d.id == id)
            .map(|d| d.name.clone())
    };
    if matches!(popup, Popup::Picker(_)) {
        return super::picker::draw(app, buf, region, area);
    }
    let seed_state = match popup {
        Popup::Seed(f) => app
            .downloads
            .iter()
            .find(|d| d.id == f.id)
            .map(|d| d.state.clone()),
        _ => None,
    };
    let (title, danger, rows) = match popup {
        Popup::Picker(_) => return None,
        Popup::Seed(f) => (
            "Seeding",
            false,
            seed_rows(&t, f, name_of(f.id), seed_state.as_ref(), content_w),
        ),
        Popup::Add(f) => (
            "Add download",
            false,
            add_rows(&t, f, default_dir, content_w),
        ),
        Popup::Limit(f) => (
            "Speed limit",
            false,
            limit_rows(&t, f, name_of(f.id), content_w),
        ),
        Popup::GlobalLimits(f) => ("Global speed limits", false, global_rows(&t, f, content_w)),
        Popup::ConfirmDelete {
            name, delete_files, ..
        } => (
            "Delete download",
            true,
            delete_rows(&t, name, *delete_files, content_w),
        ),
        Popup::Help => {
            let rect = frame(region, area, w, help_rows() as u16 + 2);
            help(app, buf, rect);
            app.hits.push((rect, HitTarget::PopupInside));
            return None;
        }
    };
    let rows = fit(rows, region.height.saturating_sub(2));
    let rect = frame(region, area, w, rows.len() as u16 + 2);
    let border = if danger {
        t.style(Token::Err)
    } else {
        t.style(Token::BorderFocus)
    };
    let block = panel(
        &t,
        vec![Span::styled(format!(" {title} "), t.style(Token::Title))],
        true,
    )
    .border_style(border);
    let inner = block.inner(rect);
    Clear.render(rect, buf);
    block.render(rect, buf);
    app.hits.push((rect, HitTarget::PopupInside));

    let x = inner.x + PAD;
    let mut cursor = None;
    for (i, row) in rows.iter().enumerate() {
        let y = inner.y + i as u16;
        if y >= inner.bottom() {
            break;
        }
        if let Some(c) = draw_row(&t, buf, &mut app.hits, row, x, y, content_w) {
            cursor = Some(c);
        }
    }
    cursor
}

/// A `w`×`h` rect centered in `region` (clamped to it), horizontally in `area`.
pub(super) fn frame(region: Rect, area: Rect, w: u16, h: u16) -> Rect {
    let h = h.min(region.height);
    Rect::new(
        area.x + (area.width - w) / 2,
        region.y + (region.height - h) / 2,
        w,
        h,
    )
}

/// Drops spacers (bottom first), then trailing rows, until `rows` fits `max`.
fn fit(mut rows: Vec<Row<'_>>, max: u16) -> Vec<Row<'_>> {
    let max = usize::from(max);
    while rows.len() > max {
        match rows.iter().rposition(|r| matches!(r, Row::Blank)) {
            Some(i) => {
                rows.remove(i);
            }
            None => rows.truncate(max),
        }
    }
    rows
}

fn error_rows<'a>(rows: &mut Vec<Row<'a>>, error: Option<&str>, w: u16) {
    if let Some(e) = error {
        let lines = wrap(e, usize::from(w.saturating_sub(2)), ERROR_LINES);
        for (i, line) in lines.into_iter().enumerate() {
            rows.push(Row::Error {
                text: line,
                first: i == 0,
            });
        }
    }
}

fn add_rows<'a>(t: &Theme, f: &'a AddForm, default_dir: Option<String>, w: u16) -> Vec<Row<'a>> {
    let err_on = |field| f.error.as_deref().filter(|_| f.error_on == field);
    let mut rows = vec![
        Row::Blank,
        Row::Label {
            text: "URL / magnet / .torrent",
            focused: f.focus == AddField::Source,
        },
        Row::Input {
            input: &f.source,
            placeholder: Some("https://…  magnet:?…  ~/file.torrent".into()),
            focused: f.focus == AddField::Source,
        },
    ];
    error_rows(&mut rows, err_on(AddField::Source), w);
    rows.extend([
        Row::Blank,
        Row::Label {
            text: "Save to",
            focused: f.focus == AddField::Dir,
        },
        Row::Input {
            input: &f.dir,
            placeholder: Some(match default_dir {
                Some(d) => format!("{d}  (default)"),
                None => "the daemon's default folder".into(),
            }),
            focused: f.focus == AddField::Dir,
        },
    ]);
    error_rows(&mut rows, err_on(AddField::Dir), w);
    let ok = if f.focus == AddField::Ok {
        t.style(Token::Badge)
    } else {
        t.style(Token::Text)
    };
    rows.extend([
        Row::Blank,
        Row::Checkbox {
            checked: f.start_paused,
            label: "start paused",
            key: None,
            focused: f.focus == AddField::Paused,
        },
        Row::Blank,
        Row::Buttons(vec![("[ Add ]", ok)]),
        Row::Blank,
    ]);
    rows
}

fn limit_rows<'a>(t: &Theme, f: &'a LimitForm, name: Option<String>, w: u16) -> Vec<Row<'a>> {
    let name = name.unwrap_or_else(|| format!("download {}", f.id));
    let mut rows = vec![
        Row::Blank,
        Row::Text(vec![(
            format::truncate_middle(&format::display_safe(&name), usize::from(w)),
            t.style(Token::Text).add_modifier(Modifier::BOLD),
        )]),
        Row::Blank,
        Row::Label {
            text: "Max speed per second",
            focused: true,
        },
        Row::Input {
            input: &f.input,
            placeholder: Some("no limit".into()),
            focused: true,
        },
    ];
    error_rows(&mut rows, f.error.as_deref(), w);
    rows.extend([
        Row::Text(vec![(
            format!("{RATE_EXAMPLES} (empty = no limit)"),
            t.style(Token::Dim),
        )]),
        Row::Blank,
        Row::Buttons(vec![("[ Set ]", t.style(Token::Badge))]),
        Row::Blank,
    ]);
    rows
}

fn global_rows<'a>(t: &Theme, f: &'a GlobalLimitsForm, w: u16) -> Vec<Row<'a>> {
    let mut rows = vec![Row::Blank];
    for (i, label, input) in [(0, "Download ↓", &f.down), (1, "Upload ↑", &f.up)] {
        if i == 1 {
            rows.push(Row::Blank);
        }
        rows.push(Row::Label {
            text: label,
            focused: f.focus == i,
        });
        rows.push(Row::Input {
            input,
            placeholder: Some("no limit".into()),
            focused: f.focus == i,
        });
        error_rows(
            &mut rows,
            f.error.as_deref().filter(|_| f.error_field == i),
            w,
        );
    }
    rows.extend([
        Row::Blank,
        Row::Text(vec![(
            format!("{RATE_EXAMPLES} (empty = no limit)"),
            t.style(Token::Dim),
        )]),
        Row::Blank,
        Row::Buttons(vec![("[ Set ]", t.style(Token::Badge))]),
        Row::Blank,
    ]);
    rows
}

fn seed_rows<'a>(
    t: &Theme,
    f: &'a SeedForm,
    name: Option<String>,
    state: Option<&puppy_proto::WireState>,
    w: u16,
) -> Vec<Row<'a>> {
    let name = name.unwrap_or_else(|| format!("download {}", f.id));
    let mut rows = vec![
        Row::Blank,
        Row::Text(vec![(
            format::truncate_middle(&format::display_safe(&name), usize::from(w)),
            t.style(Token::Text).add_modifier(Modifier::BOLD),
        )]),
    ];
    // Where it stands now.
    let now = match state {
        Some(puppy_proto::WireState::Seeding {
            ratio, seeded_secs, ..
        }) => format!(
            "seeding · ratio {ratio:.2} · seeded {}",
            format::eta(Some(*seeded_secs))
        ),
        _ => "applies once the download completes".to_string(),
    };
    rows.push(Row::Text(vec![(now, t.style(Token::Dim))]));
    for (i, label, input, examples, default) in [
        (0, "Ratio", &f.ratio, RATIO_EXAMPLES, &f.ratio_default),
        (1, "Time", &f.time, TIME_EXAMPLES, &f.time_default),
    ] {
        rows.push(Row::Blank);
        rows.push(Row::Label {
            text: label,
            focused: f.focus == i,
        });
        rows.push(Row::Input {
            input,
            placeholder: Some(default.clone()),
            focused: f.focus == i,
        });
        error_rows(
            &mut rows,
            f.error.as_deref().filter(|_| f.error_field == i),
            w,
        );
        rows.push(Row::Text(vec![(examples.to_string(), t.style(Token::Dim))]));
    }
    rows.extend([
        Row::Blank,
        Row::Buttons(vec![("[ Set ]", t.style(Token::Badge))]),
        Row::Blank,
    ]);
    rows
}

fn delete_rows<'a>(t: &Theme, name: &str, delete_files: bool, w: u16) -> Vec<Row<'a>> {
    let room = usize::from(w).saturating_sub(width("Delete ?").into());
    let name = format::truncate_middle(&format::display_safe(name), room);
    let text = t.style(Token::Text);
    let (note, note_style) = if delete_files {
        (
            "the downloaded files will be removed from disk",
            t.style(Token::Warn),
        )
    } else {
        ("downloaded files stay on disk", t.style(Token::Dim))
    };
    vec![
        Row::Blank,
        Row::Text(vec![
            ("Delete ".into(), text),
            (name, text.add_modifier(Modifier::BOLD)),
            ("?".into(), text),
        ]),
        Row::Blank,
        Row::Checkbox {
            checked: delete_files,
            label: "also delete files",
            key: Some("f"),
            focused: true,
        },
        Row::Text(vec![(format!("    {note}"), note_style)]),
        Row::Blank,
        Row::Buttons(vec![(
            "[ Delete ]",
            t.style(Token::Err)
                .add_modifier(Modifier::REVERSED | Modifier::BOLD),
        )]),
        Row::Blank,
    ]
}

/// Draws one row at (`x`, `y`) in `w` columns; returns the cursor for a focused input.
fn draw_row(
    t: &Theme,
    buf: &mut Buffer,
    hits: &mut crate::app::HitMap,
    row: &Row<'_>,
    x: u16,
    y: u16,
    w: u16,
) -> Option<Position> {
    let focus_bar = |buf: &mut Buffer| {
        text(buf, x - PAD, y, 1, "▌", t.style(Token::Accent));
    };
    match row {
        Row::Blank => {}
        Row::Label { text: s, focused } => {
            let style = if *focused {
                t.style(Token::Title)
            } else {
                t.style(Token::Text)
            };
            text(buf, x, y, w, s, style);
        }
        Row::Input {
            input,
            placeholder,
            focused,
        } => {
            let field = if *focused {
                focus_bar(buf);
                t.style(Token::Selected)
            } else {
                t.style(Token::Text)
            };
            buf.set_style(Rect::new(x, y, w, 1), field);
            let room = w.saturating_sub(2);
            if input.is_empty() {
                if let Some(p) = placeholder {
                    let dim = if *focused {
                        t.on_selected(Token::Dim)
                    } else {
                        t.style(Token::Dim)
                    };
                    text(buf, x + 1, y, room, p, dim);
                }
                return focused.then_some(Position::new(x + 1, y));
            }
            let (shown, col) = input_view(input, usize::from(room));
            let style = if *focused {
                t.on_selected(Token::Text)
            } else {
                t.style(Token::Text)
            };
            text(buf, x + 1, y, room, &shown, style);
            return focused.then_some(Position::new(x + 1 + col, y));
        }
        Row::Error { text: s, first } => {
            let mark = if *first { "✗ " } else { "  " };
            spans(
                buf,
                x,
                y,
                w,
                &[(mark, t.style(Token::Err)), (s, t.style(Token::Err))],
            );
        }
        Row::Text(parts) => {
            let parts: Vec<(&str, Style)> = parts.iter().map(|(s, st)| (s.as_str(), *st)).collect();
            spans(buf, x, y, w, &parts);
        }
        Row::Checkbox {
            checked,
            label,
            key,
            focused,
        } => {
            if *focused {
                focus_bar(buf);
            }
            let (mark, mark_style) = if *checked {
                ("[x]", t.style(Token::Accent))
            } else {
                ("[ ]", t.style(Token::Text))
            };
            let label_style = if *focused {
                t.style(Token::Title)
            } else {
                t.style(Token::Text)
            };
            let hint = key.map(|k| format!("  ({k})")).unwrap_or_default();
            let used = spans(
                buf,
                x,
                y,
                w,
                &[
                    (mark, mark_style),
                    (" ", Style::new()),
                    (label, label_style),
                    (&hint, t.style(Token::Dim)),
                ],
            );
            hits.push((Rect::new(x, y, used, 1), HitTarget::PopupCheckbox));
        }
        Row::Buttons(buttons) => {
            let total: u16 = buttons.iter().map(|(s, _)| width(s) + 2).sum::<u16>() - 2;
            let mut bx = x + w.saturating_sub(total);
            for (s, style) in buttons {
                bx += text(buf, bx, y, x + w - bx, s, *style) + 2;
            }
        }
    }
    None
}

/// The part of `input` shown in `room` columns, scrolled so the cursor is visible, and
/// the cursor's column in it.
fn input_view(input: &TextInput, room: usize) -> (String, u16) {
    let value = format::display_safe(&input.value);
    let gs: Vec<&str> = value.graphemes(true).collect();
    let cursor = input.cursor.min(gs.len());
    // The cursor needs a column of its own after the text before it.
    let mut start = 0;
    let mut before: usize = gs[..cursor].iter().map(|g| g.width()).sum();
    while before + 1 > room && start < cursor {
        before -= gs[start].width();
        start += 1;
    }
    let mut shown = String::new();
    let mut used = 0;
    for g in &gs[start..] {
        if used + g.width() > room {
            break;
        }
        used += g.width();
        shown.push_str(g);
    }
    (shown, u16::try_from(before).unwrap_or(u16::MAX))
}

/// The help popup: a scrollable two-column table of keys.
fn help(app: &mut App, buf: &mut Buffer, rect: Rect) {
    let t = app.theme;
    let total = help_rows();
    let visible = usize::from(rect.height.saturating_sub(2));
    let max_scroll = total.saturating_sub(visible);
    app.help_scroll = app
        .help_scroll
        .min(u16::try_from(max_scroll).unwrap_or(u16::MAX));
    let scroll = usize::from(app.help_scroll);

    let mut block = panel(
        &t,
        vec![Span::styled(" Help · keys ", t.style(Token::Title))],
        true,
    );
    if max_scroll > 0 {
        let pos = format!(
            " {}–{} of {} · j/k scroll ",
            scroll + 1,
            (scroll + visible).min(total),
            total
        );
        block =
            block.title_bottom(Line::from(Span::styled(pos, t.style(Token::Dim))).right_aligned());
    }
    let inner = block.inner(rect);
    Clear.render(rect, buf);
    block.render(rect, buf);

    let key_w = HELP.iter().map(|(k, _)| width(k)).max().unwrap_or(0);
    let x = inner.x + PAD;
    let w = inner.width.saturating_sub(2 * PAD);
    let mut lines: Vec<Option<(&str, &str)>> = Vec::with_capacity(total);
    for (i, (k, d)) in HELP.iter().enumerate() {
        if k.is_empty() && i > 0 {
            lines.push(None);
        }
        lines.push(Some((k, d)));
    }
    for (row, line) in lines.iter().skip(scroll).take(visible).enumerate() {
        let y = inner.y + row as u16;
        let Some((keys, what)) = line else { continue };
        if keys.is_empty() {
            text(buf, x, y, w, what, t.style(Token::Title));
            continue;
        }
        // Keys separated by `, `: keys in Key style, separators dimmed.
        let mut kx = x + 2;
        for (i, k) in keys.split(", ").enumerate() {
            if i > 0 {
                kx += text(
                    buf,
                    kx,
                    y,
                    (x + w).saturating_sub(kx),
                    ", ",
                    t.style(Token::Dim),
                );
            }
            kx += text(
                buf,
                kx,
                y,
                (x + w).saturating_sub(kx),
                k,
                t.style(Token::Key),
            );
        }
        let dx = x + 2 + key_w + 2;
        text(
            buf,
            dx,
            y,
            (x + w).saturating_sub(dx),
            what,
            t.style(Token::Text),
        );
    }
}
