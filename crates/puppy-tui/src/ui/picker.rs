//! The file picker popup: a folder tree with checkboxes and sizes, the selected totals
//! in the title, an optional filter line, an inline error and the confirm button.
//!
//! ```text
//! ╭ Choose files — Show ──────────── 3 of 4 files · 650.0 MiB of 951.0 MiB ╮
//! │ ▾ ▣ S01                                                     600.0 MiB  │
//! │     ☑ e1.mkv                                            1   300.0 MiB  │
//! │     ☐ e2.mkv                                            2   300.0 MiB  │
//! │ ▸ ☑ S02                                                     350.0 MiB  │
//! │   ☑ notes.txt                                           4     1.0 MiB  │
//! │                                                                        │
//! │                                                         [ Download ]   │
//! ╰────────────────────────────────────────────────────────────────────────╯
//! ```

use ratatui::buffer::Buffer;
use ratatui::layout::{Position, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Widget};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use super::popup::frame;
use super::{panel, spans, text, width, wrap};
use crate::app::picker::Check;
use crate::app::{App, HitTarget, Mode, Popup};
use crate::format;
use crate::theme::Token;

/// The picker is at most this wide (wider than the form popups: paths are long).
pub const PICKER_MAX_WIDTH: u16 = 100;
/// Narrower than this (or shorter than [`MIN_HEIGHT`]) and the picker is not drawn.
const MIN_WIDTH: u16 = 30;
const MIN_HEIGHT: u16 = 6;
/// The size column (`1023.9 MiB`).
const SIZE_W: u16 = 10;
/// From this content width on, files show their 1-based number.
const NUMBERS_FROM: u16 = 50;
/// Lines an inline error may wrap to.
const ERROR_LINES: usize = 2;
pub const TITLE: &str = "Choose files — ";
pub const LOADING: &str = "loading the file list…";
pub const BUTTON: &str = "[ Download ]";
pub const SENDING: &str = "choosing…";

/// Draws the picker over `region` (above the footer), centered in `area`; returns the
/// filter's cursor while it is typed into.
pub(super) fn draw(app: &mut App, buf: &mut Buffer, region: Rect, area: Rect) -> Option<Position> {
    let t = app.theme;
    let w = PICKER_MAX_WIDTH.min(area.width.saturating_sub(4));
    if w < MIN_WIDTH || region.height < MIN_HEIGHT {
        return None;
    }
    let note = match &app.mode {
        Mode::Popup(Popup::Picker(p)) => app
            .downloads
            .iter()
            .find(|d| d.id == p.id)
            .and_then(|d| d.note.clone()),
        _ => None,
    };
    // Drawing takes the hit list while it holds the picker.
    let mut hits = std::mem::take(&mut app.hits);
    let cursor = match &mut app.mode {
        Mode::Popup(Popup::Picker(p)) => draw_picker(&t, p, note, &mut hits, buf, region, area, w),
        _ => None,
    };
    app.hits = hits;
    cursor
}

#[allow(clippy::too_many_arguments)]
fn draw_picker(
    t: &crate::theme::Theme,
    p: &mut crate::app::Picker,
    note: Option<String>,
    hits: &mut crate::app::HitMap,
    buf: &mut Buffer,
    region: Rect,
    area: Rect,
    w: u16,
) -> Option<Position> {
    let t = *t;
    let rows = p.rows();
    let matching = p.matches();
    let (n, total_n, bytes, total_bytes) = p.totals();
    let content_w = w - 4;

    // Rows inside the border.
    let filter_row = u16::from(p.filtering || !p.filter.is_empty());
    let errors = p
        .error
        .as_deref()
        .map(|e| wrap(e, usize::from(content_w.saturating_sub(2)), ERROR_LINES))
        .unwrap_or_default();
    let max_inner = region.height - 2;
    let mut err_h = errors.len() as u16;
    let mut fixed = filter_row + err_h + 1;
    if fixed + 1 > max_inner {
        err_h = 0;
        fixed = filter_row + 1;
    }
    let wanted = if p.loading {
        3
    } else {
        u16::try_from(rows.len()).unwrap_or(u16::MAX).max(1)
    };
    let list_h = wanted.min(max_inner.saturating_sub(fixed)).max(1);
    let spacer = u16::from(list_h + fixed < max_inner);
    let rect = frame(region, area, w, 2 + fixed + list_h + spacer);

    // Title: the name on the left, the selected totals on the right when both fit.
    let totals = format!(
        " {n} of {total_n} files · {} of {} ",
        format::bytes(bytes),
        format::bytes(total_bytes)
    );
    let avail = w.saturating_sub(4);
    let totals_in_title = !p.loading && width(TITLE) + 10 + width(&totals) <= avail;
    let name_room = avail
        .saturating_sub(width(TITLE) + 2)
        .saturating_sub(if totals_in_title {
            width(&totals) + 1
        } else {
            0
        });
    let name = format::truncate(&format::display_safe(&p.name), usize::from(name_room));
    let mut block = panel(
        &t,
        vec![
            Span::styled(format!(" {TITLE}"), t.style(Token::Title)),
            Span::styled(
                format!("{name} "),
                t.style(Token::Text).add_modifier(Modifier::BOLD),
            ),
        ],
        true,
    );
    if totals_in_title {
        block = block.title(
            Line::from(Span::styled(totals.clone(), t.style(Token::Accent))).right_aligned(),
        );
    }

    // Scroll: the cursor stays visible.
    let shown = usize::from(list_h);
    p.page = shown;
    p.cursor = p.cursor.min(rows.len().saturating_sub(1));
    let mut offset = p.offset.min(rows.len().saturating_sub(shown));
    if p.cursor < offset {
        offset = p.cursor;
    } else if p.cursor >= offset + shown {
        offset = p.cursor + 1 - shown;
    }
    p.offset = offset;
    if rows.len() > shown {
        let pos = format!(
            " {}–{} of {} ",
            offset + 1,
            (offset + shown).min(rows.len()),
            rows.len()
        );
        block =
            block.title_bottom(Line::from(Span::styled(pos, t.style(Token::Dim))).right_aligned());
    }
    let inner = block.inner(rect);
    Clear.render(rect, buf);
    block.render(rect, buf);
    hits.push((rect, HitTarget::PopupInside));

    let x0 = inner.x + 1;
    let cw = inner.width.saturating_sub(2);
    let mut y = inner.y;
    let mut cursor = None;
    let dim = t.style(Token::Dim);

    if filter_row == 1 {
        let used = spans(
            buf,
            x0,
            y,
            cw,
            &[("/", t.style(Token::Key)), (" ", Style::new())],
        );
        let count = matching.iter().filter(|m| **m).count();
        let found = format!("{count} match{}", if count == 1 { "" } else { "es" });
        let fw = width(&found);
        let room = cw.saturating_sub(used + fw + 2);
        let value = format::display_safe(&p.filter.value);
        let drawn = text(buf, x0 + used, y, room, &value, t.style(Token::Text));
        if cw > used + fw + 2 {
            text(buf, x0 + cw - fw, y, fw, &found, dim);
        }
        if p.filtering {
            let before: usize = value
                .graphemes(true)
                .take(p.filter.cursor)
                .map(UnicodeWidthStr::width)
                .sum();
            let before = u16::try_from(before).unwrap_or(u16::MAX).min(drawn);
            cursor = Some(Position::new(x0 + used + before, y));
        }
        y += 1;
    }

    let list = Rect::new(inner.x, y, inner.width, list_h);
    if p.loading {
        super::centered(buf, list, y + list_h / 2, &[(LOADING, dim)]);
    } else if rows.is_empty() {
        let needle = format::truncate(
            &format::display_safe(p.filter.value.trim()),
            usize::from(cw / 2),
        );
        let quoted = format!("“{needle}”");
        let msg = if p.files.is_empty() {
            vec![("this torrent lists no files", dim)]
        } else {
            vec![
                ("no files match ", dim),
                (quoted.as_str(), t.style(Token::Text)),
                (" — ", dim),
                ("Esc", t.style(Token::Key)),
                (" clears", dim),
            ]
        };
        super::centered(buf, list, y, &msg);
    } else {
        let num_w = if cw >= NUMBERS_FROM {
            p.files.len().to_string().len() as u16
        } else {
            0
        };
        let right = SIZE_W + if num_w > 0 { num_w + 2 } else { 0 };
        let filtered = !p.filter.value.trim().is_empty();
        for (k, &node) in rows.iter().enumerate().skip(offset).take(shown) {
            let ry = y + (k - offset) as u16;
            let selected = k == p.cursor;
            let st = |tok| {
                if selected {
                    t.on_selected(tok)
                } else {
                    t.style(tok)
                }
            };
            let row_rect = Rect::new(inner.x, ry, inner.width, 1);
            if selected {
                buf.set_style(row_rect, t.style(Token::Selected));
                text(buf, inner.x, ry, 1, "▌", st(Token::BorderFocus));
            }
            hits.push((row_rect, HitTarget::PickerRow(k)));
            let n = &p.nodes[node];
            let indent = (n.depth as u16 * 2).min(cw / 3);
            let x = x0 + indent;
            let check = match n.file {
                Some(f) if p.checked[f] => Check::All,
                Some(_) => Check::None,
                None => p.check_with(node, &matching),
            };
            if n.is_folder() {
                let open = filtered || !p.collapsed.contains(&node);
                let glyph = if open { "▾" } else { "▸" };
                text(buf, x, ry, 1, glyph, st(Token::Accent));
                hits.push((Rect::new(x, ry, 2, 1), HitTarget::PickerFold(k)));
            }
            let (mark, mark_tok) = match check {
                Check::All => ("☑", Token::Accent),
                Check::Some => ("▣", Token::Accent),
                Check::None => ("☐", Token::Dim),
            };
            text(buf, x + 2, ry, 1, mark, st(mark_tok));
            let name_x = x + 4;
            let name_room = (x0 + cw).saturating_sub(name_x + right + 1);
            let mut name_style = if check == Check::None {
                st(Token::Dim)
            } else {
                st(Token::Text)
            };
            if n.is_folder() {
                name_style = name_style.add_modifier(Modifier::BOLD);
            }
            text(buf, name_x, ry, name_room, &n.name, name_style);
            // Right: the file's number (1-based, as `puppy select` takes it), the size.
            let size_tok = if check == Check::None {
                Token::Dim
            } else {
                Token::Text
            };
            if num_w > 0
                && let Some(f) = n.file
            {
                let num = format!("{:>w$}", p.files[f].index + 1, w = usize::from(num_w));
                text(buf, x0 + cw - right, ry, num_w, &num, st(Token::Dim));
            }
            let size = format!("{:>w$}", format::bytes(n.size), w = usize::from(SIZE_W));
            text(buf, x0 + cw - SIZE_W, ry, SIZE_W, &size, st(size_tok));
        }
        if rows.len() > shown {
            scrollbar(&t, buf, rect, list, offset, shown, rows.len());
        }
    }
    y += list_h;

    for (i, line) in errors.iter().take(usize::from(err_h)).enumerate() {
        let mark = if i == 0 { "✗ " } else { "  " };
        spans(
            buf,
            x0,
            y,
            cw,
            &[(mark, t.style(Token::Err)), (line, t.style(Token::Err))],
        );
        y += 1;
    }
    y += spacer;

    // The button row: the torrent's note (why it waits) or the totals on the left.
    let (label, label_style) = if p.submitting {
        (SENDING, dim)
    } else {
        (BUTTON, t.style(Token::Badge))
    };
    let bw = width(label);
    let bx = (x0 + cw).saturating_sub(bw);
    let left = note
        .map(|n| (n, t.style(Token::Warn)))
        .or_else(|| (!totals_in_title && !p.loading).then(|| (totals.trim().to_string(), dim)));
    if let Some((s, style)) = left {
        text(buf, x0, y, bx.saturating_sub(x0 + 2), &s, style);
    }
    if !p.loading {
        text(buf, bx, y, bw, label, label_style);
    }
    if !p.loading && !p.submitting {
        hits.push((Rect::new(bx, y, bw, 1), HitTarget::PopupButton));
    }
    cursor
}

/// Thumb over the picker's right border, beside the list rows.
fn scrollbar(
    t: &crate::theme::Theme,
    buf: &mut Buffer,
    rect: Rect,
    list: Rect,
    offset: usize,
    shown: usize,
    count: usize,
) {
    let track = usize::from(list.height);
    if track == 0 || rect.width == 0 || count <= shown {
        return;
    }
    let thumb = (track * shown / count).clamp(1, track);
    let pos = (track - thumb) * offset / (count - shown);
    let x = rect.right() - 1;
    for k in pos..pos + thumb {
        if let Some(cell) = buf.cell_mut((x, list.y + k as u16)) {
            cell.set_symbol("┃").set_style(t.style(Token::BorderFocus));
        }
    }
}
