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
    assert!(find(&r, "conn 2").is_some(), "{r:#?}");
    // Right of the list (58 % of 160).
    assert!(has_fg_in(&buf, t.p.seg_done, 95));
    assert!(has_fg_in(&buf, t.p.seg_active, 95));
    for key in [
        "Size",
        "Downloaded",
        "Speed",
        "ETA",
        "State",
        "Limit",
        "Source",
    ] {
        assert!(find(&r, key).is_some(), "{key}");
    }
    assert!(find(&r, "/home/me/Downloads/ubuntu.iso").is_some());
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
        Popup::Limit(LimitForm::new(1)),
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
    assert!(took.as_millis() < 50, "{took:?}");
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
