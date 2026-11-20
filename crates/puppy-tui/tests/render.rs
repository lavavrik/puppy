//! Dashboard rendering (`ui::draw`) on a `TestBackend`.

use std::time::Instant;

use puppy_core::types::{DownloadId, EngineDetail, SegmentView};
use puppy_proto::{DownloadSnapshot, EngineKindTag, Settings, Totals, WireAttention, WireState};
use puppy_tui::app::{
    AddForm, App, Conn, GlobalLimitsForm, HitTarget, LimitForm, Mode, Popup, TextInput,
};
use puppy_tui::theme::{Theme, ThemeChoice};
use puppy_tui::ui::draw;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::style::{Color, Modifier};
use unicode_width::UnicodeWidthStr;

const MIB: u64 = 1024 * 1024;
const GIB: u64 = 1024 * MIB;

fn truecolor() -> Theme {
    Theme::detect(ThemeChoice::Dark, &|k| {
        (k == "COLORTERM").then(|| "truecolor".to_string())
    })
}

fn snap(id: DownloadId, name: &str, state: WireState) -> DownloadSnapshot {
    DownloadSnapshot {
        id,
        kind: EngineKindTag::Http,
        source: format!("https://example.com/{name}"),
        name: name.into(),
        path: Some(format!("/home/me/Downloads/{name}").into()),
        state,
        total_bytes: Some(GIB),
        done_bytes: GIB / 4,
        down_bps: 0,
        up_bps: 0,
        eta_secs: None,
        limit: None,
        queue_position: None,
        connections: None,
        peers: None,
        next_retry_unix: None,
    }
}

fn settings() -> Settings {
    Settings {
        max_active: 3,
        global_down_limit: None,
        global_up_limit: None,
        download_dir: "/tmp".into(),
        connections: 8,
        theme: "auto".into(),
    }
}

fn app_with(theme: Theme, downloads: Vec<DownloadSnapshot>) -> App {
    let mut a = App::new(theme);
    a.conn = Conn::Connected;
    a.daemon_version = Some("0.1.0".into());
    a.settings = Some(settings());
    a.selected = downloads.first().map(|d| d.id);
    a.downloads = downloads;
    a
}

fn app(downloads: Vec<DownloadSnapshot>) -> App {
    app_with(truecolor(), downloads)
}

fn render_buf(app: &mut App, w: u16, h: u16) -> Buffer {
    let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
    term.draw(|f| draw(app, f)).unwrap();
    term.backend().buffer().clone()
}

/// Rows as text; cells hidden behind a wide glyph are skipped.
fn rows(buf: &Buffer) -> Vec<String> {
    let a = buf.area;
    (a.y..a.bottom())
        .map(|y| {
            let mut s = String::new();
            let mut skip = 0;
            for x in a.x..a.right() {
                if skip > 0 {
                    skip -= 1;
                    continue;
                }
                let sym = buf[(x, y)].symbol();
                s.push_str(sym);
                skip = sym.width().saturating_sub(1);
            }
            s
        })
        .collect()
}

fn render(app: &mut App, w: u16, h: u16) -> Vec<String> {
    rows(&render_buf(app, w, h))
}

fn find(rows: &[String], needle: &str) -> Option<usize> {
    rows.iter().position(|r| r.contains(needle))
}

/// (x, y) of the first cell where `needle` starts (ASCII needles).
fn find_xy(buf: &Buffer, needle: &str) -> Option<(u16, u16)> {
    find_xy_before(buf, needle, buf.area.right())
}

/// [`find_xy`] left of column `max_x` (the list, in the wide layout).
fn find_xy_before(buf: &Buffer, needle: &str, max_x: u16) -> Option<(u16, u16)> {
    let a = buf.area;
    for y in a.y..a.bottom() {
        for x in a.x..max_x {
            let mut ok = true;
            for (i, c) in needle.chars().enumerate() {
                let xx = x + i as u16;
                if xx >= a.right() || buf[(xx, y)].symbol() != c.to_string() {
                    ok = false;
                    break;
                }
            }
            if ok {
                return Some((x, y));
            }
        }
    }
    None
}

fn active(id: DownloadId, name: &str) -> DownloadSnapshot {
    DownloadSnapshot {
        done_bytes: 461 * MIB,
        down_bps: 11 * MIB + 4 * MIB / 10,
        eta_secs: Some(41),
        ..snap(id, name, WireState::Active)
    }
}

#[test]
fn wide_layout_has_list_and_detail() {
    let mut a = app(vec![active(1, "ubuntu.iso")]);
    let r = render(&mut a, 160, 40);
    let row = find(&r, "Downloads").expect("list title");
    assert!(r[row].contains("Details"), "{:?}", r[row]);
    assert!(r[0].contains("puppy"), "{:?}", r[0]);
}

#[test]
fn narrow_layout_stacks() {
    let mut a = app(vec![active(1, "ubuntu.iso")]);
    let r = render(&mut a, 90, 40);
    let list = find(&r, "Downloads").expect("list title");
    let detail = find(&r, "Details").expect("detail title");
    assert!(detail > list, "{list} {detail}");
    // At the minimum height the detail pane is dropped, the list kept.
    let r = render(&mut a, 80, 16);
    assert!(find(&r, "Downloads").is_some());
    assert!(find(&r, "Details").is_none());
}

#[test]
fn too_small_message() {
    let mut a = app(vec![active(1, "ubuntu.iso")]);
    let r = render(&mut a, 50, 10);
    let row = find(&r, "puppy needs a bigger terminal (60×16)").expect("message");
    assert!((3..=6).contains(&row), "centered vertically: {row}");
    assert!(r[row].starts_with("      "), "centered: {:?}", r[row]);
    assert!(find(&r, "ubuntu").is_none());
    assert!(a.hits.is_empty());
}

#[test]
fn list_row_shows_state_icon_name_percent_speed_eta() {
    let mut a = app(vec![active(7, "ubuntu.iso")]);
    let r = render(&mut a, 160, 40);
    let row = find(&r, "ubuntu.iso").unwrap();
    assert!(r[row].contains('▶'), "{:?}", r[row]);
    assert!(r[row].contains("45%"), "{:?}", r[row]);
    assert!(r[row].contains('7'), "{:?}", r[row]);
    let line2 = &r[row + 1];
    assert!(line2.contains("11.4 MiB/s"), "{line2:?}");
    assert!(line2.contains("0:41"), "{line2:?}");
    assert!(line2.contains("1.0 GiB"), "{line2:?}");
    // The kind badge marks torrents only.
    assert!(!r[row].contains("[T]"));
    let mut t = active(8, "debian.torrent");
    t.kind = EngineKindTag::Torrent;
    t.peers = Some(12);
    let mut a = app(vec![t]);
    let r = render(&mut a, 160, 40);
    let row = find(&r, "debian.torrent").unwrap();
    assert!(r[row].contains("[T]"), "{:?}", r[row]);
    assert!(r[row + 1].contains("12 peers"), "{:?}", r[row + 1]);
}

#[test]
fn state_icons_and_second_lines() {
    let mut q = snap(2, "queued.bin", WireState::Queued);
    q.queue_position = Some(1);
    let mut retry = snap(
        4,
        "retry.bin",
        WireState::Failed {
            reason: "connection reset".into(),
            transient: true,
        },
    );
    retry.next_retry_unix = Some(1_083);
    let downloads = vec![
        snap(1, "paused.bin", WireState::Paused),
        q,
        snap(3, "done.bin", WireState::Completed),
        retry,
        snap(
            5,
            "broken.bin",
            WireState::Failed {
                reason: "HTTP 404".into(),
                transient: false,
            },
        ),
        snap(
            6,
            "changed.bin",
            WireState::NeedsAttention {
                reason: WireAttention::RemoteChanged,
            },
        ),
        snap(7, "starting.bin", WireState::Starting),
        snap(8, "future.bin", WireState::Unknown),
    ];
    let mut a = app(downloads);
    a.now_unix = 1_000;
    let buf = render_buf(&mut a, 160, 40);
    let r = rows(&buf);
    let line = |name: &str| find(&r, name).unwrap_or_else(|| panic!("{name}: {r:#?}"));
    for (name, icon) in [
        ("paused.bin", '⏸'),
        ("queued.bin", '…'),
        ("done.bin", '✓'),
        ("retry.bin", '↻'),
        ("broken.bin", '✗'),
        ("changed.bin", '!'),
        ("starting.bin", '◌'),
        ("future.bin", '?'),
    ] {
        assert!(r[line(name)].contains(icon), "{name}: {:?}", r[line(name)]);
    }
    assert!(r[line("queued.bin") + 1].contains("queued #2"));
    assert!(r[line("retry.bin") + 1].contains("retry in 1:23"));
    assert!(r[line("broken.bin") + 1].contains("HTTP 404"));
    assert!(r[line("changed.bin") + 1].contains("remote file changed"));
    // Unknown state: the `?` is dim.
    let (x, y) = find_xy(&buf, "future.bin").unwrap();
    let t = truecolor();
    let q = (1..x).find(|&xx| buf[(xx, y)].symbol() == "?").unwrap();
    assert_eq!(buf[(q, y)].fg, t.p.dim);
    // Failed: `✗` in the error colour.
    let (x, y) = find_xy(&buf, "broken.bin").unwrap();
    let cross = (1..x).find(|&xx| buf[(xx, y)].symbol() == "✗").unwrap();
    assert_eq!(buf[(cross, y)].fg, t.p.err);
}

#[test]
fn selected_row_highlighted() {
    let mut a = app(vec![active(1, "one.iso"), active(2, "two.iso")]);
    a.selected = Some(2);
    let t = truecolor();
    let buf = render_buf(&mut a, 160, 40);
    let (x2, y2) = find_xy_before(&buf, "two.iso", 90).unwrap();
    let (x1, y1) = find_xy_before(&buf, "one.iso", 90).unwrap();
    assert_eq!(buf[(x2, y2)].bg, t.p.selection_bg);
    // Both lines, including blank cells after the text.
    assert_eq!(buf[(x2 + 30, y2 + 1)].bg, t.p.selection_bg);
    assert_eq!(buf[(x2 - 2, y2 + 1)].bg, t.p.selection_bg);
    assert_ne!(buf[(x1, y1)].bg, t.p.selection_bg);
    assert_ne!(buf[(x1 + 30, y1 + 1)].bg, t.p.selection_bg);
}

#[test]
fn ansi_selected_row_keeps_dim_text_visible() {
    let ansi = Theme::detect(ThemeChoice::Dark, &|_| None);
    let mut a = app_with(ansi, vec![active(1, "one.iso")]);
    let buf = render_buf(&mut a, 160, 40);
    let (x, y) = find_xy(&buf, "11.4 MiB/s").expect("speed on line 2");
    let cell = &buf[(x, y)];
    assert_ne!(cell.fg, cell.bg, "dim text invisible on the selection");
    assert_ne!(cell.fg, Color::Reset, "still dimmer than normal text");
}

#[test]
fn footer_hints_depend_on_selection() {
    let mut a = app(vec![snap(1, "done.bin", WireState::Completed)]);
    let r = render(&mut a, 160, 40);
    let hints = r.last().unwrap();
    assert!(!hints.contains("p pause"), "{hints:?}");
    assert!(!hints.contains("l limit"), "{hints:?}");
    assert!(hints.contains("d delete"), "{hints:?}");
    assert!(hints.contains("a add"), "{hints:?}");
    assert!(hints.contains("q quit"), "{hints:?}");

    let mut a = app(vec![active(1, "a.iso")]);
    let r = render(&mut a, 160, 40);
    let hints = r.last().unwrap();
    assert!(hints.contains("p pause"), "{hints:?}");
    assert!(!hints.contains("r resume"), "{hints:?}");
    assert!(!hints.contains("R restart"), "{hints:?}");
    let keys: Vec<char> = a
        .hits
        .iter()
        .filter_map(|(_, t)| match t {
            HitTarget::FooterKey(c) => Some(*c),
            _ => None,
        })
        .collect();
    assert_eq!(keys, ['a', 'p', 'd', 'l', '/', '?', 'q']);

    let failed = snap(
        1,
        "f.bin",
        WireState::Failed {
            reason: "x".into(),
            transient: false,
        },
    );
    let mut a = app(vec![failed]);
    let hints = render(&mut a, 160, 40).pop().unwrap();
    assert!(hints.contains("r resume") && hints.contains("R restart"));

    // Narrow: hints that do not fit are dropped from the right, whole.
    let mut a = app(vec![active(1, "a.iso")]);
    let hints = render(&mut a, 60, 20).pop().unwrap();
    assert!(hints.contains("a add"), "{hints:?}");
    assert!(!hints.contains("q qu") || hints.contains("q quit"));
    for (rect, t) in &a.hits {
        if matches!(t, HitTarget::FooterKey(_)) {
            assert!(rect.right() <= 60);
        }
    }
}

#[test]
fn footer_hints_per_mode() {
    let mut a = app(vec![active(1, "a.iso")]);
    a.mode = Mode::Filter;
    let hints = render(&mut a, 160, 40).pop().unwrap();
    assert!(hints.contains("type to filter"), "{hints:?}");
    assert!(hints.contains("Enter keep"), "{hints:?}");
    assert!(hints.contains("Esc clear"), "{hints:?}");
    a.mode = Mode::Popup(Popup::Help);
    let hints = render(&mut a, 160, 40).pop().unwrap();
    assert!(hints.contains("Esc close"), "{hints:?}");
    assert!(!hints.contains("a add"), "{hints:?}");
}

#[test]
fn footer_stats_line() {
    let mut a = app(vec![active(1, "a.iso")]);
    a.totals = Totals {
        down_bps: 14 * MIB + MIB / 2,
        up_bps: 512 * 1024,
        active: 2,
        queued: 1,
        paused: 0,
        completed: 3,
        failed: 0,
    };
    let r = render(&mut a, 160, 40);
    let stats = &r[r.len() - 2];
    for s in [
        "↓ 14.5 MiB/s",
        "↑ 512.0 KiB/s",
        "2 active",
        "1 queued",
        "3 done",
        "limit: none",
        "max 3",
    ] {
        assert!(stats.contains(s), "{s:?} in {stats:?}");
    }
    let order: Vec<usize> = ["↓", "↑", "active", "queued", "done", "limit", "max"]
        .iter()
        .map(|s| stats.find(s).unwrap())
        .collect();
    assert!(order.windows(2).all(|w| w[0] < w[1]), "{stats:?}");
}

#[test]
fn empty_state_hint() {
    let mut a = app(vec![]);
    let r = render(&mut a, 160, 40);
    assert!(
        find(&r, "No downloads yet — press a to add one").is_some(),
        "{r:#?}"
    );
    let hints = r.last().unwrap();
    assert!(hints.contains("a add") && !hints.contains("d delete"));
}

fn http_detail() -> EngineDetail {
    EngineDetail::Http {
        segments: vec![
            SegmentView {
                start: 0,
                end: GIB / 2,
                done: GIB / 2,
                bps: 0,
                active: false,
            },
            SegmentView {
                start: GIB / 2,
                end: GIB,
                done: GIB / 8,
                bps: 3 * MIB,
                active: true,
            },
        ],
        connections: 2,
        resumable: true,
    }
}

fn has_fg_in(buf: &Buffer, color: Color, x0: u16) -> bool {
    let a = buf.area;
    (a.y..a.bottom()).any(|y| (x0..a.right()).any(|x| buf[(x, y)].fg == color))
}

#[test]
fn detail_http_shows_segment_strip() {
    let mut a = app(vec![active(1, "ubuntu.iso")]);
    a.detail = Some((1, http_detail()));
    let t = truecolor();
    let buf = render_buf(&mut a, 160, 40);
    let r = rows(&buf);
    assert!(find(&r, "Segments").is_some(), "{r:#?}");
    assert!(find(&r, "conn 1").is_some(), "{r:#?}");
    // Right of the list (58 % of 160).
    assert!(has_fg_in(&buf, t.p.seg_done, 95));
    assert!(has_fg_in(&buf, t.p.seg_active, 95));
    for key in ["Downloaded", "Speed", "ETA", "State", "Limit", "Source"] {
        assert!(find(&r, key).is_some(), "{key}");
    }
    assert!(find(&r, "461.0 MiB / 1.0 GiB (45%)").is_some(), "{r:#?}");
    // The directory only: the file name is the pane's title.
    assert!(find(&r, "/home/me/Downloads").is_some());
    assert!(find(&r, "/home/me/Downloads/ubuntu.iso").is_none());
}

#[test]
fn detail_torrent_shows_piece_map() {
    let mut d = active(1, "debian.iso");
    d.kind = EngineKindTag::Torrent;
    let mut a = app(vec![d]);
    a.detail = Some((
        1,
        EngineDetail::Torrent {
            piece_count: 64,
            have: vec![0xff, 0xff, 0xff, 0x0f, 0, 0, 0, 0],
            peers: 12,
            seeds: 3,
            leechers: 9,
            ratio: 0.42,
        },
    ));
    let t = truecolor();
    let buf = render_buf(&mut a, 160, 40);
    let r = rows(&buf);
    assert!(find(&r, "Pieces").is_some(), "{r:#?}");
    assert!(find(&r, "12 peers").is_some(), "{r:#?}");
    assert!(find(&r, "0.42").is_some(), "{r:#?}");
    assert!(has_fg_in(&buf, t.p.seg_done, 95));
}

#[test]
fn detail_placeholders() {
    let mut a = app(vec![active(1, "ubuntu.iso")]);
    let r = render(&mut a, 160, 40);
    assert!(find(&r, "No detail yet").is_some(), "{r:#?}");
    a.detail = Some((1, EngineDetail::Unknown));
    let r = render(&mut a, 160, 40);
    assert!(find(&r, "No detail available").is_some(), "{r:#?}");
}

#[test]
fn long_cjk_name_fits() {
    let name = "日本語の非常に長いファイル名です😀".repeat(12) + ".iso";
    let mut d = active(1, &name);
    d.path = Some(format!("/データ/{name}").into());
    d.source = format!("https://例え.jp/{name}");
    let mut a = app(vec![d, active(2, "x.iso")]);
    for (w, h) in [
        (160, 40),
        (161, 40),
        (111, 30),
        (90, 30),
        (61, 20),
        (60, 16),
    ] {
        let r = render(&mut a, w, h);
        for (y, row) in r.iter().enumerate() {
            assert_eq!(row.width(), usize::from(w), "{w}×{h} row {y}: {row:?}");
        }
        assert!(find(&r, "日本語").is_some());
        assert!(find(&r, "…").is_some());
    }
}

#[test]
fn names_are_sanitized() {
    let mut d = active(1, "evil\x1b[31m\tname\r\n.iso");
    d.source = "http://x/\x1b]52;c;bad\x07".into();
    let mut a = app(vec![d]);
    let r = render(&mut a, 160, 40);
    for row in &r {
        assert!(!row.contains(|c: char| c.is_control()), "{row:?}");
    }
    assert!(find(&r, "evil [31m name  .iso").is_some(), "{r:#?}");
}

fn many(n: u64) -> Vec<DownloadSnapshot> {
    (1..=n)
        .map(|i| snap(i, &format!("file-{i:04}.bin"), WireState::Queued))
        .collect()
}

#[test]
fn selection_stays_visible_when_scrolling() {
    let mut a = app(many(300));
    a.selected = Some(250);
    let r = render(&mut a, 120, 40);
    assert!(find(&r, "file-0250.bin").is_some());
    assert!(find(&r, "file-0001.bin").is_none());
    assert!(a.list_offset > 0);
    a.selected = Some(3);
    let r = render(&mut a, 120, 40);
    assert!(find(&r, "file-0003.bin").is_some());
    a.selected = Some(300);
    let r = render(&mut a, 90, 24);
    assert!(find(&r, "file-0300.bin").is_some());
    // Shrinking the terminal keeps the selection on screen too.
    a.selected = Some(150);
    render(&mut a, 160, 60);
    let r = render(&mut a, 80, 24);
    assert!(find(&r, "file-0150.bin").is_some());
}

#[test]
fn hits_cover_rows_and_hints() {
    let mut a = app(many(5));
    render(&mut a, 160, 40);
    let n = a.hits.len();
    render(&mut a, 160, 40);
    assert_eq!(a.hits.len(), n, "hits refilled, not appended");
    let rows: Vec<_> = a
        .hits
        .iter()
        .filter(|(_, t)| matches!(t, HitTarget::ListRow(_)))
        .collect();
    assert_eq!(rows.len(), 5);
    for (rect, _) in &rows {
        assert_eq!(rect.height, 2);
    }
    assert!(matches!(rows[0].1, HitTarget::ListRow(1)));
    assert!(rows[1].0.y >= rows[0].0.bottom());
    let buf = render_buf(&mut a, 160, 40);
    let (rect, _) = a
        .hits
        .iter()
        .find(|(_, t)| *t == HitTarget::FooterKey('q'))
        .unwrap();
    assert_eq!(buf[(rect.x, rect.y)].symbol(), "q");
}

#[test]
fn connection_badges() {
    let mut a = app(vec![active(1, "a.iso")]);
    let r = render(&mut a, 160, 40);
    assert!(r[0].contains("● connected"), "{:?}", r[0]);
    assert!(r[0].contains("daemon v0.1.0"), "{:?}", r[0]);

    a.conn = Conn::Reconnecting { attempt: 3 };
    let t = truecolor();
    let buf = render_buf(&mut a, 160, 40);
    let r = rows(&buf);
    assert!(r[0].contains("◌ reconnecting (3)"), "{:?}", r[0]);
    let (x, y) = find_xy(&buf, "a.iso").unwrap();
    assert_eq!(buf[(x, y)].fg, t.p.dim, "list greyed while reconnecting");

    a.conn = Conn::Connecting;
    a.daemon_version = None;
    let r = render(&mut a, 160, 40);
    assert!(r[0].contains("◌ connecting…"), "{:?}", r[0]);
    assert!(!r[0].contains("daemon v"), "{:?}", r[0]);
}

#[test]
fn filter_shown_and_applied() {
    let mut a = app(many(20));
    a.filter = TextInput::with_value("0017");
    a.mode = Mode::Filter;
    a.selected = Some(17);
    let r = render(&mut a, 160, 40);
    assert!(find(&r, "file-0017.bin").is_some());
    assert!(find(&r, "file-0016.bin").is_none());
    assert!(r[0].contains("0017"), "{:?}", r[0]);
    a.filter = TextInput::with_value("nothing-matches");
    let r = render(&mut a, 160, 40);
    assert!(find(&r, "No matches").is_some(), "{r:#?}");
}

#[test]
fn render_never_panics_any_size() {
    let mut states: Vec<App> = vec![app(vec![]), app(many(40))];
    let mut full = app(vec![active(1, &"長い名前".repeat(30)), active(2, "b.iso")]);
    full.detail = Some((1, http_detail()));
    states.push(full);
    let popups = [
        Popup::Add(AddForm::default()),
        Popup::Limit(LimitForm::new(1, None)),
        Popup::GlobalLimits(GlobalLimitsForm::default()),
        Popup::ConfirmDelete {
            id: 1,
            name: "x".into(),
            delete_files: false,
        },
        Popup::Help,
    ];
    for p in popups {
        let mut a = app(many(3));
        a.mode = Mode::Popup(p);
        states.push(a);
    }
    let mut f = app(many(3));
    f.mode = Mode::Filter;
    f.conn = Conn::Reconnecting { attempt: 9 };
    states.push(f);
    let mono = Theme::detect(ThemeChoice::Dark, &|k| {
        (k == "NO_COLOR").then(|| "1".into())
    });
    states.push(app_with(mono, many(4)));
    for a in &mut states {
        for w in (1..=200).step_by(7) {
            for h in (1..=60).step_by(7) {
                render_buf(a, w, h);
            }
        }
        render_buf(a, 60, 16);
        render_buf(a, 110, 24);
    }
}

#[test]
fn render_500_rows_fast() {
    let mut a = app(many(500));
    a.selected = Some(400);
    render_buf(&mut a, 120, 40);
    let start = Instant::now();
    render_buf(&mut a, 120, 40);
    let took = start.elapsed();
    assert!(took.as_millis() < 200, "{took:?}");
}

#[test]
fn mono_selected_row_reversed() {
    let mono = Theme::detect(ThemeChoice::Dark, &|k| {
        (k == "NO_COLOR").then(|| "1".into())
    });
    let mut a = app_with(mono, vec![active(1, "a.iso"), active(2, "b.iso")]);
    let buf = render_buf(&mut a, 160, 40);
    let (x, y) = find_xy(&buf, "a.iso").unwrap();
    assert!(buf[(x, y)].modifier.contains(Modifier::REVERSED));
    let (x, y) = find_xy(&buf, "b.iso").unwrap();
    assert!(!buf[(x, y)].modifier.contains(Modifier::REVERSED));
}

fn torrent_detail() -> EngineDetail {
    EngineDetail::Torrent {
        piece_count: 64,
        have: vec![0xff, 0xff, 0xff, 0x0f, 0, 0, 0, 0],
        peers: 12,
        seeds: 3,
        leechers: 9,
        ratio: 0.42,
    }
}

#[test]
fn narrow_detail_keeps_engine_section() {
    let t = truecolor();
    for (w, h) in [(90, 30), (80, 24)] {
        let mut a = app(vec![active(1, "ubuntu.iso"), active(2, "b.iso")]);
        a.detail = Some((1, http_detail()));
        let buf = render_buf(&mut a, w, h);
        let r = rows(&buf);
        let head = find(&r, "Segments").unwrap_or_else(|| panic!("{w}×{h}: {r:#?}"));
        let strip = (head + 1) as u16;
        assert!(
            (0..w).any(|x| buf[(x, strip)].fg == t.p.seg_done),
            "{w}×{h} strip: {:?}",
            r[head + 1]
        );

        let mut d = active(1, "debian.iso");
        d.kind = EngineKindTag::Torrent;
        let mut a = app(vec![d, active(2, "b.iso")]);
        a.detail = Some((1, torrent_detail()));
        let buf = render_buf(&mut a, w, h);
        let r = rows(&buf);
        let head = find(&r, "Pieces").unwrap_or_else(|| panic!("{w}×{h}: {r:#?}"));
        let map_rows = (head + 1..r.len())
            .filter(|&y| (0..w).any(|x| buf[(x, y as u16)].fg == t.p.seg_done))
            .count();
        assert!(map_rows >= 1, "{w}×{h}: {r:#?}");
    }
    // 90×30 still has room for the speed history.
    let mut a = app(vec![active(1, "ubuntu.iso")]);
    a.detail = Some((1, http_detail()));
    let r = render(&mut a, 90, 30);
    assert!(find(&r, "speed · last 60 s").is_some(), "{r:#?}");
}

/// The fg of the first full-block bar cell on the list row of `name`.
fn bar_fg(buf: &Buffer, name: &str) -> Color {
    let (x, y) = find_xy_before(buf, name, 90).unwrap();
    (x..90)
        .map(|xx| &buf[(xx, y)])
        .find(|c| c.symbol() == "█")
        .unwrap_or_else(|| panic!("no bar for {name}"))
        .fg
}

#[test]
fn bars_tinted_by_state() {
    let t = truecolor();
    let mut q = snap(2, "queued.bin", WireState::Queued);
    q.queue_position = Some(0);
    let failed = |id, name: &str, transient| {
        snap(
            id,
            name,
            WireState::Failed {
                reason: "x".into(),
                transient,
            },
        )
    };
    let downloads = vec![
        active(1, "active.bin"),
        q,
        snap(3, "paused.bin", WireState::Paused),
        snap(4, "done.bin", WireState::Completed),
        failed(5, "failed.bin", false),
        failed(6, "retry.bin", true),
        snap(
            7,
            "attn.bin",
            WireState::NeedsAttention {
                reason: WireAttention::NotResumable,
            },
        ),
        snap(8, "unknown.bin", WireState::Unknown),
    ];
    let mut a = app(downloads);
    a.selected = None;
    let buf = render_buf(&mut a, 160, 40);
    assert_eq!(bar_fg(&buf, "active.bin"), t.p.bar_done);
    assert_eq!(bar_fg(&buf, "queued.bin"), t.p.dim);
    assert_eq!(bar_fg(&buf, "paused.bin"), t.p.dim);
    assert_eq!(bar_fg(&buf, "done.bin"), t.p.dim);
    assert_eq!(bar_fg(&buf, "failed.bin"), t.p.err);
    assert_eq!(bar_fg(&buf, "retry.bin"), t.p.err);
    assert_eq!(bar_fg(&buf, "attn.bin"), t.p.warn);
    assert_eq!(bar_fg(&buf, "unknown.bin"), t.p.dim);
}

#[test]
fn percent_slot_aligned_outside_bar() {
    let mut half = active(2, "b.iso");
    half.done_bytes = GIB / 2 + 1;
    let mut unknown = active(3, "c.iso");
    unknown.total_bytes = None;
    let done = snap(4, "d.iso", WireState::Completed);
    let mut a = app(vec![active(1, "a.iso"), half, unknown, done]);
    let r = render(&mut a, 160, 40);
    let ends: Vec<usize> = [
        ("45%", "a.iso"),
        ("50%", "b.iso"),
        ("—", "c.iso"),
        ("100%", "d.iso"),
    ]
    .iter()
    .map(|(pct, name)| {
        let row = &r[find(&r, name).unwrap()];
        let list: String = row.chars().take(92).collect();
        let at = list
            .find(pct)
            .unwrap_or_else(|| panic!("{pct} in {list:?}"));
        list[..at + pct.len()].chars().count()
    })
    .collect();
    assert!(ends.windows(2).all(|w| w[0] == w[1]), "{ends:?}");
    // The percent is after the bar, not inside it.
    let row = &r[find(&r, "b.iso").unwrap()];
    assert!(
        row.rfind('█').unwrap() < row.find("50%").unwrap(),
        "{row:?}"
    );
}

#[test]
fn selection_band_spans_bar_track_with_accent_edge() {
    let mut d = snap(1, "a.iso", WireState::Paused);
    d.done_bytes = 0;
    let mut a = app(vec![d, active(2, "b.iso")]);
    let t = truecolor();
    let buf = render_buf(&mut a, 160, 40);
    let (x, y) = find_xy_before(&buf, "a.iso", 90).unwrap();
    for xx in x + 10..88 {
        assert_eq!(buf[(xx, y)].bg, t.p.selection_bg, "x={xx}");
    }
    for yy in [y, y + 1] {
        assert_eq!(buf[(1, yy)].symbol(), "▌");
        assert_eq!(buf[(1, yy)].fg, t.p.border_focus);
    }
    assert_ne!(buf[(1, y + 2)].symbol(), "▌");
}

#[test]
fn odd_height_shows_partial_item() {
    let mut a = app(many(30));
    let r = render(&mut a, 120, 24);
    let inner = 24 - 3 - 2; // title + footer, list borders
    assert_eq!(inner % 2, 1);
    let full = inner / 2;
    assert!(
        find(&r, &format!("file-{:04}.bin", full + 1)).is_some(),
        "{r:#?}"
    );
    assert!(find(&r, &format!("file-{:04}.bin", full + 2)).is_none());
    let (rect, _) = a
        .hits
        .iter()
        .find(|(_, t)| *t == HitTarget::ListRow(full as u64 + 1))
        .unwrap();
    assert_eq!(rect.height, 1);
}

#[test]
fn second_line_drops_whole_parts() {
    let mut d = active(1, "a.iso");
    d.limit = Some(20 * MIB);
    let mut a = app(vec![d]);
    a.totals.failed = 2;
    a.totals.paused = 1;
    for w in [60u16, 70, 80, 90, 100] {
        let r = render(&mut a, w, 30);
        let row = &r[find(&r, "a.iso").unwrap() + 1];
        assert!(!row.contains('…'), "{w}: {row:?}");
        assert!(row.contains("11.4 MiB/s"), "{w}: {row:?}");
        let stats = &r[r.len() - 2];
        assert!(!stats.contains('…'), "{w}: {stats:?}");
    }
    let mut t = active(2, "t.iso");
    t.kind = EngineKindTag::Torrent;
    t.up_bps = 1024;
    let mut a = app(vec![t]);
    let r = render(&mut a, 160, 40);
    let row = &r[find(&r, "t.iso").unwrap() + 1];
    let down = row.find("↓ 11.4 MiB/s").unwrap();
    let up = row.find("↑ 1.0 KiB/s").unwrap();
    let eta = row.find("0:41").unwrap();
    assert!(down < up && up < eta, "{row:?}");
}

#[test]
fn source_middle_ellipsis_keeps_filename() {
    let mut d = active(1, "file.iso");
    d.source = format!(
        "https://mirror.example.org/{}the-file-name.iso",
        "deep/".repeat(30)
    );
    let mut a = app(vec![d]);
    let r = render(&mut a, 160, 40);
    let row = &r[find(&r, "Source").unwrap()];
    assert!(row.contains("https://"), "{row:?}");
    assert!(
        row.contains('…') && row.contains("the-file-name.iso"),
        "{row:?}"
    );
}

/// Prints a realistic dashboard at a few sizes, for eyeballing:
/// `cargo test -p puppy-tui --test render demo_render_print -- --ignored --nocapture`.
#[test]
#[ignore]
fn demo_render_print() {
    let mut q = snap(3, "archlinux-2026.11.01-x86_64.iso", WireState::Queued);
    q.queue_position = Some(0);
    let mut tor = active(2, "debian-13.2.0-amd64-DVD-1.iso");
    tor.kind = EngineKindTag::Torrent;
    tor.peers = Some(34);
    tor.up_bps = 400 * 1024;
    let mut retry = snap(
        5,
        "big-dataset.tar.zst",
        WireState::Failed {
            reason: "connection reset by peer".into(),
            transient: true,
        },
    );
    retry.next_retry_unix = Some(1083);
    let mut list = vec![
        active(1, "ubuntu-24.04.3-desktop-amd64.iso"),
        tor,
        q,
        snap(4, "Big Buck Bunny 4K.mkv", WireState::Completed),
        retry,
        snap(
            6,
            "photos.zip",
            WireState::NeedsAttention {
                reason: WireAttention::RemoteChanged,
            },
        ),
        snap(7, "notes.pdf", WireState::Paused),
    ];
    list[0].limit = Some(20 * MIB);
    let mut a = app(list);
    a.now_unix = 1000;
    a.detail = Some((1, http_detail()));
    a.totals = Totals {
        down_bps: 14 * MIB,
        up_bps: 400 * 1024,
        active: 2,
        queued: 1,
        paused: 1,
        completed: 1,
        failed: 2,
    };
    let mut h = puppy_tui::app::History::default();
    let t0 = Instant::now();
    for i in 0..80u64 {
        let mut d = a.downloads.clone();
        d[0].down_bps = (5 + (i * 7 % 13)) * MIB;
        let totals = Totals {
            down_bps: (8 + i % 9) * MIB,
            ..Default::default()
        };
        h.record(&d, &totals, t0 + std::time::Duration::from_secs(i));
    }
    a.history = h;
    for (w, hh) in [(120u16, 32u16), (90, 30), (80, 24)] {
        println!("--- {w}×{hh}");
        for r in render(&mut a, w, hh) {
            println!("{r}");
        }
    }

    // Popups at 100×30 (and a toast).
    let mut add = AddForm::with_source("htps://example.com/file.iso");
    add.error = Some(puppy_tui::app::forms::ADD_BAD_SOURCE.into());
    let popups = [
        ("add popup with an inline error", Popup::Add(add)),
        (
            "confirm delete",
            Popup::ConfirmDelete {
                id: 6,
                name: "photos.zip".into(),
                delete_files: true,
            },
        ),
        ("help", Popup::Help),
        ("limit", Popup::Limit(LimitForm::new(1, Some(20 * MIB)))),
        ("global limits", {
            let mut g = GlobalLimitsForm::new(Some(2 * MIB), None);
            g.focus = 1;
            g.up = TextInput::with_value("fast");
            g.error_field = 1;
            g.error = puppy_tui::format::parse_rate("fast").err();
            Popup::GlobalLimits(g)
        }),
    ];
    let now = Instant::now();
    a.push_toast(
        "big-dataset.tar.zst failed: connection reset by peer",
        puppy_tui::app::ToastKind::Error,
        now,
    );
    for (what, p) in popups {
        a.mode = Mode::Popup(p);
        println!("--- 100×30 {what}");
        for r in render(&mut a, 100, 30) {
            println!("{r}");
        }
        println!("--- 60×16 {what}");
        for r in render(&mut a, 60, 16) {
            println!("{r}");
        }
    }
}

#[test]
fn filter_cursor_never_on_ellipsis() {
    let mut a = app(many(3));
    a.mode = Mode::Filter;
    a.filter = TextInput::with_value(&"x".repeat(200));
    let mut term = Terminal::new(TestBackend::new(80, 24)).unwrap();
    term.draw(|f| draw(&mut a, f)).unwrap();
    let pos = term.get_cursor_position().unwrap();
    let buf = term.backend().buffer();
    assert_eq!(pos.y, 0);
    assert!(pos.x < 80);
    assert_ne!(buf[(pos.x, 0)].symbol(), "…");
    assert_eq!(buf[(pos.x - 1, 0)].symbol(), "…");
}

// ---------------------------------------------------------------------------------
// Final fix wave

#[test]
fn detail_placeholder_reserves_the_http_height() {
    // The sections above the engine part do not move when the detail arrives.
    let detail = EngineDetail::Http {
        segments: Vec::new(),
        connections: 1,
        resumable: true,
    };
    for (w, h) in [(80u16, 24u16), (80, 30), (120, 24)] {
        let mut a = app(vec![active(1, "ubuntu.iso")]);
        let before = render(&mut a, w, h);
        a.detail = Some((1, detail.clone()));
        let after = render(&mut a, w, h);
        let head = |r: &[String]| find(r, "speed · last 60 s");
        if h >= 30 {
            assert!(head(&before).is_some(), "{w}×{h}: {before:#?}");
        }
        assert_eq!(head(&before), head(&after), "{w}×{h}");
        assert_eq!(
            find(&before, "No detail yet"),
            find(&after, "Segments"),
            "{w}×{h}"
        );
    }
}

#[test]
fn truecolor_paints_the_background_everywhere() {
    for theme in [
        truecolor(),
        Theme::detect(ThemeChoice::Light, &|k| {
            (k == "COLORTERM").then(|| "truecolor".to_string())
        }),
    ] {
        // An empty dashboard: every cell but the title badge is on the palette's bg.
        let mut a = app_with(theme, vec![]);
        let buf = render_buf(&mut a, 100, 30);
        let badge = theme.style(puppy_tui::theme::Token::Badge).bg;
        for c in buf.content() {
            assert!(c.bg == theme.p.bg || Some(c.bg) == badge, "{c:?}");
        }
        // With popups and toasts (which clear their area): no cell is left on the
        // terminal's own background.
        let mut a = app_with(theme, many(5));
        a.mode = Mode::Popup(Popup::Help);
        a.push_toast("hello", puppy_tui::app::ToastKind::Info, Instant::now());
        for (w, h) in [(120, 32), (80, 24), (40, 10)] {
            let buf = render_buf(&mut a, w, h);
            for c in buf.content() {
                assert_ne!(c.bg, Color::Reset, "{w}×{h} {c:?}");
                assert_ne!(c.fg, Color::Reset, "{w}×{h} {c:?}");
            }
        }
    }
    // ANSI and mono keep the terminal's background.
    let ansi = Theme::detect(ThemeChoice::Dark, &|_| None);
    let mut a = app_with(ansi, vec![]);
    let buf = render_buf(&mut a, 100, 30);
    assert!(buf.content().iter().any(|c| c.bg == Color::Reset));
}

#[test]
fn disconnected_dims_footer_and_detail_and_hides_speeds() {
    let mut a = app(vec![active(1, "ubuntu.iso")]);
    a.totals.down_bps = 5 * MIB;
    a.conn = Conn::Reconnecting { attempt: 1 };
    let buf = render_buf(&mut a, 120, 32);
    let r = rows(&buf);
    let stats = &r[r.len() - 2];
    assert!(stats.contains("↓ —") && stats.contains("↑ —"), "{stats:?}");
    assert!(!stats.contains("MiB/s"), "{stats:?}");
    let dim = a.theme.p.dim;
    let y = (r.len() - 2) as u16;
    for x in 0..120 {
        let c = &buf[(x, y)];
        if c.symbol() != " " {
            assert_eq!(c.fg, dim, "stats x={x} {c:?}");
        }
    }
    // The detail pane: speed and ETA unknown, all dim.
    let (dx, dy) = find_xy(&buf, "Downloaded").unwrap();
    let speed_row = &r[usize::from(dy) + 1];
    let eta_row = &r[usize::from(dy) + 2];
    assert!(speed_row.contains("Speed      —"), "{speed_row:?}");
    assert!(eta_row.contains("ETA        —"), "{eta_row:?}");
    for y in dy..dy + 4 {
        for x in dx..119 {
            let c = &buf[(x, y)];
            if c.symbol() != " " && c.symbol() != "│" && c.symbol() != "╯" {
                assert_eq!(c.fg, dim, "detail ({x},{y}) {c:?}");
            }
        }
    }
}

#[test]
fn footer_offers_clearing_an_active_filter() {
    let mut a = app(vec![active(1, "ubuntu.iso")]);
    let hints = render(&mut a, 160, 40).pop().unwrap();
    assert!(!hints.contains("Esc clear filter"), "{hints:?}");
    a.filter = TextInput::with_value("ubu");
    let hints = render(&mut a, 160, 40).pop().unwrap();
    assert!(hints.contains("Esc clear filter"), "{hints:?}");
    assert!(
        a.hits
            .iter()
            .any(|(_, t)| *t == HitTarget::FooterKey('\u{1b}'))
    );
}
