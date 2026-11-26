//! Popups (add, limit, global limits, confirm delete, help) and toasts: rendering,
//! validation and submission.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use puppy_core::types::{DownloadId, Source};
use puppy_proto::{DownloadSnapshot, EngineKindTag, Settings, WireState};
use puppy_tui::app::forms::{ADD_BAD_DIR, ADD_BAD_SOURCE, parse_source};
use puppy_tui::app::{
    AddField, AddForm, App, Cmd, Conn, GlobalLimitsForm, HitTarget, LimitForm, Mode, Msg, Popup,
    TextInput, ToastKind,
};
use puppy_tui::theme::{Theme, ThemeChoice, Token};
use puppy_tui::ui::draw;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::layout::{Position, Rect};
use unicode_width::UnicodeWidthStr;

const MIB: u64 = 1024 * 1024;

fn theme() -> Theme {
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
        path: None,
        state,
        total_bytes: Some(1000),
        done_bytes: 0,
        down_bps: 0,
        up_bps: 0,
        eta_secs: None,
        limit: None,
        queue_position: None,
        connections: None,
        peers: None,
        next_retry_unix: None,
        note: None,
        seed: None,
        checksum: None,
    }
}

fn settings() -> Settings {
    Settings {
        max_active: 3,
        global_down_limit: Some(2 * MIB),
        global_up_limit: None,
        download_dir: "/srv/downloads".into(),
        connections: 8,
        theme: "auto".into(),
    }
}

fn connected(downloads: Vec<DownloadSnapshot>) -> (App, Instant) {
    let mut a = App::new(theme());
    let now = Instant::now();
    a.update(
        Msg::Connected {
            version: "0.1.0".into(),
            settings: settings(),
            downloads,
        },
        now,
    );
    assert_eq!(a.conn, Conn::Connected);
    (a, now)
}

fn key(code: KeyCode) -> Msg {
    Msg::Key(KeyEvent::new(code, KeyModifiers::NONE))
}

fn ch(c: char) -> Msg {
    key(KeyCode::Char(c))
}

fn typed(a: &mut App, s: &str, now: Instant) {
    for c in s.chars() {
        a.update(ch(c), now);
    }
}

fn render_term(app: &mut App, w: u16, h: u16) -> Terminal<TestBackend> {
    let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
    term.draw(|f| draw(app, f)).unwrap();
    term
}

fn render_buf(app: &mut App, w: u16, h: u16) -> Buffer {
    render_term(app, w, h).backend().buffer().clone()
}

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

/// (x, y) of the first cell where `needle` (ASCII) starts.
fn find_xy(buf: &Buffer, needle: &str) -> Option<(u16, u16)> {
    let a = buf.area;
    for y in a.y..a.bottom() {
        for x in a.x..a.right() {
            let ok = needle.chars().enumerate().all(|(i, c)| {
                let xx = x + i as u16;
                xx < a.right() && buf[(xx, y)].symbol() == c.to_string()
            });
            if ok {
                return Some((x, y));
            }
        }
    }
    None
}

fn popup_rect(a: &App) -> Rect {
    a.hits
        .iter()
        .find(|(_, t)| *t == HitTarget::PopupInside)
        .map(|(r, _)| *r)
        .expect("popup hit region")
}

fn add_form(a: &App) -> &AddForm {
    match &a.mode {
        Mode::Popup(Popup::Add(f)) => f,
        m => panic!("expected add popup, got {m:?}"),
    }
}

fn temp_torrent(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("puppy-tui-popups-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let p = dir.join(name);
    std::fs::write(&p, b"d4:infoe").unwrap();
    p
}

#[test]
fn add_popup_renders_fields() {
    let (mut a, now) = connected(vec![]);
    a.update(ch('a'), now);
    let mut term = render_term(&mut a, 100, 30);
    let buf = term.backend().buffer().clone();
    let r = rows(&buf);
    let top = find(&r, "Add download").expect("title");
    assert!(r[top].contains('╭'), "{:?}", r[top]);
    let src = find(&r, "URL / magnet / .torrent").expect("source label");
    let dir = find(&r, "Save to").expect("dir label");
    assert!(src < dir);
    // Placeholder: the daemon's default folder, dimmed.
    let (px, py) = find_xy(&buf, "/srv/downloads").expect("placeholder");
    assert!(py as usize > dir);
    assert_eq!(buf[(px, py)].fg, theme().style(Token::Dim).fg.unwrap());
    let cb = find(&r, "[ ] start paused").expect("checkbox");
    assert!(cb > py as usize);
    assert!(find(&r, "Add ]").is_some_and(|b| b > cb), "{r:#?}");
    // Centered, width min(70, w−4).
    let rect = popup_rect(&a);
    assert_eq!(rect.width, 70);
    assert_eq!(rect.x, 15);
    assert!(
        a.hits
            .iter()
            .any(|(r, t)| *t == HitTarget::PopupCheckbox && r.y == cb as u16)
    );
    // The cursor sits in the source input.
    let pos = term.get_cursor_position().unwrap();
    assert!(term.backend().cursor_visible());
    assert!(pos.y > src as u16 && pos.y < dir as u16, "{pos:?}");
    // Footer hints come from the popup.
    let hints = r.last().unwrap();
    for h in ["Tab next", "Enter add", "Esc cancel"] {
        assert!(hints.contains(h), "{hints:?}");
    }
    assert!(!hints.contains("q quit"), "{hints:?}");
}

#[test]
fn add_submit_valid_url_emits_cmd() {
    let (mut a, now) = connected(vec![]);
    a.update(ch('a'), now);
    typed(&mut a, "https://example.com/f.iso", now);
    let cmds = a.update(key(KeyCode::Enter), now);
    match cmds.as_slice() {
        [
            Cmd::Add {
                request,
                start_paused: false,
            },
        ] => {
            assert_eq!(
                request.source,
                Source::Http {
                    url: "https://example.com/f.iso".parse().unwrap()
                }
            );
            // Empty "Save to" ⇒ the daemon's default.
            assert_eq!(request.output_dir, None);
        }
        c => panic!("unexpected {c:?}"),
    }
    assert_eq!(a.mode, Mode::Normal);
}

#[test]
fn invalid_source_inline_error() {
    let (mut a, now) = connected(vec![]);
    a.update(ch('a'), now);
    typed(&mut a, "ftp://nope/x", now);
    a.update(key(KeyCode::Tab), now);
    assert!(a.update(key(KeyCode::Enter), now).is_empty());
    let f = add_form(&a);
    assert_eq!(f.error.as_deref(), Some(ADD_BAD_SOURCE));
    assert_eq!(f.error_on, AddField::Source);
    // Focus returns to the field at fault.
    assert_eq!(f.focus, AddField::Source);
    let buf = render_buf(&mut a, 100, 30);
    let r = rows(&buf);
    let label = find(&r, "URL / magnet / .torrent").unwrap();
    let dir = find(&r, "Save to").unwrap();
    let (ex, ey) = find_xy(&buf, ADD_BAD_SOURCE).expect("inline error");
    assert!(ey as usize > label && (ey as usize) < dir, "{r:#?}");
    assert_eq!(buf[(ex, ey)].fg, theme().style(Token::Err).fg.unwrap());
    // Editing clears the error.
    a.update(key(KeyCode::Backspace), now);
    assert_eq!(add_form(&a).error, None);
}

#[test]
fn add_dir_must_be_absolute() {
    let (mut a, now) = connected(vec![]);
    a.update(ch('a'), now);
    typed(&mut a, "magnet:?xt=urn:btih:abc", now);
    a.update(key(KeyCode::Tab), now);
    typed(&mut a, "relative/dir", now);
    a.update(key(KeyCode::Tab), now);
    assert!(a.update(key(KeyCode::Enter), now).is_empty());
    let f = add_form(&a);
    assert_eq!(f.error.as_deref(), Some(ADD_BAD_DIR));
    assert_eq!((f.error_on, f.focus), (AddField::Dir, AddField::Dir));
    let r = render(&mut a, 100, 30);
    let err = find(&r, ADD_BAD_DIR).expect("dir error");
    assert!(err > find(&r, "Save to").unwrap());
    assert!(err < find(&r, "start paused").unwrap());

    // `~` expands to the home directory.
    let mut f = AddForm::with_source("magnet:?xt=urn:btih:abc");
    f.dir = TextInput::with_value("~/Movies");
    match f.submit() {
        Some(Cmd::Add { request, .. }) => {
            let dir = request.output_dir.unwrap();
            assert!(dir.is_absolute(), "{dir:?}");
            assert!(dir.ends_with("Movies"));
            assert!(!dir.to_string_lossy().contains('~'));
        }
        c => panic!("unexpected {c:?}"),
    }
}

#[test]
fn paste_goes_to_focused_field() {
    let (mut a, now) = connected(vec![]);
    a.update(ch('a'), now);
    a.update(Msg::Paste("https://x.org/a.iso".into()), now);
    a.update(key(KeyCode::Tab), now);
    a.update(Msg::Paste("/data/isos\n".into()), now);
    let f = add_form(&a);
    assert_eq!(f.source.value, "https://x.org/a.iso");
    assert_eq!(f.dir.value, "/data/isos");

    // Global limits: into the focused field.
    a.update(key(KeyCode::Esc), now);
    a.update(ch('L'), now);
    a.update(key(KeyCode::Tab), now);
    a.update(Msg::Paste("300k".into()), now);
    match &a.mode {
        Mode::Popup(Popup::GlobalLimits(f)) => assert_eq!(f.up.value, "300k"),
        m => panic!("{m:?}"),
    }
}

#[test]
fn tab_cycles_focus() {
    let (mut a, now) = connected(vec![]);
    a.update(ch('a'), now);
    let mut seen = vec![add_form(&a).focus];
    for _ in 0..5 {
        a.update(key(KeyCode::Tab), now);
        seen.push(add_form(&a).focus);
    }
    assert_eq!(
        seen,
        [
            AddField::Source,
            AddField::Dir,
            AddField::Checksum,
            AddField::Paused,
            AddField::Ok,
            AddField::Source
        ]
    );
    a.update(key(KeyCode::BackTab), now);
    assert_eq!(add_form(&a).focus, AddField::Ok);
    // Space toggles the checkbox only when it has focus.
    a.update(key(KeyCode::BackTab), now);
    a.update(ch(' '), now);
    assert!(add_form(&a).start_paused);
    let r = render(&mut a, 100, 30);
    assert!(find(&r, "[x] start paused").is_some(), "{r:#?}");
    // Enter on any field submits.
    a.update(key(KeyCode::BackTab), now);
    a.update(key(KeyCode::BackTab), now);
    a.update(key(KeyCode::BackTab), now);
    typed(&mut a, "https://x.org/a", now);
    a.update(key(KeyCode::Tab), now);
    let cmds = a.update(key(KeyCode::Enter), now);
    assert!(
        matches!(
            cmds.as_slice(),
            [Cmd::Add {
                start_paused: true,
                ..
            }]
        ),
        "{cmds:?}"
    );
}

#[test]
fn limit_popup_parses_and_emits() {
    let mut d = snap(5, "ubuntu.iso", WireState::Active);
    d.limit = Some(MIB);
    let (mut a, now) = connected(vec![d]);
    a.update(ch('l'), now);
    // Prefilled with the current limit.
    match &a.mode {
        Mode::Popup(Popup::Limit(f)) => assert_eq!(f.input.value, "1.0M"),
        m => panic!("{m:?}"),
    }
    let r = render(&mut a, 100, 30);
    assert!(find(&r, "ubuntu.iso").is_some());
    assert!(find(&r, "e.g. 2M, 500k, none").is_some(), "{r:#?}");
    let hints = r.last().unwrap();
    assert!(hints.contains("Enter set") && hints.contains("Esc cancel"));
    // Ctrl+U clears, then a new value.
    a.update(
        Msg::Key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL)),
        now,
    );
    typed(&mut a, "500k", now);
    assert_eq!(
        a.update(key(KeyCode::Enter), now),
        vec![Cmd::SetLimit {
            id: 5,
            bps: Some(500 * 1024)
        }]
    );
    // No limit ⇒ empty input; empty submits "none".
    let (mut a, now) = connected(vec![snap(6, "b", WireState::Active)]);
    a.update(ch('l'), now);
    match &a.mode {
        Mode::Popup(Popup::Limit(f)) => assert!(f.input.is_empty()),
        m => panic!("{m:?}"),
    }
    assert_eq!(
        a.update(key(KeyCode::Enter), now),
        vec![Cmd::SetLimit { id: 6, bps: None }]
    );
}

#[test]
fn limit_invalid_shows_error() {
    let (mut a, now) = connected(vec![snap(5, "a", WireState::Active)]);
    a.update(ch('l'), now);
    typed(&mut a, "fast", now);
    assert!(a.update(key(KeyCode::Enter), now).is_empty());
    let err = match &a.mode {
        Mode::Popup(Popup::Limit(f)) => f.error.clone().expect("error"),
        m => panic!("{m:?}"),
    };
    let buf = render_buf(&mut a, 100, 30);
    let r = rows(&buf);
    // Long errors wrap inside the popup; the start is visible in Err style.
    let head: String = err.chars().take(20).collect();
    let (x, y) = find_xy(&buf, &head).expect("error shown");
    assert_eq!(buf[(x, y)].fg, theme().style(Token::Err).fg.unwrap());
    let rect = popup_rect(&a);
    assert!(
        x >= rect.x && x < rect.right() && y < rect.bottom(),
        "{r:#?}"
    );
}

#[test]
fn global_limits_two_fields() {
    let (mut a, now) = connected(vec![]);
    a.update(ch('L'), now);
    // Prefilled from the settings (down 2 MiB, up none).
    match &a.mode {
        Mode::Popup(Popup::GlobalLimits(f)) => {
            assert_eq!(f.down.value, "2.0M");
            assert_eq!(f.up.value, "");
            assert_eq!(f.focus, 0);
        }
        m => panic!("{m:?}"),
    }
    let r = render(&mut a, 100, 30);
    let down = find(&r, "Download").expect("down label");
    let up = find(&r, "Upload").expect("up label");
    assert!(down < up);
    assert!(find(&r, "e.g. 2M, 500k, none").is_some());
    a.update(key(KeyCode::Tab), now);
    typed(&mut a, "bogus", now);
    assert!(a.update(key(KeyCode::Enter), now).is_empty());
    match &a.mode {
        Mode::Popup(Popup::GlobalLimits(f)) => {
            assert!(f.error.is_some());
            assert_eq!((f.error_field, f.focus), (1, 1));
        }
        m => panic!("{m:?}"),
    }
    a.update(
        Msg::Key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL)),
        now,
    );
    typed(&mut a, "100k", now);
    assert_eq!(
        a.update(key(KeyCode::Enter), now),
        vec![Cmd::SetGlobalLimits {
            down: Some(2 * MIB),
            up: Some(100 * 1024)
        }]
    );
}

#[test]
fn confirm_delete_toggle_files() {
    let (mut a, now) = connected(vec![snap(9, "photos.zip", WireState::Paused)]);
    a.update(ch('d'), now);
    let r = render(&mut a, 100, 30);
    assert!(find(&r, "Delete photos.zip?").is_some(), "{r:#?}");
    let cb = find(&r, "[ ] also delete files").expect("checkbox");
    assert!(
        a.hits
            .iter()
            .any(|(r, t)| *t == HitTarget::PopupCheckbox && r.y == cb as u16)
    );
    let hints = r.last().unwrap();
    assert!(hints.contains("Enter delete") && hints.contains("Esc cancel"));
    a.update(ch('f'), now);
    assert!(find(&render(&mut a, 100, 30), "[x] also delete files").is_some());
    a.update(ch(' '), now);
    a.update(ch(' '), now);
    assert_eq!(
        a.update(key(KeyCode::Enter), now),
        vec![Cmd::Remove {
            id: 9,
            delete_files: true
        }]
    );
    // Esc cancels.
    a.update(ch('d'), now);
    assert!(a.update(key(KeyCode::Esc), now).is_empty());
    assert_eq!(a.mode, Mode::Normal);
}

#[test]
fn help_lists_all_keys() {
    let (mut a, now) = connected(vec![]);
    a.update(ch('?'), now);
    let r = render(&mut a, 100, 60);
    let all = r.join("\n");
    for k in [
        "j",
        "↓",
        "k",
        "↑",
        "g",
        "Home",
        "G",
        "End",
        "a",
        "p",
        "r",
        "d",
        "Delete",
        "R",
        "l",
        "L",
        "+",
        "-",
        "Shift+↑",
        "Shift+↓",
        "K",
        "J",
        "/",
        "Esc",
        "?",
        "q",
        "Ctrl+C",
    ] {
        // Keys are listed in the key column: preceded by two spaces or a separator.
        assert!(
            r.iter().any(|row| row.contains(&format!(" {k} "))
                || row.contains(&format!(" {k},"))
                || row.contains(&format!("/{k} "))),
            "missing key {k:?} in\n{all}"
        );
    }
    for m in ["click", "double-click", "wheel"] {
        assert!(all.contains(m), "missing {m:?}");
    }
}

#[test]
fn help_scrolls_when_taller_than_screen() {
    let (mut a, now) = connected(vec![]);
    a.update(ch('?'), now);
    let first = render(&mut a, 80, 24);
    assert!(first.join("\n").contains("down"));
    for _ in 0..200 {
        a.update(ch('j'), now);
    }
    let last = render(&mut a, 80, 24);
    assert_ne!(first, last);
    // Scrolled to the end: the last section (mouse) shows.
    assert!(last.join("\n").contains("wheel"), "{last:#?}");
    for _ in 0..200 {
        a.update(ch('k'), now);
    }
    assert_eq!(render(&mut a, 80, 24), first);
    // `j` scrolls rather than closing; `q` closes.
    a.update(ch('q'), now);
    assert_eq!(a.mode, Mode::Normal);
}

#[test]
fn popup_clamped_on_small_screen() {
    let mut d = snap(1, &"x".repeat(200), WireState::Active);
    d.limit = Some(3 * MIB);
    let popups = [
        Popup::Add(AddForm {
            error: Some(ADD_BAD_SOURCE.into()),
            ..AddForm::default()
        }),
        Popup::Limit(LimitForm::new(1, Some(3 * MIB))),
        Popup::GlobalLimits(GlobalLimitsForm::default()),
        Popup::ConfirmDelete {
            id: 1,
            name: "y".repeat(300),
            delete_files: true,
        },
        Popup::Help,
    ];
    for p in popups {
        let (mut a, _) = connected(vec![d.clone()]);
        a.mode = Mode::Popup(p.clone());
        let r = render(&mut a, 60, 16);
        let rect = popup_rect(&a);
        assert!(rect.x >= 1 && rect.right() <= 59, "{p:?} {rect:?}");
        assert!(rect.bottom() <= 16, "{p:?} {rect:?}");
        assert_eq!(rect.width, 56);
        // Rounded corners at the rect's edges: fully drawn.
        let top: Vec<char> = r[rect.y as usize].chars().collect();
        assert_eq!(top[rect.x as usize], '╭', "{p:?}\n{r:#?}");
        let bottom: Vec<char> = r[rect.bottom() as usize - 1].chars().collect();
        assert_eq!(bottom[rect.right() as usize - 1], '╯', "{p:?}\n{r:#?}");
        // The footer hints stay visible.
        assert!(r[15].contains("Esc"), "{p:?}\n{r:#?}");
    }
    // Add: the error and every field still fit.
    let (mut a, now) = connected(vec![]);
    a.update(ch('a'), now);
    a.update(key(KeyCode::Enter), now);
    let r = render(&mut a, 60, 16);
    for s in [
        "URL / magnet",
        "enter a URL",
        "Save to",
        "start paused",
        "Add ]",
    ] {
        assert!(find(&r, s).is_some(), "{s:?}\n{r:#?}");
    }
}

#[test]
fn toasts_stack_max_three() {
    let (mut a, now) = connected(vec![]);
    for i in 0..5 {
        a.push_toast(format!("toast number {i}"), ToastKind::Info, now);
    }
    a.push_toast(
        "error: the daemon refused the request because it was rather long and wordy indeed, much longer than fits",
        ToastKind::Error,
        now,
    );
    assert_eq!(a.toasts.len(), 3);
    let buf = render_buf(&mut a, 100, 30);
    let r = rows(&buf);
    // The oldest are dropped: 3, 4 and the error remain, newest at the bottom.
    assert!(find(&r, "toast number 2").is_none(), "dropped: {r:#?}");
    let t3 = find(&r, "toast number 3").expect("toast 3");
    let t4 = find(&r, "toast number 4").expect("toast 4");
    assert!(t3 < t4);
    let te = find(&r, "error: the daemon").expect("error toast");
    // Newest at the bottom, above the footer (rows 28–29).
    assert!(t4 < te);
    assert!(te + 1 < 28, "{r:#?}");
    // Wrapped to at most two lines, ending with an ellipsis.
    assert!(r[te + 1].contains('…') || r[te + 1].contains("refused"));
    // Right-aligned and at most 50 wide.
    let (x4, _) = find_xy(&buf, "toast number 4").unwrap();
    assert!(x4 >= 50, "{x4}");
    assert!(r[te].trim_end().ends_with('│'), "{:?}", r[te]);
    let (xe, ye) = find_xy(&buf, "error: the daemon").unwrap();
    assert_eq!(buf[(xe, ye)].fg, theme().style(Token::Err).fg.unwrap());
    let (xi, yi) = find_xy(&buf, "toast number 4").unwrap();
    assert_eq!(buf[(xi, yi)].fg, theme().style(Token::Info).fg.unwrap());
    // Footer untouched.
    assert!(r[29].contains("a add"), "{:?}", r[29]);
    // Expiry.
    a.update(Msg::Tick, now + Duration::from_secs(10));
    assert!(a.toasts.is_empty());
    assert!(find(&render(&mut a, 100, 30), "toast number").is_none());
    // Too small for toasts: nothing drawn, no panic.
    a.push_toast("tiny", ToastKind::Info, now + Duration::from_secs(10));
    render(&mut a, 60, 16);
}

#[test]
fn parse_source_variants() {
    assert!(matches!(
        parse_source("https://example.com/a.iso"),
        Ok(Source::Http { .. })
    ));
    assert!(matches!(
        parse_source("  HTTP://Example.com/a  "),
        Ok(Source::Http { .. })
    ));
    assert_eq!(
        parse_source("magnet:?xt=urn:btih:abc"),
        Ok(Source::Magnet {
            uri: "magnet:?xt=urn:btih:abc".into()
        })
    );
    assert!(matches!(
        parse_source("\"MAGNET:?xt=urn:btih:abc\""),
        Ok(Source::Magnet { .. })
    ));
    let t = temp_torrent("ubuntu.TORRENT");
    match parse_source(&format!("'{}'", t.display())) {
        Ok(Source::TorrentFile { path }) => {
            assert!(path.is_absolute());
            assert_eq!(path.file_name(), t.file_name());
        }
        r => panic!("{r:?}"),
    }
    let missing = t.with_file_name("missing.torrent");
    for bad in [
        "ftp://x/a",
        "hello",
        "magnet:xt",
        "",
        "https://",
        &missing.display().to_string(),
    ] {
        assert_eq!(
            parse_source(bad),
            Err("not a URL, magnet link or .torrent file".to_string()),
            "{bad:?}"
        );
    }
    // A torrent under `~` expands to the home directory.
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from)
        && home.is_absolute()
        && home.is_dir()
    {
        let name = format!("puppy-tui-test-{}.torrent", std::process::id());
        let p = home.join(&name);
        if std::fs::write(&p, b"x").is_ok() {
            let got = parse_source(&format!("~/{name}"));
            let _ = std::fs::remove_file(&p);
            assert_eq!(got, Ok(Source::TorrentFile { path: p }));
        }
    }
}

#[test]
fn popup_cursor_follows_focused_input() {
    let (mut a, now) = connected(vec![snap(1, "a", WireState::Active)]);
    a.update(ch('l'), now);
    typed(&mut a, "12", now);
    let term = render_term(&mut a, 100, 30);
    let mut term = term;
    let pos = term.get_cursor_position().unwrap();
    assert!(term.backend().cursor_visible());
    let buf = term.backend().buffer();
    let (x, y) = find_xy(buf, "12").unwrap();
    assert_eq!(pos, Position::new(x + 2, y));
    // No cursor on the checkbox / confirm popups.
    a.update(key(KeyCode::Esc), now);
    a.update(ch('d'), now);
    let term = render_term(&mut a, 100, 30);
    assert!(!term.backend().cursor_visible());
}

#[test]
fn add_form_takes_an_optional_checksum() {
    use puppy_core::checksum::HashAlgo;
    use puppy_tui::app::forms::ADD_CHECKSUM_NOT_HTTP;
    // Empty: no check.
    let mut f = AddForm::with_source("https://example.com/a.iso");
    match f.submit() {
        Some(Cmd::Add { request, .. }) => assert_eq!(request.checksum, None),
        c => panic!("{c:?}"),
    }
    let hex = "AB".repeat(32);
    f.checksum = TextInput::with_value(&format!(" sha256={hex} "));
    match f.submit() {
        Some(Cmd::Add { request, .. }) => {
            let c = request.checksum.unwrap();
            assert_eq!((c.algo, c.hex), (HashAlgo::Sha256, hex.to_lowercase()));
        }
        c => panic!("{c:?}"),
    }
    f.checksum = TextInput::with_value("sha-256=abc");
    assert!(f.submit().is_none());
    assert_eq!(
        (f.error_on, f.focus),
        (AddField::Checksum, AddField::Checksum)
    );
    assert!(
        f.error.as_deref().unwrap().starts_with("bad checksum: "),
        "{f:?}"
    );
    let mut t = AddForm::with_source("magnet:?xt=urn:btih:abc");
    t.checksum = TextInput::with_value(&format!("sha-256={hex}"));
    assert!(t.submit().is_none());
    assert_eq!(t.error.as_deref(), Some(ADD_CHECKSUM_NOT_HTTP));

    // Rendered under "Save to", the error under its field.
    let (mut a, now) = connected(vec![]);
    a.update(ch('a'), now);
    typed(&mut a, "https://example.com/a.iso", now);
    a.update(key(KeyCode::Tab), now);
    a.update(key(KeyCode::Tab), now);
    typed(&mut a, "md5=1", now);
    assert!(a.update(key(KeyCode::Enter), now).is_empty());
    let r = render(&mut a, 100, 30);
    let label = find(&r, "Checksum (optional").unwrap_or_else(|| panic!("{r:#?}"));
    assert!(label > find(&r, "Save to").unwrap());
    let err = find(&r, "bad checksum").unwrap_or_else(|| panic!("{r:#?}"));
    assert!(
        err > label && err < find(&r, "start paused").unwrap(),
        "{r:#?}"
    );
}
