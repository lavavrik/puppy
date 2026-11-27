//! Mouse support: render once to fill `App::hits`, then send `Msg::Mouse`.

use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use puppy_core::types::DownloadId;
use puppy_proto::{DownloadSnapshot, EngineKindTag, Settings, WireState};
use puppy_tui::app::{AddField, App, Cmd, Conn, HitTarget, Mode, Msg, Popup};
use puppy_tui::theme::{Theme, ThemeChoice};
use puppy_tui::ui::draw;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::layout::{Position, Rect};

fn snap(id: DownloadId, state: WireState) -> DownloadSnapshot {
    DownloadSnapshot {
        id,
        kind: EngineKindTag::Http,
        source: format!("https://example.com/f{id}"),
        name: format!("file{id}.bin"),
        path: None,
        state,
        total_bytes: Some(1000),
        done_bytes: 10,
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
        mirrors: Vec::new(),
        expected_size: None,
    }
}

fn app(downloads: Vec<DownloadSnapshot>) -> App {
    let mut a = App::new(Theme::detect(ThemeChoice::Dark, &|_| None));
    a.conn = Conn::Connected;
    a.settings = Some(Settings {
        max_active: 3,
        global_down_limit: None,
        global_up_limit: None,
        download_dir: "/srv".into(),
        connections: 8,
        theme: "auto".into(),
    });
    a.selected = downloads.first().map(|d| d.id);
    a.downloads = downloads;
    a
}

fn render(a: &mut App) {
    render_at(a, 100, 30);
}

fn render_at(a: &mut App, w: u16, h: u16) {
    let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
    term.draw(|f| draw(a, f)).unwrap();
}

fn mouse(kind: MouseEventKind, x: u16, y: u16, modifiers: KeyModifiers) -> Msg {
    Msg::Mouse(MouseEvent {
        kind,
        column: x,
        row: y,
        modifiers,
    })
}

fn click(a: &mut App, x: u16, y: u16, now: Instant) -> Vec<Cmd> {
    a.update(
        mouse(
            MouseEventKind::Down(MouseButton::Left),
            x,
            y,
            KeyModifiers::NONE,
        ),
        now,
    )
}

fn rect_of(a: &App, t: HitTarget) -> Rect {
    a.hits.iter().find(|(_, h)| *h == t).expect("hit").0
}

fn three() -> App {
    app(vec![
        snap(1, WireState::Active),
        snap(2, WireState::Active),
        snap(3, WireState::Paused),
    ])
}

#[test]
fn click_selects_row() {
    let mut a = three();
    render(&mut a);
    let r = rect_of(&a, HitTarget::ListRow(2));
    click(&mut a, r.x + 3, r.y, Instant::now());
    assert_eq!(a.selected, Some(2));
}

#[test]
fn click_second_line_selects_same_row() {
    let mut a = three();
    render(&mut a);
    let r = rect_of(&a, HitTarget::ListRow(3));
    assert_eq!(r.height, 2);
    click(&mut a, r.x + 3, r.y + 1, Instant::now());
    assert_eq!(a.selected, Some(3));
}

#[test]
fn double_click_toggles_pause() {
    let mut a = three();
    render(&mut a);
    let now = Instant::now();
    let r = rect_of(&a, HitTarget::ListRow(1));
    assert!(click(&mut a, r.x + 3, r.y, now).is_empty());
    let cmds = click(&mut a, r.x + 3, r.y, now + Duration::from_millis(300));
    assert!(matches!(cmds.as_slice(), [Cmd::Pause(1)]), "{cmds:?}");
    // A paused row resumes.
    let r = rect_of(&a, HitTarget::ListRow(3));
    click(&mut a, r.x, r.y, now);
    let cmds = click(&mut a, r.x, r.y, now + Duration::from_millis(100));
    assert!(matches!(cmds.as_slice(), [Cmd::Resume(3)]), "{cmds:?}");
}

#[test]
fn slow_or_different_row_clicks_are_not_double() {
    let mut a = three();
    render(&mut a);
    let now = Instant::now();
    let r1 = rect_of(&a, HitTarget::ListRow(1));
    let r2 = rect_of(&a, HitTarget::ListRow(2));
    click(&mut a, r1.x, r1.y, now);
    assert!(click(&mut a, r1.x, r1.y, now + Duration::from_millis(500)).is_empty());
    click(&mut a, r1.x, r1.y, now + Duration::from_millis(600));
    // Selecting another row only fetches its detail.
    assert_eq!(
        click(&mut a, r2.x, r2.y, now + Duration::from_millis(700)),
        vec![Cmd::FetchDetail(2)]
    );
}

#[test]
fn wheel_moves_selection() {
    let ds = (1..=8).map(|i| snap(i, WireState::Active)).collect();
    let mut a = app(ds);
    render(&mut a);
    let list = rect_of(&a, HitTarget::List);
    let (x, y) = (list.x + 2, list.y + 2);
    let now = Instant::now();
    a.update(
        mouse(MouseEventKind::ScrollDown, x, y, KeyModifiers::NONE),
        now,
    );
    assert_eq!(a.selected, Some(2));
    a.update(
        mouse(MouseEventKind::ScrollDown, x, y, KeyModifiers::SHIFT),
        now,
    );
    assert_eq!(a.selected, Some(5));
    a.update(
        mouse(MouseEventKind::ScrollUp, x, y, KeyModifiers::NONE),
        now,
    );
    assert_eq!(a.selected, Some(4));
}

#[test]
fn wheel_over_detail_pane_does_nothing() {
    let ds = (1..=8).map(|i| snap(i, WireState::Active)).collect();
    let mut a = app(ds);
    // Wide layout: the detail pane sits right of the list.
    render_at(&mut a, 120, 30);
    let list = rect_of(&a, HitTarget::List);
    assert!(list.right() < 110, "{list:?}");
    let (x, y) = (list.right() + 5, list.y + 3);
    assert!(a.hits.iter().all(|(r, _)| !r.contains(Position::new(x, y))));
    a.update(
        mouse(MouseEventKind::ScrollDown, x, y, KeyModifiers::NONE),
        Instant::now(),
    );
    assert_eq!(a.selected, Some(1));
    // The same notch over the list does scroll.
    a.update(
        mouse(
            MouseEventKind::ScrollDown,
            list.x + 2,
            y,
            KeyModifiers::NONE,
        ),
        Instant::now(),
    );
    assert_eq!(a.selected, Some(2));
}

#[test]
fn resize_clears_stale_hits() {
    let mut a = three();
    render(&mut a);
    assert!(!a.hits.is_empty());
    a.update(Msg::Resize(60, 20), Instant::now());
    assert!(a.hits.is_empty());
    let r = Rect::new(0, 0, 100, 30);
    // A click where a row used to be does nothing until the next frame.
    for y in r.y..r.bottom() {
        click(&mut a, 5, y, Instant::now());
    }
    assert_eq!(a.selected, Some(1));
}

#[test]
fn double_click_memory_resets_on_selection_change() {
    let mut a = three();
    render(&mut a);
    let now = Instant::now();
    let r = rect_of(&a, HitTarget::ListRow(1));
    click(&mut a, r.x + 3, r.y, now);
    // j then k: back on row 1, but the click memory is gone.
    a.update(Msg::Key(KeyEvent::from(KeyCode::Char('j'))), now);
    a.update(Msg::Key(KeyEvent::from(KeyCode::Char('k'))), now);
    let cmds = click(&mut a, r.x + 3, r.y, now + Duration::from_millis(100));
    assert!(cmds.is_empty(), "{cmds:?}");
}

#[test]
fn double_click_memory_resets_on_mode_change() {
    let mut a = three();
    render(&mut a);
    let now = Instant::now();
    let r = rect_of(&a, HitTarget::ListRow(1));
    click(&mut a, r.x + 3, r.y, now);
    a.update(Msg::Key(KeyEvent::from(KeyCode::Char('/'))), now);
    a.update(Msg::Key(KeyEvent::from(KeyCode::Enter)), now);
    assert_eq!(a.mode, Mode::Normal);
    let cmds = click(&mut a, r.x + 3, r.y, now + Duration::from_millis(100));
    assert!(cmds.is_empty(), "{cmds:?}");
}

#[test]
fn ctrl_alt_keys_ignored_in_confirm_and_help() {
    let now = Instant::now();
    for m in [
        KeyModifiers::CONTROL,
        KeyModifiers::ALT,
        KeyModifiers::CONTROL | KeyModifiers::ALT,
    ] {
        let mut a = three();
        a.update(Msg::Key(KeyEvent::from(KeyCode::Char('d'))), now);
        for c in ['f', ' ', 'n'] {
            a.update(Msg::Key(KeyEvent::new(KeyCode::Char(c), m)), now);
        }
        let cmds = a.update(Msg::Key(KeyEvent::new(KeyCode::Enter, m)), now);
        assert!(cmds.is_empty(), "{cmds:?}");
        assert!(matches!(
            a.mode,
            Mode::Popup(Popup::ConfirmDelete {
                delete_files: false,
                ..
            })
        ));

        let mut a = three();
        a.update(Msg::Key(KeyEvent::from(KeyCode::Char('?'))), now);
        for c in ['q', '?', 'j', 'G'] {
            a.update(Msg::Key(KeyEvent::new(KeyCode::Char(c), m)), now);
        }
        assert_eq!(a.mode, Mode::Popup(Popup::Help));
        assert_eq!(a.help_scroll, 0);
        // Ctrl+C still quits.
        a.update(
            Msg::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            now,
        );
        assert!(a.should_quit);
    }
}

#[test]
fn click_footer_hint_opens_add_popup() {
    let mut a = three();
    render(&mut a);
    let r = rect_of(&a, HitTarget::FooterKey('a'));
    click(&mut a, r.x, r.y, Instant::now());
    assert!(matches!(a.mode, Mode::Popup(Popup::Add(_))));
}

#[test]
fn click_outside_popup_closes() {
    let mut a = three();
    a.update(
        Msg::Key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE)),
        Instant::now(),
    );
    render(&mut a);
    let popup = rect_of(&a, HitTarget::PopupInside);
    // Inside: stays open.
    click(&mut a, popup.x + 2, popup.y + 1, Instant::now());
    assert!(matches!(a.mode, Mode::Popup(_)));
    // A list row behind the popup is not selected; the popup closes.
    let row = rect_of(&a, HitTarget::ListRow(2));
    assert!(!popup.contains(Position::new(row.x, row.y)));
    click(&mut a, row.x, row.y, Instant::now());
    assert_eq!(a.mode, Mode::Normal);
    assert_eq!(a.selected, Some(1));
}

#[test]
fn click_popup_checkbox_toggles() {
    let mut a = three();
    a.update(
        Msg::Key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE)),
        Instant::now(),
    );
    render(&mut a);
    let cb = rect_of(&a, HitTarget::PopupCheckbox);
    click(&mut a, cb.x, cb.y, Instant::now());
    let Mode::Popup(Popup::Add(f)) = &a.mode else {
        panic!("popup closed")
    };
    assert!(f.start_paused);
    assert_eq!(f.focus, AddField::Paused);

    // Delete confirmation: the files checkbox.
    let mut a = three();
    a.update(
        Msg::Key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE)),
        Instant::now(),
    );
    render(&mut a);
    let cb = rect_of(&a, HitTarget::PopupCheckbox);
    click(&mut a, cb.x, cb.y, Instant::now());
    assert!(matches!(
        a.mode,
        Mode::Popup(Popup::ConfirmDelete {
            delete_files: true,
            ..
        })
    ));
}

#[test]
fn wheel_scrolls_help() {
    let mut a = three();
    a.update(
        Msg::Key(KeyEvent::new(KeyCode::Char('?'), KeyModifiers::NONE)),
        Instant::now(),
    );
    render(&mut a);
    let p = rect_of(&a, HitTarget::PopupInside);
    a.update(
        mouse(
            MouseEventKind::ScrollDown,
            p.x + 2,
            p.y + 2,
            KeyModifiers::NONE,
        ),
        Instant::now(),
    );
    assert!(a.help_scroll > 0);
}

#[test]
fn altgr_chars_are_not_normal_mode_actions() {
    let mut a = three();
    let now = Instant::now();
    let altgr = KeyModifiers::CONTROL | KeyModifiers::ALT;
    for c in ['a', 'q', 'p', '/', 'd'] {
        let cmds = a.update(Msg::Key(KeyEvent::new(KeyCode::Char(c), altgr)), now);
        assert!(cmds.is_empty());
    }
    assert_eq!(a.mode, Mode::Normal);
    assert!(!a.should_quit);
    // Ctrl+C still quits.
    a.update(
        Msg::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
        now,
    );
    assert!(a.should_quit);
    // In a text input AltGr still types.
    let mut a = three();
    a.update(
        Msg::Key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE)),
        now,
    );
    a.update(Msg::Key(KeyEvent::new(KeyCode::Char('@'), altgr)), now);
    let Mode::Popup(Popup::Add(f)) = &a.mode else {
        panic!()
    };
    assert_eq!(f.source.value, "@");
}

fn drag_divider(a: &mut App, from: (u16, u16), to: (u16, u16), now: Instant) {
    click(a, from.0, from.1, now);
    let ev = |kind, (x, y)| mouse(kind, x, y, KeyModifiers::NONE);
    a.update(ev(MouseEventKind::Drag(MouseButton::Left), to), now);
    a.update(ev(MouseEventKind::Up(MouseButton::Left), to), now);
}

/// Wide layout: the list's width (the detail pane starts right after it).
fn list_width(a: &App) -> u16 {
    rect_of(a, HitTarget::List).width
}

#[test]
fn drag_wide_divider_resizes_panes() {
    let mut a = three();
    render_at(&mut a, 160, 40);
    let d = rect_of(&a, HitTarget::Divider);
    assert_eq!((d.width, list_width(&a)), (2, 92));
    let now = Instant::now();
    // Grabbed on the list's border, dropped 20 columns left: the border follows.
    drag_divider(&mut a, (d.x, 10), (d.x - 20, 12), now);
    render_at(&mut a, 160, 40);
    assert_eq!(list_width(&a), 72);
    // Moves after the release do nothing.
    a.update(
        mouse(
            MouseEventKind::Drag(MouseButton::Left),
            10,
            10,
            KeyModifiers::NONE,
        ),
        now,
    );
    render_at(&mut a, 160, 40);
    assert_eq!(list_width(&a), 72);
    // Dragged past the edge: the detail pane keeps its minimum.
    let d = rect_of(&a, HitTarget::Divider);
    drag_divider(
        &mut a,
        (d.x + 1, 10),
        (159, 10),
        now + Duration::from_secs(1),
    );
    render_at(&mut a, 160, 40);
    assert_eq!(list_width(&a), 130);
    // Scales with the terminal.
    render_at(&mut a, 320, 40);
    assert_eq!(list_width(&a), 260);
}

#[test]
fn double_click_divider_resets() {
    let mut a = three();
    render_at(&mut a, 160, 40);
    let d = rect_of(&a, HitTarget::Divider);
    let now = Instant::now();
    drag_divider(&mut a, (d.x, 10), (d.x - 20, 10), now);
    render_at(&mut a, 160, 40);
    let d = rect_of(&a, HitTarget::Divider);
    let later = now + Duration::from_secs(2);
    click(&mut a, d.x, 10, later);
    click(&mut a, d.x, 10, later + Duration::from_millis(200));
    render_at(&mut a, 160, 40);
    assert_eq!(list_width(&a), 92);
}

#[test]
fn drag_narrow_divider_resizes_panes() {
    let mut a = three();
    render_at(&mut a, 90, 40);
    let list = rect_of(&a, HitTarget::List);
    let d = rect_of(&a, HitTarget::Divider);
    assert_eq!((d.height, d.y), (2, list.bottom() - 1));
    // Grabbed on the detail pane's title row, dropped 5 rows up.
    drag_divider(&mut a, (30, d.y + 1), (30, d.y - 4), Instant::now());
    render_at(&mut a, 90, 40);
    assert_eq!(rect_of(&a, HitTarget::List).height, list.height - 5);
}

#[test]
fn divider_keys_move_it() {
    let mut a = three();
    render_at(&mut a, 160, 40);
    let key = |c| Msg::Key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
    a.update(key('<'), Instant::now());
    render_at(&mut a, 160, 40);
    assert_eq!(list_width(&a), 88);
    a.update(key('>'), Instant::now());
    a.update(key('>'), Instant::now());
    render_at(&mut a, 160, 40);
    assert_eq!(list_width(&a), 96);
    // Narrow: rows.
    render_at(&mut a, 90, 40);
    let h = rect_of(&a, HitTarget::List).height;
    a.update(key('>'), Instant::now());
    render_at(&mut a, 90, 40);
    assert_eq!(rect_of(&a, HitTarget::List).height, h + 1);
}

#[test]
fn press_beside_divider_does_not_drag() {
    let mut a = three();
    render_at(&mut a, 160, 40);
    let d = rect_of(&a, HitTarget::Divider);
    drag_divider(&mut a, (d.x - 5, 10), (d.x - 25, 10), Instant::now());
    render_at(&mut a, 160, 40);
    assert_eq!(list_width(&a), 92);
}
