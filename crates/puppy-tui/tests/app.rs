//! Behaviour of the dashboard's pure state machine (`App::update`).

use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use puppy_core::types::DownloadId;
use puppy_proto::{DownloadSnapshot, EngineKindTag, Event, Settings, Totals, WireState};
use puppy_tui::app::{App, Cmd, CmdOutcome, Mode, Msg, Popup, ToastKind};
use puppy_tui::theme::{Theme, ThemeChoice};

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
    }
}

fn queued(id: DownloadId, name: &str, pos: usize) -> DownloadSnapshot {
    DownloadSnapshot {
        queue_position: Some(pos),
        ..snap(id, name, WireState::Queued)
    }
}

fn settings(max_active: u16) -> Settings {
    Settings {
        max_active,
        global_down_limit: None,
        global_up_limit: None,
        download_dir: "/tmp".into(),
        connections: 8,
        theme: "auto".into(),
    }
}

fn app() -> App {
    App::new(Theme::detect(ThemeChoice::Dark, &|_| None))
}

fn connected(downloads: Vec<DownloadSnapshot>) -> (App, Instant) {
    let mut a = app();
    let now = Instant::now();
    a.update(
        Msg::Connected {
            version: "0.1.0".into(),
            settings: settings(3),
            downloads,
        },
        now,
    );
    (a, now)
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

fn shift(code: KeyCode) -> Msg {
    Msg::Key(KeyEvent::new(code, KeyModifiers::SHIFT))
}

fn ctrl(c: char) -> Msg {
    Msg::Key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL))
}

#[test]
fn connected_replaces_list_and_records_version() {
    let (a, _) = connected(vec![snap(1, "a", WireState::Active)]);
    assert_eq!(a.daemon_version.as_deref(), Some("0.1.0"));
    assert_eq!(a.downloads.len(), 1);
    assert_eq!(a.selected, Some(1));
    assert!(matches!(a.conn, puppy_tui::app::Conn::Connected));
}

#[test]
fn selection_follows_id_across_snapshots() {
    let (mut a, now) = connected(vec![
        snap(1, "a", WireState::Active),
        snap(2, "b", WireState::Active),
        snap(3, "c", WireState::Active),
    ]);
    a.update(ch('j'), now);
    assert_eq!(a.selected, Some(2));
    a.update(
        snapshot(vec![
            snap(3, "c", WireState::Active),
            snap(1, "a", WireState::Active),
            snap(2, "b", WireState::Active),
        ]),
        now,
    );
    assert_eq!(a.selected, Some(2));
    a.update(ch('k'), now);
    assert_eq!(a.selected, Some(1));
    a.update(ch('G'), now);
    assert_eq!(a.selected, Some(2));
    a.update(key(KeyCode::Home), now);
    assert_eq!(a.selected, Some(3));
}

#[test]
fn selection_falls_back_when_removed() {
    let (mut a, now) = connected(vec![
        snap(1, "a", WireState::Active),
        snap(2, "b", WireState::Active),
        snap(3, "c", WireState::Active),
    ]);
    a.update(key(KeyCode::Down), now);
    assert_eq!(a.selected, Some(2));
    // Same index (1) is now id 3.
    a.update(
        snapshot(vec![
            snap(1, "a", WireState::Active),
            snap(3, "c", WireState::Active),
        ]),
        now,
    );
    assert_eq!(a.selected, Some(3));
    // Index 1 gone ⇒ last.
    a.update(snapshot(vec![snap(1, "a", WireState::Active)]), now);
    assert_eq!(a.selected, Some(1));
    a.update(snapshot(vec![]), now);
    assert_eq!(a.selected, None);
}

#[test]
fn filter_matches_name_and_source_case_insensitive() {
    let mut by_source = snap(3, "file.bin", WireState::Active);
    by_source.source = "https://mirror.UBUNTU.com/file.bin".into();
    let (mut a, now) = connected(vec![
        snap(1, "Ubuntu-24.04.iso", WireState::Active),
        snap(2, "debian.iso", WireState::Active),
        by_source,
    ]);
    a.update(ch('/'), now);
    assert!(matches!(a.mode, Mode::Filter));
    for c in "uBuNtU".chars() {
        a.update(ch(c), now);
    }
    let ids: Vec<_> = a.visible().iter().map(|d| d.id).collect();
    assert_eq!(ids, vec![1, 3]);
    assert!(a.selected.is_some_and(|s| ids.contains(&s)));
    // Enter keeps the filter, Esc in normal mode clears it.
    a.update(key(KeyCode::Enter), now);
    assert!(matches!(a.mode, Mode::Normal));
    assert_eq!(a.visible().len(), 2);
    a.update(key(KeyCode::Esc), now);
    assert_eq!(a.visible().len(), 3);
}

#[test]
fn pause_on_completed_is_toast_not_cmd() {
    let (mut a, now) = connected(vec![snap(1, "a", WireState::Completed)]);
    let cmds = a.update(ch('p'), now);
    assert!(cmds.is_empty());
    let cmds = a.update(ch('r'), now);
    assert!(cmds.is_empty());
    let texts: Vec<_> = a.toasts.iter().map(|t| t.text.as_str()).collect();
    assert_eq!(
        texts,
        vec![
            "can't pause a completed download",
            "can't resume a completed download"
        ]
    );
    assert!(a.toasts.iter().all(|t| t.kind == ToastKind::Info));

    let (mut a, now) = connected(vec![snap(1, "a", WireState::Active)]);
    assert_eq!(a.update(ch('p'), now), vec![Cmd::Pause(1)]);
    let (mut a, now) = connected(vec![snap(1, "a", WireState::Paused)]);
    assert_eq!(a.update(ch('r'), now), vec![Cmd::Resume(1)]);
}

#[test]
fn delete_opens_confirm_then_remove_cmd() {
    let (mut a, now) = connected(vec![snap(7, "big.iso", WireState::Active)]);
    assert!(a.update(ch('d'), now).is_empty());
    match &a.mode {
        Mode::Popup(Popup::ConfirmDelete {
            id,
            name,
            delete_files,
        }) => {
            assert_eq!((*id, name.as_str(), *delete_files), (7, "big.iso", false));
        }
        m => panic!("unexpected mode {m:?}"),
    }
    a.update(ch('f'), now);
    a.update(ch(' '), now);
    a.update(ch('f'), now);
    let cmds = a.update(key(KeyCode::Enter), now);
    assert_eq!(
        cmds,
        vec![Cmd::Remove {
            id: 7,
            delete_files: true
        }]
    );
    assert!(matches!(a.mode, Mode::Normal));

    // Delete key (past the paste-burst guard), then Esc cancels.
    let now = now + LATER;
    a.update(key(KeyCode::Delete), now);
    assert!(matches!(a.mode, Mode::Popup(Popup::ConfirmDelete { .. })));
    assert!(a.update(key(KeyCode::Esc), now).is_empty());
    assert!(matches!(a.mode, Mode::Normal));
}

#[test]
fn restart_only_for_failed_or_attention() {
    let failed = WireState::Failed {
        reason: "x".into(),
        transient: false,
    };
    let attention = WireState::NeedsAttention {
        reason: puppy_proto::WireAttention::RemoteChanged,
    };
    for (state, ok) in [
        (failed, true),
        (attention, true),
        (WireState::Active, false),
        (WireState::Completed, false),
        (WireState::Paused, false),
        (WireState::Unknown, false),
    ] {
        let (mut a, now) = connected(vec![snap(4, "a", state.clone())]);
        let cmds = a.update(ch('R'), now);
        if ok {
            assert_eq!(cmds, vec![Cmd::Restart(4)], "{state:?}");
        } else {
            assert!(cmds.is_empty(), "{state:?}");
            assert_eq!(a.toasts.len(), 1, "{state:?}");
        }
    }
}

#[test]
fn unknown_state_allows_only_delete() {
    let (mut a, now) = connected(vec![DownloadSnapshot {
        queue_position: Some(1),
        ..snap(1, "a", WireState::Unknown)
    }]);
    for m in [ch('p'), ch('r'), ch('R'), ch('l'), ch('K'), ch('J')] {
        assert!(a.update(m, now).is_empty());
        assert!(matches!(a.mode, Mode::Normal));
    }
    a.update(ch('d'), now);
    assert!(matches!(a.mode, Mode::Popup(Popup::ConfirmDelete { .. })));
}

#[test]
fn max_active_plus_minus_clamped() {
    let (mut a, now) = connected(vec![]);
    assert_eq!(a.update(ch('+'), now), vec![Cmd::SetMaxActive(4)]);
    assert_eq!(a.update(ch('-'), now), vec![Cmd::SetMaxActive(3)]);
    a.update(ch('-'), now);
    assert_eq!(a.update(ch('-'), now), vec![Cmd::SetMaxActive(1)]);
    assert!(a.update(ch('-'), now).is_empty());
    assert_eq!(a.settings.as_ref().unwrap().max_active, 1);

    let mut a = app();
    a.update(
        Msg::Connected {
            version: "v".into(),
            settings: settings(32),
            downloads: vec![],
        },
        now,
    );
    assert!(a.update(ch('+'), now).is_empty());
    assert_eq!(a.update(ch('-'), now), vec![Cmd::SetMaxActive(31)]);

    // No settings yet ⇒ nothing.
    let mut a = app();
    assert!(a.update(ch('+'), now).is_empty());
}

#[test]
fn move_shift_arrows_emit_move_cmd() {
    let (mut a, now) = connected(vec![
        queued(1, "a", 0),
        queued(2, "b", 1),
        queued(3, "c", 2),
        snap(4, "d", WireState::Active),
    ]);
    a.update(ch('j'), now);
    assert_eq!(
        a.update(shift(KeyCode::Up), now),
        vec![Cmd::Move { id: 2, position: 0 }]
    );
    assert_eq!(
        a.update(shift(KeyCode::Down), now),
        vec![Cmd::Move { id: 2, position: 2 }]
    );
    assert_eq!(
        a.update(ch('K'), now),
        vec![Cmd::Move { id: 2, position: 0 }]
    );
    assert_eq!(
        a.update(ch('J'), now),
        vec![Cmd::Move { id: 2, position: 2 }]
    );
    // First in queue can't move up; last can't move down; active can't move.
    a.update(ch('g'), now);
    assert!(a.update(ch('K'), now).is_empty());
    a.update(ch('j'), now);
    a.update(ch('j'), now);
    assert_eq!(a.selected, Some(3));
    assert!(a.update(ch('J'), now).is_empty());
    a.update(ch('j'), now);
    assert!(a.update(ch('K'), now).is_empty());
    assert!(a.toasts.len() >= 2);
}

#[test]
fn cmd_error_becomes_toast_and_expires() {
    let (mut a, now) = connected(vec![]);
    a.update(
        Msg::CmdResult(CmdOutcome {
            label: "pause".into(),
            result: Err("not found".into()),
        }),
        now,
    );
    a.update(
        Msg::CmdResult(CmdOutcome {
            label: "resume".into(),
            result: Ok(()),
        }),
        now,
    );
    assert_eq!(a.toasts.len(), 1);
    assert_eq!(a.toasts[0].kind, ToastKind::Error);
    assert!(a.toasts[0].text.contains("not found"));
    a.update(Msg::Tick, now + Duration::from_millis(3900));
    assert_eq!(a.toasts.len(), 1);
    a.update(Msg::Tick, now + Duration::from_secs(4));
    assert!(a.toasts.is_empty());

    for i in 0..5 {
        a.update(
            Msg::CmdResult(CmdOutcome {
                label: format!("op{i}"),
                result: Err("e".into()),
            }),
            now,
        );
    }
    assert_eq!(a.toasts.len(), 3);
    assert!(a.toasts[0].text.contains("op2"));
}

#[test]
fn ctrl_c_quits_from_any_mode() {
    let opens = [
        None,
        Some(ch('/')),
        Some(ch('a')),
        Some(ch('?')),
        Some(ch('L')),
    ];
    for open in opens {
        let (mut a, now) = connected(vec![snap(1, "a", WireState::Active)]);
        if let Some(m) = open {
            a.update(m, now);
        }
        let cmds = a.update(ctrl('c'), now);
        assert_eq!(cmds, vec![Cmd::Quit]);
        assert!(a.should_quit);
    }
    // `q` quits in normal mode but is text in the add popup.
    let (mut a, now) = connected(vec![]);
    assert_eq!(a.update(ch('q'), now), vec![Cmd::Quit]);
    let (mut a, now) = connected(vec![]);
    a.update(ch('a'), now);
    assert!(a.update(ch('q'), now).is_empty());
    assert!(!a.should_quit);
}

#[test]
fn history_ring_caps_at_120() {
    let (mut a, now) = connected(vec![]);
    for i in 0..130u64 {
        let mut d = snap(1, "a", WireState::Active);
        d.down_bps = i;
        a.update(
            Msg::Daemon(Event::Snapshot {
                downloads: vec![d],
                totals: Totals {
                    down_bps: i * 2,
                    ..Totals::default()
                },
            }),
            // One sample per second.
            now + Duration::from_secs(i),
        );
    }
    let h = a.history.get(1).unwrap();
    assert_eq!(h.len(), 120);
    assert_eq!(h.front(), Some(&10));
    assert_eq!(h.back(), Some(&129));
    assert_eq!(a.history.global().len(), 120);
    assert_eq!(a.history.global().back(), Some(&258));
    a.update(snapshot(vec![]), now);
    assert!(a.history.get(1).is_none());
}

#[test]
fn tick_fetches_detail_for_running_selection() {
    let (mut a, now) = connected(vec![snap(1, "a", WireState::Active)]);
    assert_eq!(a.update(Msg::Tick, now), vec![Cmd::FetchDetail(1)]);
    let (mut a, now) = connected(vec![snap(1, "a", WireState::Completed)]);
    assert!(a.update(Msg::Tick, now).is_empty());
}

#[test]
fn disconnected_reconnects() {
    let (mut a, now) = connected(vec![]);
    a.update(Msg::Disconnected, now);
    assert!(matches!(
        a.conn,
        puppy_tui::app::Conn::Reconnecting { attempt: 1 }
    ));
}

#[test]
fn add_popup_submits_add_cmd() {
    let (mut a, now) = connected(vec![]);
    a.update(ch('a'), now);
    // Empty source ⇒ error, popup stays.
    assert!(a.update(key(KeyCode::Enter), now).is_empty());
    match &a.mode {
        Mode::Popup(Popup::Add(f)) => assert_eq!(
            f.error.as_deref(),
            Some("enter a URL, magnet link or .torrent path")
        ),
        m => panic!("unexpected {m:?}"),
    }
    a.update(Msg::Paste("https://example.com/f.iso\nsecond".into()), now);
    a.update(key(KeyCode::Tab), now);
    for c in "/data".chars() {
        a.update(ch(c), now);
    }
    a.update(key(KeyCode::Tab), now);
    a.update(ch(' '), now);
    let cmds = a.update(key(KeyCode::Enter), now);
    match cmds.as_slice() {
        [
            Cmd::Add {
                request,
                start_paused,
            },
        ] => {
            assert!(*start_paused);
            assert_eq!(
                request.source,
                puppy_core::types::Source::Http {
                    url: "https://example.com/f.iso".parse().unwrap()
                }
            );
            // On Windows a rooted path gets the current drive.
            assert_eq!(
                request.output_dir,
                Some(std::path::absolute("/data").unwrap())
            );
        }
        c => panic!("unexpected {c:?}"),
    }
    assert!(matches!(a.mode, Mode::Normal));
}

#[test]
fn limit_popups_parse_rates() {
    let (mut a, now) = connected(vec![snap(5, "a", WireState::Active)]);
    a.update(ch('l'), now);
    for c in "5M".chars() {
        a.update(ch(c), now);
    }
    assert_eq!(
        a.update(key(KeyCode::Enter), now),
        vec![Cmd::SetLimit {
            id: 5,
            bps: Some(5 << 20)
        }]
    );

    let now = now + LATER;
    a.update(ch('l'), now);
    for c in "abc".chars() {
        a.update(ch(c), now);
    }
    assert!(a.update(key(KeyCode::Enter), now).is_empty());
    assert!(matches!(&a.mode, Mode::Popup(Popup::Limit(f)) if f.error.is_some()));
    a.update(key(KeyCode::Esc), now);

    a.update(ch('L'), now);
    for c in "1k".chars() {
        a.update(ch(c), now);
    }
    a.update(key(KeyCode::Tab), now);
    for c in "none".chars() {
        a.update(ch(c), now);
    }
    assert_eq!(
        a.update(key(KeyCode::Enter), now),
        vec![Cmd::SetGlobalLimits {
            down: Some(1024),
            up: None
        }]
    );
}

#[test]
fn transient_failure_does_not_toast_and_permanent_toasts_once() {
    let (mut a, now) = connected(vec![snap(1, "a", WireState::Active)]);
    let failed = |transient| {
        Msg::Daemon(Event::Failed {
            id: 1,
            reason: "503".into(),
            transient,
        })
    };
    a.update(failed(true), now);
    a.update(failed(true), now + Duration::from_secs(1));
    assert!(a.toasts.is_empty());
    a.update(failed(false), now);
    a.update(failed(false), now + Duration::from_secs(1));
    assert_eq!(a.toasts.len(), 1);
    assert_eq!(a.toasts[0].kind, ToastKind::Error);
    // Once it has expired, the same text may show again.
    a.update(failed(false), now + Duration::from_secs(5));
    a.update(Msg::Tick, now + Duration::from_secs(5));
    assert_eq!(a.toasts.len(), 1);
}

#[test]
fn selection_survives_filter_hiding_everything() {
    let (mut a, now) = connected(vec![
        snap(1, "a", WireState::Active),
        snap(2, "b", WireState::Active),
        snap(3, "c", WireState::Active),
    ]);
    a.update(ch('G'), now);
    assert_eq!(a.selected, Some(3));
    a.update(ch('/'), now);
    for c in "zzz".chars() {
        a.update(ch(c), now);
    }
    assert!(a.visible().is_empty());
    assert_eq!(a.selected, Some(3));
    // Hidden selection is not acted on.
    a.update(key(KeyCode::Enter), now);
    assert!(a.update(ch('p'), now).is_empty());
    assert!(matches!(a.mode, Mode::Normal));
    // A snapshot while everything is hidden keeps it too.
    a.update(
        snapshot(vec![
            snap(1, "a", WireState::Active),
            snap(2, "b", WireState::Active),
            snap(3, "c", WireState::Active),
        ]),
        now,
    );
    assert_eq!(a.selected, Some(3));
    a.update(key(KeyCode::Esc), now);
    assert_eq!(a.visible().len(), 3);
    assert_eq!(a.selected, Some(3));
}

#[test]
fn keymap_chars_are_text_in_filter_and_add_popup() {
    let typed = "qdj/?pR+";
    // Filter.
    let (mut a, now) = connected(vec![snap(1, "a", WireState::Active)]);
    a.update(ch('/'), now);
    for c in typed.chars() {
        assert!(a.update(ch(c), now).is_empty(), "{c}");
    }
    assert!(!a.should_quit);
    assert!(matches!(a.mode, Mode::Filter));
    assert_eq!(a.filter.value, typed);
    // Add popup.
    let (mut a, now) = connected(vec![snap(1, "a", WireState::Active)]);
    a.update(ch('a'), now);
    for c in typed.chars() {
        assert!(a.update(ch(c), now).is_empty(), "{c}");
    }
    assert!(!a.should_quit);
    match &a.mode {
        Mode::Popup(Popup::Add(f)) => assert_eq!(f.source.value, typed),
        m => panic!("unexpected {m:?}"),
    }
    assert_eq!(a.settings.as_ref().unwrap().max_active, 3);
}

#[test]
fn navigation_on_empty_list_is_a_no_op() {
    let (mut a, now) = connected(vec![]);
    for m in [
        ch('j'),
        ch('k'),
        ch('g'),
        ch('G'),
        key(KeyCode::Up),
        key(KeyCode::Down),
        key(KeyCode::Home),
        key(KeyCode::End),
        ch('p'),
        ch('r'),
        ch('d'),
        ch('R'),
        ch('l'),
        ch('K'),
        ch('J'),
    ] {
        assert!(a.update(m, now).is_empty());
    }
    assert_eq!(a.selected, None);
    assert!(matches!(a.mode, Mode::Normal));
    assert!(a.toasts.is_empty());
}

#[test]
fn confirm_delete_space_toggles_and_only_enter_confirms() {
    let (mut a, now) = connected(vec![snap(7, "x", WireState::Active)]);
    a.update(ch('d'), now);
    a.update(ch(' '), now);
    assert!(matches!(
        a.mode,
        Mode::Popup(Popup::ConfirmDelete {
            delete_files: true,
            ..
        })
    ));
    assert!(a.update(ch('y'), now).is_empty());
    assert!(matches!(a.mode, Mode::Popup(Popup::ConfirmDelete { .. })));
    assert_eq!(
        a.update(key(KeyCode::Enter), now),
        vec![Cmd::Remove {
            id: 7,
            delete_files: true
        }]
    );
}

// ---------------------------------------------------------------------------------
// Final fix wave

fn torrent(id: DownloadId, name: &str, state: WireState) -> DownloadSnapshot {
    DownloadSnapshot {
        kind: EngineKindTag::Torrent,
        ..snap(id, name, state)
    }
}

fn http_detail(connections: u16) -> puppy_core::types::EngineDetail {
    puppy_core::types::EngineDetail::Http {
        segments: Vec::new(),
        connections,
        resumable: true,
    }
}

/// Past the paste-burst guard after a popup submit.
const LATER: Duration = Duration::from_millis(100);

#[test]
fn global_limits_submit_is_optimistic_and_refresh_replaces_settings() {
    let (mut a, now) = connected(vec![]);
    a.update(ch('L'), now);
    for c in "2M".chars() {
        a.update(ch(c), now);
    }
    assert_eq!(
        a.update(key(KeyCode::Enter), now),
        vec![Cmd::SetGlobalLimits {
            down: Some(2 << 20),
            up: None
        }]
    );
    let s = a.settings.as_ref().unwrap();
    assert_eq!(
        (s.global_down_limit, s.global_up_limit),
        (Some(2 << 20), None)
    );

    // The daemon's answer wins (here it normalised the limit and set max 5).
    let mut fresh = settings(5);
    fresh.global_down_limit = Some(3 << 20);
    a.update(Msg::Settings(fresh.clone()), now);
    assert_eq!(a.settings, Some(fresh));
    // Reopening `L` shows the new values.
    a.update(ch('L'), now + LATER);
    match &a.mode {
        Mode::Popup(Popup::GlobalLimits(f)) => {
            assert_eq!(f.down.value, "3.0M");
            assert_eq!(f.up.value, "");
        }
        m => panic!("{m:?}"),
    }
}

#[test]
fn selecting_a_running_item_fetches_detail_at_once() {
    let (mut a, now) = connected(vec![
        snap(1, "a", WireState::Active),
        snap(2, "b", WireState::Completed),
        snap(3, "c", WireState::Paused),
    ]);
    assert_eq!(a.update(ch('j'), now), Vec::<Cmd>::new());
    assert_eq!(a.update(ch('j'), now), vec![Cmd::FetchDetail(3)]);
    assert_eq!(a.update(ch('g'), now), vec![Cmd::FetchDetail(1)]);
    // Not moving: no fetch (ticks do the polling).
    assert_eq!(a.update(ch('k'), now), Vec::<Cmd>::new());
}

#[test]
fn detail_cache_shows_last_detail_when_moving_back() {
    let (mut a, now) = connected(vec![
        snap(1, "a", WireState::Active),
        snap(2, "b", WireState::Active),
    ]);
    a.update(Msg::Detail(1, Some(http_detail(1))), now);
    a.update(ch('j'), now);
    assert!(a.detail.is_none());
    a.update(Msg::Detail(2, Some(http_detail(2))), now);
    assert_eq!(a.detail, Some((2, http_detail(2))));
    a.update(ch('k'), now);
    assert_eq!(a.detail, Some((1, http_detail(1))));
    a.update(ch('j'), now);
    assert_eq!(a.detail, Some((2, http_detail(2))));

    // A removed download's entry is dropped.
    a.update(snapshot(vec![snap(1, "a", WireState::Active)]), now);
    assert!(a.cached_detail(2).is_none());
    assert!(a.cached_detail(1).is_some());
}

#[test]
fn detail_cache_keeps_the_last_32() {
    let downloads: Vec<_> = (1..=40)
        .map(|i| snap(i, &format!("d{i}"), WireState::Active))
        .collect();
    let (mut a, now) = connected(downloads);
    for i in 1..=40 {
        a.update(Msg::Detail(i, Some(http_detail(1))), now);
    }
    assert!((1..=8).all(|i| a.cached_detail(i).is_none()));
    assert!((9..=40).all(|i| a.cached_detail(i).is_some()));
    // Use refreshes: 9 survives the next insert, 10 goes.
    a.update(Msg::Detail(9, Some(http_detail(2))), now);
    a.update(Msg::Detail(1, Some(http_detail(1))), now);
    assert!(a.cached_detail(9).is_some());
    assert!(a.cached_detail(10).is_none());
}

#[test]
fn detail_dropped_when_the_selection_stops_running() {
    let (mut a, now) = connected(vec![snap(1, "a", WireState::Active)]);
    a.update(Msg::Detail(1, Some(http_detail(4))), now);
    assert!(a.detail.is_some());
    a.update(snapshot(vec![snap(1, "a", WireState::Completed)]), now);
    assert!(a.detail.is_none());
    assert!(a.cached_detail(1).is_none());
    // A late reply for it is not shown either.
    a.update(Msg::Detail(1, Some(http_detail(4))), now);
    assert!(a.detail.is_none());
}

#[test]
fn keys_right_after_a_popup_submit_are_a_paste_burst() {
    let (mut a, now) = connected(vec![snap(1, "a", WireState::Active)]);
    a.update(ch('l'), now);
    for c in "5M".chars() {
        a.update(ch(c), now);
    }
    assert_eq!(a.update(key(KeyCode::Enter), now).len(), 1);
    // The rest of the paste (`\n` then more text) arrives within 30 ms: swallowed.
    let soon = now + Duration::from_millis(10);
    assert!(a.update(ch('p'), soon).is_empty());
    assert!(a.update(ch('q'), soon).is_empty());
    assert!(!a.should_quit);
    assert_eq!(a.mode, Mode::Normal);
    // Ctrl+C still quits.
    let mut b = a.clone();
    b.update(ctrl('c'), soon);
    assert!(b.should_quit);
    // After 30 ms keys work again.
    let later = now + Duration::from_millis(31);
    assert_eq!(a.update(ch('p'), later), vec![Cmd::Pause(1)]);
}

#[test]
fn unsupported_error_is_a_friendly_info_toast() {
    let (mut a, now) = connected(vec![]);
    a.update(
        Msg::CmdResult(CmdOutcome {
            label: "limit".into(),
            result: Err(puppy_tui::app::CmdError::Unsupported(
                "per-torrent limits while running".into(),
            )),
        }),
        now,
    );
    assert_eq!(
        a.toasts[0].text,
        "not supported for this download: per-torrent limits while running"
    );
    assert_eq!(a.toasts[0].kind, ToastKind::Info);
}

#[test]
fn torrent_limit_only_before_it_starts() {
    let (mut a, now) = connected(vec![
        torrent(1, "t", WireState::Active),
        torrent(2, "q", WireState::Queued),
    ]);
    let running = a.downloads[0].clone();
    let queued = a.downloads[1].clone();
    assert!(!a.can(puppy_tui::app::Action::Limit, &running));
    assert!(a.can(puppy_tui::app::Action::Limit, &queued));
    assert!(a.update(ch('l'), now).is_empty());
    assert_eq!(a.mode, Mode::Normal);
    assert_eq!(
        a.toasts[0].text,
        "per-torrent limits can only be set before a torrent starts"
    );
    assert_eq!(a.toasts[0].kind, ToastKind::Info);
    a.update(ch('j'), now);
    a.update(ch('l'), now);
    assert!(matches!(a.mode, Mode::Popup(Popup::Limit(_))));
}

#[test]
fn quit_message_quits_from_any_mode() {
    let (mut a, now) = connected(vec![]);
    a.update(ch('a'), now);
    assert_eq!(a.update(Msg::Quit, now), vec![Cmd::Quit]);
    assert!(a.should_quit);
}
