//! The torrent file picker (`c`, auto-opened for torrents waiting in "choosing files")
//! and the seeding popup (`s`).

use std::time::Instant;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use puppy_core::types::{DownloadId, FileChoice, SeedPolicy, Selection, TorrentFile};
use puppy_proto::{DownloadSnapshot, EngineKindTag, Event, Settings, Totals, WireState};
use puppy_tui::app::picker::{Check, EXISTS_HINT, PICK_NONE, Picker};
use puppy_tui::app::{
    AddForm, App, Cmd, CmdError, Conn, HitTarget, Mode, Msg, Popup, SeedForm, TextInput,
};
use puppy_tui::theme::{Theme, ThemeChoice};
use puppy_tui::ui::draw;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use unicode_width::UnicodeWidthStr;

const MIB: u64 = 1024 * 1024;

fn theme() -> Theme {
    Theme::detect(ThemeChoice::Dark, &|k| {
        (k == "COLORTERM").then(|| "truecolor".to_string())
    })
}

fn torrent(id: DownloadId, name: &str, state: WireState) -> DownloadSnapshot {
    DownloadSnapshot {
        id,
        kind: EngineKindTag::Torrent,
        source: format!("magnet:?xt=urn:btih:{id}&dn={name}"),
        name: name.into(),
        path: None,
        state,
        total_bytes: Some(1000 * MIB),
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
    }
}

fn settings() -> Settings {
    Settings {
        max_active: 3,
        global_down_limit: None,
        global_up_limit: None,
        download_dir: "/srv/downloads".into(),
        connections: 8,
        theme: "auto".into(),
    }
}

fn connected(downloads: Vec<DownloadSnapshot>) -> (App, Instant, Vec<Cmd>) {
    let mut a = App::new(theme());
    let now = Instant::now();
    let cmds = a.update(
        Msg::Connected {
            version: "0.1.0".into(),
            settings: settings(),
            downloads,
        },
        now,
    );
    assert_eq!(a.conn, Conn::Connected);
    (a, now, cmds)
}

fn snapshot(downloads: Vec<DownloadSnapshot>) -> Msg {
    Msg::Daemon(Event::Snapshot {
        downloads,
        totals: Totals::default(),
    })
}

fn key(code: KeyCode) -> Msg {
    Msg::Key(KeyEvent::new(code, KeyModifiers::NONE))
}

fn ch(c: char) -> Msg {
    key(KeyCode::Char(c))
}

fn file(index: usize, path: &str, mib: u64) -> TorrentFile {
    TorrentFile {
        index,
        path: path.into(),
        size: mib * MIB,
    }
}

/// `Show/` with two seasons and a loose file.
fn show_files() -> Vec<TorrentFile> {
    vec![
        file(0, "Show/S01/e1.mkv", 300),
        file(1, "Show/S01/e2.mkv", 300),
        file(2, "Show/S02/e1.mkv", 350),
        file(3, "Show/notes.txt", 1),
    ]
}

fn render_buf(app: &mut App, w: u16, h: u16) -> Buffer {
    let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
    term.draw(|f| draw(app, f)).unwrap();
    term.backend().buffer().clone()
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

/// An app showing the picker for torrent 1 ("Show") with [`show_files`].
fn picker_app() -> (App, Instant) {
    let (mut a, now, cmds) = connected(vec![torrent(1, "Show", WireState::ChoosingFiles)]);
    assert_eq!(cmds, vec![Cmd::FetchFiles(1)]);
    a.update(Msg::Files(1, show_files()), now);
    assert!(
        matches!(a.mode, Mode::Popup(Popup::Picker(_))),
        "{:?}",
        a.mode
    );
    (a, now)
}

fn picker(a: &App) -> &Picker {
    match &a.mode {
        Mode::Popup(Popup::Picker(p)) => p,
        m => panic!("no picker: {m:?}"),
    }
}

/// Moves the picker's cursor to the row showing `name` (by node name).
fn cursor_to(a: &mut App, now: Instant, name: &str) {
    a.update(key(KeyCode::Home), now);
    for _ in 0..64 {
        let p = picker(a);
        let rows = p.rows();
        if p.nodes[rows[p.cursor]].name == name {
            return;
        }
        a.update(key(KeyCode::Down), now);
    }
    panic!("no row {name}");
}

fn row_names(p: &Picker) -> Vec<String> {
    p.rows().iter().map(|&i| p.nodes[i].name.clone()).collect()
}

#[test]
fn picker_renders_tree_and_totals() {
    let (mut a, _) = picker_app();
    let p = picker(&a);
    // The torrent's own folder is the title already: its contents are the top level.
    assert_eq!(
        row_names(p),
        ["S01", "e1.mkv", "e2.mkv", "S02", "e1.mkv", "notes.txt"]
    );
    // Everything starts selected.
    assert_eq!(p.totals(), (4, 4, 951 * MIB, 951 * MIB));
    let r = render(&mut a, 100, 30);
    let title = find(&r, "Choose files — Show").unwrap_or_else(|| panic!("{r:#?}"));
    assert!(
        r[title].contains("4 of 4 files · 951.0 MiB of 951.0 MiB"),
        "{:?}",
        r[title]
    );
    let s01 = find(&r, "S01").unwrap();
    assert!(r[s01].contains("▾ ☑ S01"), "{:?}", r[s01]);
    assert!(r[s01].contains("600.0 MiB"), "{:?}", r[s01]);
    let e1 = s01 + 1;
    assert!(r[e1].contains("☑ e1.mkv"), "{:?}", r[e1]);
    // Sizes are right-aligned in one column; files carry their 1-based number.
    let col = |row: &str, needle: &str| row.find(needle).map(|b| row[..b].width());
    let end = |row: usize, size: &str| col(&r[row], size).unwrap() + size.width();
    assert_eq!(end(s01, "600.0 MiB"), end(e1, "300.0 MiB"));
    let notes = find(&r, "notes.txt").unwrap();
    assert_eq!(end(notes, "1.0 MiB"), end(e1, "300.0 MiB"));
    assert!(r[notes].contains(" 4 "), "{:?}", r[notes]);
    // Footer hints for the picker.
    let hints = r.last().unwrap();
    for h in ["Space toggle", "a all", "n none", "/ filter", "Esc close"] {
        assert!(hints.contains(h), "{h}: {hints:?}");
    }
    // Collapse S01 with ←: its files are hidden, the folder shows ▸.
    a.update(key(KeyCode::Left), Instant::now());
    assert_eq!(row_names(picker(&a)), ["S01", "S02", "e1.mkv", "notes.txt"]);
    let r = render(&mut a, 100, 30);
    assert!(find(&r, "▸ ☑ S01").is_some(), "{r:#?}");
    // → expands it again; Enter on a folder folds too.
    a.update(key(KeyCode::Right), Instant::now());
    assert_eq!(row_names(picker(&a)).len(), 6);
    let cmds = a.update(key(KeyCode::Enter), Instant::now());
    assert!(cmds.is_empty());
    assert_eq!(row_names(picker(&a)).len(), 4);
}

#[test]
fn picker_keeps_a_root_folder_named_differently() {
    let p = Picker::with_files(
        1,
        "Other",
        vec![file(0, "Show/a.mkv", 1), file(1, "Show/b.mkv", 1)],
    );
    assert_eq!(row_names(&p), ["Show", "a.mkv", "b.mkv"]);
    // Loose files at the top level.
    let p = Picker::with_files(1, "x", vec![file(0, "a", 1), file(1, "d/b", 1)]);
    assert_eq!(row_names(&p), ["d", "b", "a"]);
}

#[test]
fn picker_toggle_folder_tristate() {
    let (mut a, now) = picker_app();
    // Space on S01: both its files off; S01 ☐.
    cursor_to(&mut a, now, "S01");
    a.update(ch(' '), now);
    let p = picker(&a);
    assert_eq!(p.checked, [false, false, true, true]);
    assert_eq!(p.check_of(p.rows()[0]), Check::None);
    assert_eq!(p.totals(), (2, 4, 351 * MIB, 951 * MIB));
    // One file back on: S01 is partly selected.
    a.update(key(KeyCode::Down), now);
    a.update(ch(' '), now);
    let p = picker(&a);
    assert_eq!(p.checked, [true, false, true, true]);
    assert_eq!(p.check_of(p.rows()[0]), Check::Some);
    let r = render(&mut a, 100, 30);
    assert!(find(&r, "▾ ▣ S01").is_some(), "{r:#?}");
    assert!(find(&r, "3 of 4 files").is_some(), "{r:#?}");
    // Space on a partly selected folder selects all of it.
    cursor_to(&mut a, now, "S01");
    a.update(ch(' '), now);
    assert_eq!(picker(&a).checked, [true, true, true, true]);
    // `n` none, `a` all.
    a.update(ch('n'), now);
    assert_eq!(picker(&a).checked, [false; 4]);
    a.update(ch('a'), now);
    assert_eq!(picker(&a).checked, [true; 4]);

    // A click on a row toggles it (and moves the cursor there).
    let buf = render_buf(&mut a, 100, 30);
    let r = rows(&buf);
    let y = find(&r, "notes.txt").unwrap() as u16;
    let x = r[usize::from(y)].find("notes.txt").unwrap() as u16;
    let x = r[usize::from(y)][..usize::from(x)].width() as u16;
    assert!(
        a.hits
            .iter()
            .any(|(rect, t)| matches!(t, HitTarget::PickerRow(_)) && rect.contains((x, y).into()))
    );
    let click = Msg::Mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: x,
        row: y,
        modifiers: KeyModifiers::NONE,
    });
    a.update(click.clone(), now);
    let p = picker(&a);
    assert_eq!(p.checked, [true, true, true, false]);
    assert_eq!(p.nodes[p.rows()[p.cursor]].name, "notes.txt");
    a.update(click, now + std::time::Duration::from_secs(1));
    assert_eq!(picker(&a).checked, [true; 4]);
}

#[test]
fn picker_filter() {
    let (mut a, now) = picker_app();
    a.update(ch('/'), now);
    for c in "E1".chars() {
        a.update(ch(c), now);
    }
    // Case-insensitive substring on the path; matching files keep their folders.
    assert_eq!(row_names(picker(&a)), ["S01", "e1.mkv", "S02", "e1.mkv"]);
    let r = render(&mut a, 100, 30);
    assert!(find(&r, "/ E1").is_some(), "{r:#?}");
    assert!(find(&r, "notes.txt").is_none(), "{r:#?}");
    // A folder name matches too (its files are shown).
    a.update(key(KeyCode::Backspace), now);
    a.update(key(KeyCode::Backspace), now);
    for c in "s02".chars() {
        a.update(ch(c), now);
    }
    assert_eq!(row_names(picker(&a)), ["S02", "e1.mkv"]);
    // Enter keeps the filter; `n` then clears only what is shown.
    a.update(key(KeyCode::Enter), now);
    assert!(!picker(&a).filtering);
    a.update(ch('n'), now);
    assert_eq!(picker(&a).checked, [true, true, false, true]);
    // Nothing matches: a hint, no rows.
    a.update(ch('/'), now);
    a.update(ch('z'), now);
    assert!(picker(&a).rows().is_empty());
    let r = render(&mut a, 100, 30);
    assert!(find(&r, "no files match").is_some(), "{r:#?}");
    // Esc while typing clears the filter (the picker stays open).
    a.update(key(KeyCode::Esc), now);
    let p = picker(&a);
    assert!(!p.filtering && p.filter.is_empty());
    assert_eq!(p.rows().len(), 6);
    // Esc again closes the picker.
    a.update(key(KeyCode::Esc), now);
    assert_eq!(a.mode, Mode::Normal);
}

#[test]
fn picker_confirm_emits_select() {
    let (mut a, now) = picker_app();
    cursor_to(&mut a, now, "S02");
    a.update(ch(' '), now);
    cursor_to(&mut a, now, "notes.txt");
    let cmds = a.update(key(KeyCode::Enter), now);
    assert_eq!(
        cmds,
        vec![Cmd::SelectFiles {
            id: 1,
            selection: Selection {
                indices: vec![0, 1, 3],
                globs: vec![],
            },
        }]
    );
    // Open until the daemon answers; a second Enter sends nothing.
    assert!(picker(&a).submitting);
    assert!(a.update(key(KeyCode::Enter), now).is_empty());
    // A refusal is shown inline, with what to do about it.
    a.update(
        Msg::Selected(
            1,
            Err(CmdError::Failed(
                "refusing to overwrite: a file of this torrent already exists".into(),
            )),
        ),
        now,
    );
    let p = picker(&a);
    assert!(!p.submitting);
    let e = p.error.as_deref().unwrap();
    assert!(
        e.contains("already exists") && e.contains(EXISTS_HINT),
        "{e}"
    );
    let r = render(&mut a, 100, 30);
    assert!(find(&r, "✗ refusing to overwrite").is_some(), "{r:#?}");
    // `y` confirms from anywhere (here: a folder row); success closes the picker.
    cursor_to(&mut a, now, "S01");
    let cmds = a.update(ch('y'), now);
    assert!(
        matches!(cmds.as_slice(), [Cmd::SelectFiles { id: 1, .. }]),
        "{cmds:?}"
    );
    a.update(Msg::Selected(1, Ok(())), now);
    assert_eq!(a.mode, Mode::Normal);
    // The torrent leaving "choosing files" does not bring the picker back.
    let cmds = a.update(snapshot(vec![torrent(1, "Show", WireState::Queued)]), now);
    assert!(cmds.is_empty());
    assert_eq!(a.mode, Mode::Normal);
}

#[test]
fn picker_zero_selected_error() {
    let (mut a, now) = picker_app();
    a.update(ch('n'), now);
    cursor_to(&mut a, now, "notes.txt");
    assert!(a.update(key(KeyCode::Enter), now).is_empty());
    let p = picker(&a);
    assert_eq!(p.error.as_deref(), Some(PICK_NONE));
    assert!(!p.submitting);
    let r = render(&mut a, 100, 30);
    assert!(find(&r, PICK_NONE).is_some(), "{r:#?}");
    assert!(find(&r, "0 of 4 files").is_some(), "{r:#?}");
    // Toggling something clears the error.
    a.update(ch(' '), now);
    assert_eq!(picker(&a).error, None);
}

#[test]
fn picker_large_list_and_small_screen() {
    let files: Vec<TorrentFile> = (0..5000)
        .map(|i| file(i, &format!("Big/dir{:02}/file{i:05}.bin", i / 100), 1))
        .collect();
    let mut p = Picker::with_files(1, "Big", files);
    assert_eq!(p.rows().len(), 5050);
    p.handle_key(&KeyEvent::from(KeyCode::End));
    let (mut a, now, _) = connected(vec![torrent(1, "Big", WireState::ChoosingFiles)]);
    a.mode = Mode::Popup(Popup::Picker(Box::new(p)));
    for (w, h) in [(100u16, 30u16), (60, 16), (80, 24), (200, 60)] {
        let r = render(&mut a, w, h);
        // The cursor row (the last file) is visible, with the scroll position.
        assert!(find(&r, "file04999.bin").is_some(), "{w}×{h}: {r:#?}");
        assert!(find(&r, "of 5050").is_some(), "{w}×{h}: {r:#?}");
        assert!(find(&r, "Choose files").is_some(), "{w}×{h}: {r:#?}");
    }
    // Back to the top: the first rows show.
    a.update(key(KeyCode::Home), now);
    let r = render(&mut a, 60, 16);
    assert!(find(&r, "dir00").is_some(), "{r:#?}");
    assert!(find(&r, "file04999.bin").is_none());
    // A page down moves by what is shown and stays visible.
    a.update(key(KeyCode::PageDown), now);
    let p = picker(&a);
    assert!(p.cursor > 3, "{}", p.cursor);
    let name = p.nodes[p.rows()[p.cursor]].name.clone();
    let r = render(&mut a, 60, 16);
    assert!(find(&r, &name).is_some(), "{name}: {r:#?}");
    // Any size: no panic.
    for w in [0u16, 1, 10, 30, 59, 60, 61, 79, 120] {
        for h in [0u16, 1, 5, 15, 16, 17, 40] {
            render(&mut a, w, h);
        }
    }
    // Toggling everything stays fast enough on 5000 files.
    let t = Instant::now();
    for _ in 0..20 {
        a.update(ch('n'), now);
        a.update(ch('a'), now);
        render(&mut a, 100, 30);
    }
    assert!(t.elapsed().as_secs() < 5, "{:?}", t.elapsed());
}

#[test]
fn auto_open_on_choosing() {
    // An HTTP download and a torrent that is still fetching metadata: nothing opens.
    let mut http = torrent(1, "page.html", WireState::Active);
    http.kind = EngineKindTag::Http;
    let (mut a, now, cmds) = connected(vec![http.clone(), torrent(2, "Show", WireState::Active)]);
    assert_eq!(cmds, vec![Cmd::FetchDetail(1)]);
    // The torrent's contents are known: its files are fetched and the picker opens.
    let cmds = a.update(
        snapshot(vec![
            http.clone(),
            torrent(2, "Show", WireState::ChoosingFiles),
        ]),
        now,
    );
    assert_eq!(cmds, vec![Cmd::FetchFiles(2)]);
    // Not asked twice while the list is on its way.
    let cmds = a.update(
        snapshot(vec![
            http.clone(),
            torrent(2, "Show", WireState::ChoosingFiles),
        ]),
        now,
    );
    assert!(cmds.is_empty(), "{cmds:?}");
    a.update(Msg::Files(2, show_files()), now);
    assert_eq!(picker(&a).id, 2);
    // Esc dismisses it; it does not come back by itself.
    a.update(key(KeyCode::Esc), now);
    assert_eq!(a.mode, Mode::Normal);
    let cmds = a.update(
        snapshot(vec![
            http.clone(),
            torrent(2, "Show", WireState::ChoosingFiles),
        ]),
        now,
    );
    assert!(cmds.is_empty(), "{cmds:?}");
    assert_eq!(a.mode, Mode::Normal);
    // `c` reopens it for the selection (footer offers it).
    a.update(key(KeyCode::Down), now);
    assert_eq!(a.selected, Some(2));
    let r = render(&mut a, 120, 30);
    assert!(r.last().unwrap().contains("c choose files"), "{r:#?}");
    let cmds = a.update(ch('c'), now);
    assert_eq!(cmds, vec![Cmd::FetchFiles(2)]);
    assert!(picker(&a).loading);
    let r = render(&mut a, 100, 30);
    assert!(find(&r, "loading the file list").is_some(), "{r:#?}");
    a.update(Msg::Files(2, show_files()), now);
    assert!(!picker(&a).loading);
    // A failed fetch: a toast, and the loading picker closes.
    a.update(key(KeyCode::Esc), now);
    a.update(ch('c'), now);
    a.update(
        Msg::FilesFailed(2, CmdError::Failed("not in choosing files".into())),
        now,
    );
    assert_eq!(a.mode, Mode::Normal);
    assert!(
        a.toasts
            .iter()
            .any(|t| t.text.contains("not in choosing files"))
    );
    // `c` on an HTTP download: a toast says why not.
    a.update(key(KeyCode::Up), now);
    assert!(a.update(ch('c'), now).is_empty());
    assert_eq!(a.mode, Mode::Normal);

    // Another popup open: a torrent entering "choosing files" waits for it to close.
    let (mut a, now, _) = connected(vec![torrent(3, "Other", WireState::Active)]);
    a.update(ch('?'), now);
    let cmds = a.update(
        snapshot(vec![torrent(3, "Other", WireState::ChoosingFiles)]),
        now,
    );
    assert!(cmds.is_empty(), "{cmds:?}");
    a.update(key(KeyCode::Esc), now);
    let cmds = a.update(
        snapshot(vec![torrent(3, "Other", WireState::ChoosingFiles)]),
        now,
    );
    assert_eq!(cmds, vec![Cmd::FetchFiles(3)]);
    // The list arrives while the user opened another popup: dropped, asked again later.
    a.update(ch('?'), now);
    a.update(Msg::Files(3, show_files()), now);
    assert_eq!(a.mode, Mode::Popup(Popup::Help));
    a.update(key(KeyCode::Esc), now);
    let cmds = a.update(
        snapshot(vec![torrent(3, "Other", WireState::ChoosingFiles)]),
        now,
    );
    assert_eq!(cmds, vec![Cmd::FetchFiles(3)]);
    // The picker closes when the torrent leaves "choosing files" elsewhere (the CLI).
    a.update(Msg::Files(3, show_files()), now);
    assert!(matches!(a.mode, Mode::Popup(Popup::Picker(_))));
    a.update(snapshot(vec![torrent(3, "Other", WireState::Queued)]), now);
    assert_eq!(a.mode, Mode::Normal);
}

#[test]
fn dashboard_adds_ask_for_torrents() {
    let mut f = AddForm::with_source("magnet:?xt=urn:btih:abc");
    match f.submit() {
        Some(Cmd::Add { request, .. }) => assert_eq!(request.files, FileChoice::Ask),
        c => panic!("{c:?}"),
    }
    let mut f = AddForm::with_source("https://example.com/a.iso");
    match f.submit() {
        Some(Cmd::Add { request, .. }) => assert_eq!(request.files, FileChoice::All),
        c => panic!("{c:?}"),
    }
}

#[test]
fn seed_popup_emits_set_seed() {
    let seeding = WireState::Seeding {
        uploaded: 420 * MIB,
        ratio: 0.42,
        seeded_secs: 600,
        ratio_target: Some(1.0),
        time_target_secs: Some(7200),
    };
    let mut http = torrent(2, "page.html", WireState::Active);
    http.kind = EngineKindTag::Http;
    let (mut a, now, _) = connected(vec![torrent(1, "Show", seeding), http]);
    // The footer offers it; pause stops seeding.
    let r = render(&mut a, 120, 30);
    let hints = r.last().unwrap();
    assert!(
        hints.contains("s seeding") && hints.contains("p pause"),
        "{hints:?}"
    );
    assert_eq!(a.update(ch('p'), now), vec![Cmd::Pause(1)]);

    a.update(ch('s'), now);
    let form = match &a.mode {
        Mode::Popup(Popup::Seed(f)) => f.clone(),
        m => panic!("{m:?}"),
    };
    assert_eq!(form.ratio.value, "1.0");
    assert_eq!(form.time.value, "2h");
    let r = render(&mut a, 100, 30);
    for s in [
        "Seeding",
        "Ratio",
        "Time",
        "e.g. 1.5, 0 = off, empty = default",
        "e.g. 2h, 30m, 0 = off",
    ] {
        assert!(find(&r, s).is_some(), "{s}: {r:#?}");
    }
    // Bad ratio: inline error, no command.
    a.mode = Mode::Popup(Popup::Seed(SeedForm {
        ratio: TextInput::with_value("lots"),
        ..form.clone()
    }));
    assert!(a.update(key(KeyCode::Enter), now).is_empty());
    match &a.mode {
        Mode::Popup(Popup::Seed(f)) => assert!(f.error.is_some() && f.error_field == 0),
        m => panic!("{m:?}"),
    }
    let r = render(&mut a, 100, 30);
    assert!(find(&r, "✗").is_some(), "{r:#?}");
    // Bad time.
    a.mode = Mode::Popup(Popup::Seed(SeedForm {
        time: TextInput::with_value("soon"),
        ..form.clone()
    }));
    assert!(a.update(key(KeyCode::Enter), now).is_empty());
    match &a.mode {
        Mode::Popup(Popup::Seed(f)) => assert!(f.error.is_some() && f.error_field == 1),
        m => panic!("{m:?}"),
    }
    // Ratio 1.5, time empty (default): the whole policy is replaced.
    a.mode = Mode::Popup(Popup::Seed(SeedForm {
        ratio: TextInput::with_value("1.5"),
        time: TextInput::new(),
        ..form.clone()
    }));
    assert_eq!(
        a.update(key(KeyCode::Enter), now),
        vec![Cmd::SetSeed {
            id: 1,
            policy: SeedPolicy {
                ratio: Some(1.5),
                time_secs: None,
            },
        }]
    );
    assert_eq!(a.mode, Mode::Normal);
    // `0` turns a trigger off; `90m` is a duration.
    let mut f = SeedForm {
        ratio: TextInput::with_value("0"),
        time: TextInput::with_value("90m"),
        ..form.clone()
    };
    assert_eq!(
        f.submit(),
        Some(Cmd::SetSeed {
            id: 1,
            policy: SeedPolicy {
                ratio: Some(0.0),
                time_secs: Some(5400),
            },
        })
    );

    // A torrent still downloading: the policy can be set ahead (fields empty).
    let (mut a, now, _) = connected(vec![torrent(5, "Next", WireState::Active)]);
    a.update(ch('s'), now);
    match &a.mode {
        Mode::Popup(Popup::Seed(f)) => assert!(f.ratio.is_empty() && f.time.is_empty()),
        m => panic!("{m:?}"),
    }
    // Disabled targets prefill as 0.
    let f = SeedForm::new(
        1,
        &WireState::Seeding {
            uploaded: 0,
            ratio: 0.0,
            seeded_secs: 0,
            ratio_target: None,
            time_target_secs: Some(5400),
        },
    );
    assert_eq!(
        (f.ratio.value.as_str(), f.time.value.as_str()),
        ("0", "1h 30m")
    );
    // Not for HTTP or completed torrents.
    let mut http = torrent(6, "page.html", WireState::Active);
    http.kind = EngineKindTag::Http;
    let (mut a, now, _) = connected(vec![http, torrent(7, "Old", WireState::Completed)]);
    a.update(ch('s'), now);
    assert_eq!(a.mode, Mode::Normal);
    assert!(!a.toasts.is_empty());
    a.update(key(KeyCode::Down), now);
    a.update(ch('s'), now);
    assert_eq!(a.mode, Mode::Normal);
}
