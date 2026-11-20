//! The runtime loop against a scripted [`FakeApi`] (tokio's paused clock: timers fire
//! as soon as everything waits, so these are fast and deterministic) and, in
//! `real_daemon_smoke`, against an in-process daemon over a real socket.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use futures_util::Stream;
use puppy_core::types::{DownloadId, EngineDetail};
use puppy_proto::{DownloadSnapshot, EngineKindTag, Event, Settings, Totals, WireState};
use puppy_tui::api::{ApiError, Connected, DaemonApi};
use puppy_tui::app::{App, Banner, Cmd, CmdError, Conn, Msg};
use puppy_tui::runtime::run_with;
use puppy_tui::theme::{Theme, ThemeChoice};
use ratatui::Terminal;
use ratatui::backend::{Backend, ClearType, TestBackend, WindowSize};
use ratatui::buffer::{Buffer, Cell};
use ratatui::layout::{Position, Size};
use tokio::sync::mpsc;
use tokio::time::{Instant, sleep};
use unicode_width::UnicodeWidthStr;

// ---------------------------------------------------------------------------------
// Fixtures

fn theme() -> Theme {
    Theme::detect(ThemeChoice::Dark, &|_| None)
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
        done_bytes: 100,
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
    }
}

fn settings() -> Settings {
    Settings {
        max_active: 3,
        global_down_limit: None,
        global_up_limit: None,
        download_dir: "/srv".into(),
        connections: 8,
        theme: "auto".into(),
    }
}

fn ok(downloads: Vec<DownloadSnapshot>) -> Result<Connected, ApiError> {
    Ok(("9.9.9".into(), settings(), downloads))
}

fn key(c: char) -> Msg {
    Msg::Key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE))
}

fn ctrl_c() -> Msg {
    Msg::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL))
}

/// What the fake does for one executed command.
enum Exec {
    Reply(Result<Option<Msg>, CmdError>),
    /// Replies after this long.
    Slow(Duration, Result<Option<Msg>, CmdError>),
    Hang,
}

/// A scripted daemon: connect results in order (then `fallback`, or hanging when there
/// is none), events pushed by the test (`None` closes the connection), a record of the
/// executed commands.
struct FakeApi {
    connects: Mutex<VecDeque<Result<Connected, ApiError>>>,
    fallback: Mutex<Option<Result<Connected, ApiError>>>,
    /// Becomes the fallback when `RestartDaemon` runs.
    after_restart: Mutex<Option<Result<Connected, ApiError>>>,
    /// Script loaded into `connects` when `RestartDaemon` runs.
    after_restart_script: Mutex<Vec<Result<Connected, ApiError>>>,
    /// How long each connect takes (its result is decided when it starts).
    connect_delay: Duration,
    connect_panics: bool,
    connect_count: AtomicUsize,
    events_tx: mpsc::UnboundedSender<Option<Event>>,
    events_rx: tokio::sync::Mutex<mpsc::UnboundedReceiver<Option<Event>>>,
    executed: Mutex<Vec<Cmd>>,
    exec: Box<dyn Fn(&Cmd) -> Exec + Send + Sync>,
}

impl FakeApi {
    fn new(connects: Vec<Result<Connected, ApiError>>) -> Self {
        let (events_tx, events_rx) = mpsc::unbounded_channel();
        FakeApi {
            connects: Mutex::new(connects.into()),
            fallback: Mutex::new(None),
            after_restart: Mutex::new(None),
            after_restart_script: Mutex::new(Vec::new()),
            connect_delay: Duration::ZERO,
            connect_panics: false,
            connect_count: AtomicUsize::new(0),
            events_tx,
            events_rx: tokio::sync::Mutex::new(events_rx),
            executed: Mutex::new(Vec::new()),
            exec: Box::new(|_| Exec::Reply(Ok(None))),
        }
    }

    fn with_exec(mut self, f: impl Fn(&Cmd) -> Exec + Send + Sync + 'static) -> Self {
        self.exec = Box::new(f);
        self
    }

    fn with_fallback(self, r: Result<Connected, ApiError>) -> Self {
        *self.fallback.lock().unwrap() = Some(r);
        self
    }

    fn connects(&self) -> usize {
        self.connect_count.load(Ordering::SeqCst)
    }

    /// Executed commands other than detail fetches (ticks add those).
    fn actions(&self) -> Vec<Cmd> {
        self.executed
            .lock()
            .unwrap()
            .iter()
            .filter(|c| !matches!(c, Cmd::FetchDetail(_)))
            .cloned()
            .collect()
    }
}

#[async_trait]
impl DaemonApi for FakeApi {
    async fn connect(&self) -> Result<Connected, ApiError> {
        self.connect_count.fetch_add(1, Ordering::SeqCst);
        assert!(!self.connect_panics, "connect panics");
        let next = self.connects.lock().unwrap().pop_front();
        let next = next.or_else(|| self.fallback.lock().unwrap().clone());
        sleep(self.connect_delay).await;
        match next {
            Some(r) => r,
            None => std::future::pending().await,
        }
    }

    async fn next_event(&self) -> Option<Event> {
        match self.events_rx.lock().await.recv().await {
            Some(e) => e,
            None => std::future::pending().await,
        }
    }

    async fn exec(&self, cmd: Cmd) -> Result<Option<Msg>, CmdError> {
        self.executed.lock().unwrap().push(cmd.clone());
        if cmd == Cmd::RestartDaemon {
            let next = self.after_restart.lock().unwrap().take();
            if next.is_some() {
                *self.fallback.lock().unwrap() = next;
            }
            let script = std::mem::take(&mut *self.after_restart_script.lock().unwrap());
            self.connects.lock().unwrap().extend(script);
        }
        match (self.exec)(&cmd) {
            Exec::Reply(r) => r,
            Exec::Slow(d, r) => {
                sleep(d).await;
                r
            }
            Exec::Hang => std::future::pending().await,
        }
    }
}

/// A [`TestBackend`] that keeps the text of every frame drawn.
struct Recorder {
    inner: TestBackend,
    frames: Arc<Mutex<Vec<String>>>,
}

impl Recorder {
    fn terminal(w: u16, h: u16) -> (Terminal<Recorder>, Arc<Mutex<Vec<String>>>) {
        let frames = Arc::new(Mutex::new(Vec::new()));
        let backend = Recorder {
            inner: TestBackend::new(w, h),
            frames: frames.clone(),
        };
        (Terminal::new(backend).unwrap(), frames)
    }
}

fn text(buf: &Buffer) -> String {
    let a = buf.area;
    let mut s = String::new();
    for y in a.y..a.bottom() {
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
        s.push('\n');
    }
    s
}

impl Backend for Recorder {
    type Error = std::convert::Infallible;

    fn draw<'a, I>(&mut self, content: I) -> Result<(), Self::Error>
    where
        I: Iterator<Item = (u16, u16, &'a Cell)>,
    {
        self.inner.draw(content)
    }
    fn hide_cursor(&mut self) -> Result<(), Self::Error> {
        self.inner.hide_cursor()
    }
    fn show_cursor(&mut self) -> Result<(), Self::Error> {
        self.inner.show_cursor()
    }
    fn get_cursor_position(&mut self) -> Result<Position, Self::Error> {
        self.inner.get_cursor_position()
    }
    fn set_cursor_position<P: Into<Position>>(&mut self, p: P) -> Result<(), Self::Error> {
        self.inner.set_cursor_position(p)
    }
    fn clear(&mut self) -> Result<(), Self::Error> {
        self.inner.clear()
    }
    fn clear_region(&mut self, t: ClearType) -> Result<(), Self::Error> {
        self.inner.clear_region(t)
    }
    fn size(&self) -> Result<Size, Self::Error> {
        self.inner.size()
    }
    fn window_size(&mut self) -> Result<WindowSize, Self::Error> {
        self.inner.window_size()
    }
    fn flush(&mut self) -> Result<(), Self::Error> {
        self.frames.lock().unwrap().push(text(self.inner.buffer()));
        self.inner.flush()
    }
}

fn last(frames: &Mutex<Vec<String>>) -> String {
    frames.lock().unwrap().last().cloned().unwrap_or_default()
}

fn any_frame(frames: &Mutex<Vec<String>>, needle: &str) -> bool {
    frames.lock().unwrap().iter().any(|f| f.contains(needle))
}

/// A channel-fed input stream (stays open while the sender lives).
fn input() -> (
    mpsc::UnboundedSender<Msg>,
    impl Stream<Item = Msg> + Unpin + use<>,
) {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let stream = futures_util::stream::poll_fn(move |cx| rx.poll_recv(cx));
    (tx, stream)
}

/// Sends `msgs` to `tx` after `delay` (on the paused clock).
fn send_after(tx: &mpsc::UnboundedSender<Msg>, delay: Duration, msgs: Vec<Msg>) {
    let tx = tx.clone();
    tokio::spawn(async move {
        sleep(delay).await;
        for m in msgs {
            tx.send(m).unwrap();
        }
    });
}

async fn run_for(
    api: &Arc<FakeApi>,
    input: impl Stream<Item = Msg> + Unpin,
    secs: f64,
) -> (App, Arc<Mutex<Vec<String>>>) {
    let (mut term, frames) = Recorder::terminal(120, 40);
    let api: Arc<dyn DaemonApi> = api.clone();
    let app = run_with(
        api,
        theme(),
        &mut term,
        input,
        Some(Duration::from_secs_f64(secs)),
    )
    .await
    .unwrap();
    (app, frames)
}

// ---------------------------------------------------------------------------------
// Fake daemon

#[tokio::test(start_paused = true)]
async fn startup_connects_and_renders_list() {
    let api = Arc::new(FakeApi::new(vec![ok(vec![
        snap(1, "ubuntu.iso", WireState::Active),
        snap(2, "debian.iso", WireState::Paused),
    ])]));
    let (_tx, input) = input();
    let (app, frames) = run_for(&api, input, 1.0).await;
    assert_eq!(app.conn, Conn::Connected);
    assert_eq!(app.daemon_version.as_deref(), Some("9.9.9"));
    assert_eq!(app.downloads.len(), 2);
    let f = last(&frames);
    assert!(f.contains("ubuntu.iso"), "{f}");
    assert!(f.contains("debian.iso"), "{f}");
    assert!(f.contains("daemon v9.9.9"), "{f}");
    assert!(f.contains("● connected"), "{f}");
    assert_eq!(api.connects(), 1);
    // Throttled: a 1 s run with a handful of messages draws a handful of frames.
    let n = frames.lock().unwrap().len();
    assert!((1..=10).contains(&n), "{n} frames");
}

#[tokio::test(start_paused = true)]
async fn idle_dashboard_redraws_only_on_ticks() {
    let api = Arc::new(FakeApi::new(vec![ok(vec![snap(
        1,
        "a.bin",
        WireState::Paused,
    )])]));
    let (_tx, input) = input();
    let (_, frames) = run_for(&api, input, 10.0).await;
    // 20 ticks in 10 s, nothing else: one frame per tick, plus the first ones.
    let n = frames.lock().unwrap().len();
    assert!((18..=24).contains(&n), "{n} frames");
}

#[tokio::test(start_paused = true)]
async fn snapshot_events_update_the_list() {
    let api = Arc::new(FakeApi::new(vec![ok(vec![snap(
        1,
        "a.bin",
        WireState::Active,
    )])]));
    let events = api.events_tx.clone();
    tokio::spawn(async move {
        sleep(Duration::from_millis(300)).await;
        events
            .send(Some(Event::Snapshot {
                downloads: vec![
                    snap(1, "a.bin", WireState::Completed),
                    snap(2, "b.bin", WireState::Queued),
                ],
                totals: Totals::default(),
            }))
            .unwrap();
    });
    let (_tx, input) = input();
    let (app, frames) = run_for(&api, input, 1.0).await;
    assert_eq!(app.downloads.len(), 2);
    let f = last(&frames);
    assert!(f.contains("b.bin") && f.contains('✓'), "{f}");
}

#[tokio::test(start_paused = true)]
async fn bursts_are_coalesced() {
    let api = Arc::new(FakeApi::new(vec![ok(Vec::new())]));
    let events = api.events_tx.clone();
    tokio::spawn(async move {
        sleep(Duration::from_millis(200)).await;
        for i in 0..100 {
            let downloads = vec![snap(1, &format!("v{i}.bin"), WireState::Active)];
            let e = Event::Snapshot {
                downloads,
                totals: Totals::default(),
            };
            events.send(Some(e)).unwrap();
        }
    });
    let (_tx, input) = input();
    // Before the first tick.
    let (app, frames) = run_for(&api, input, 0.45).await;
    assert_eq!(app.downloads[0].name, "v99.bin");
    let n = frames.lock().unwrap().len();
    assert!(n <= 4, "{n} frames");
    assert!(last(&frames).contains("v99.bin"));
}

#[tokio::test(start_paused = true)]
async fn key_cmd_executed_via_api() {
    let api = Arc::new(FakeApi::new(vec![ok(vec![snap(
        1,
        "a.bin",
        WireState::Active,
    )])]));
    let (tx, input) = input();
    send_after(&tx, Duration::from_millis(200), vec![key('p')]);
    let (app, _) = run_for(&api, input, 1.0).await;
    assert_eq!(api.actions(), vec![Cmd::Pause(1)]);
    assert!(app.toasts.is_empty(), "{:?}", app.toasts);
}

#[tokio::test(start_paused = true)]
async fn cmd_error_toast_shown() {
    let api = Arc::new(
        FakeApi::new(vec![ok(vec![snap(1, "a.bin", WireState::Active)])]).with_exec(|c| match c {
            Cmd::Pause(_) => Exec::Reply(Err("the daemon said no".into())),
            _ => Exec::Reply(Ok(None)),
        }),
    );
    let (tx, input) = input();
    send_after(&tx, Duration::from_millis(200), vec![key('p')]);
    let (app, frames) = run_for(&api, input, 1.0).await;
    assert_eq!(app.toasts.len(), 1);
    assert_eq!(app.toasts[0].text, "pause: the daemon said no");
    let f = last(&frames);
    assert!(f.contains("pause: the daemon said no"), "{f}");
}

#[tokio::test(start_paused = true)]
async fn detail_fetched_on_tick() {
    let detail = EngineDetail::Http {
        segments: Vec::new(),
        connections: 4,
        resumable: true,
    };
    let reply = detail.clone();
    let api = Arc::new(
        FakeApi::new(vec![ok(vec![snap(1, "a.bin", WireState::Active)])]).with_exec(
            move |c| match c {
                Cmd::FetchDetail(id) => {
                    Exec::Reply(Ok(Some(Msg::Detail(*id, Some(reply.clone())))))
                }
                _ => Exec::Reply(Ok(None)),
            },
        ),
    );
    let (_tx, input) = input();
    // Selecting it on connect fetches at once, before the first tick (500 ms).
    let (app, _) = run_for(&api, input, 0.4).await;
    assert!(matches!(app.detail, Some((1, _))), "{:?}", app.detail);
    let fetches = |api: &FakeApi| {
        api.executed
            .lock()
            .unwrap()
            .iter()
            .filter(|c| **c == Cmd::FetchDetail(1))
            .count()
    };
    assert_eq!(fetches(&api), 1);
    let (_tx, input2) = self::input();
    let api2 = Arc::new(
        FakeApi::new(vec![ok(vec![snap(1, "a.bin", WireState::Active)])]).with_exec(
            move |c| match c {
                Cmd::FetchDetail(id) => {
                    Exec::Reply(Ok(Some(Msg::Detail(*id, Some(detail.clone())))))
                }
                _ => Exec::Reply(Ok(None)),
            },
        ),
    );
    let (app, _) = run_for(&api2, input2, 0.7).await;
    assert!(
        matches!(app.detail, Some((1, EngineDetail::Http { .. }))),
        "{:?}",
        app.detail
    );
    // Then every tick polls it.
    assert_eq!(fetches(&api2), 2);
    // A failing detail fetch is not a toast.
    let api3 = Arc::new(
        FakeApi::new(vec![ok(vec![snap(1, "a.bin", WireState::Active)])])
            .with_exec(|_| Exec::Reply(Err("gone".into()))),
    );
    let (_tx, input3) = self::input();
    let (app, _) = run_for(&api3, input3, 2.0).await;
    assert!(app.toasts.is_empty(), "{:?}", app.toasts);
}

#[tokio::test(start_paused = true)]
async fn reconnects_after_daemon_restart() {
    let api = Arc::new(FakeApi::new(vec![
        ok(vec![snap(1, "old.bin", WireState::Active)]),
        Err(ApiError::Other("connection refused".into())),
        ok(vec![
            snap(1, "old.bin", WireState::Paused),
            snap(2, "new.bin", WireState::Queued),
        ]),
    ]));
    let events = api.events_tx.clone();
    tokio::spawn(async move {
        sleep(Duration::from_millis(300)).await;
        // The daemon goes away.
        events.send(None).unwrap();
    });
    let (_tx, input) = input();
    let (app, frames) = run_for(&api, input, 3.0).await;
    assert_eq!(app.conn, Conn::Connected);
    assert_eq!(app.banner, None);
    assert_eq!(app.downloads.len(), 2);
    assert_eq!(api.connects(), 3);
    assert!(any_frame(&frames, "reconnecting"), "no reconnecting frame");
    assert!(any_frame(&frames, "connection refused"), "no error banner");
    assert!(last(&frames).contains("new.bin"));
}

#[tokio::test(start_paused = true)]
async fn reconnect_backoff_and_attempt_counter() {
    let api = Arc::new(
        FakeApi::new(Vec::new()).with_fallback(Err(ApiError::NoDaemon(
            "no daemon answered after starting one — see its log: /x/puppyd.log".into(),
        ))),
    );
    let (_tx, input) = input();
    // Attempts at 0, .25, .75, 1.75, 3.75, 7.75, 12.75, 17.75 s (backoff capped at 5 s).
    let (app, frames) = run_for(&api, input, 18.0).await;
    assert_eq!(api.connects(), 8);
    assert_eq!(app.conn, Conn::Reconnecting { attempt: 8 });
    assert!(matches!(app.banner, Some(Banner::NoDaemon(_))));
    let f = last(&frames);
    assert!(f.contains("see its log: /x/puppyd.log"), "{f}");
    assert!(f.contains("reconnecting (8)"), "{f}");
}

#[tokio::test(start_paused = true)]
async fn incompatible_daemon_banner_and_restart() {
    // Stopping the old daemon takes a moment.
    let api = FakeApi::new(Vec::new())
        .with_fallback(Err(ApiError::Incompatible("protocol 1, expected 2".into())))
        .with_exec(|_| Exec::Slow(Duration::from_millis(100), Ok(None)));
    *api.after_restart.lock().unwrap() = Some(ok(vec![snap(1, "a.bin", WireState::Paused)]));
    let api = Arc::new(api);
    let (tx, input) = input();
    send_after(&tx, Duration::from_secs(2), vec![key('U')]);
    let started = Instant::now();
    let (app, frames) = run_for(&api, input, 2.5).await;
    assert!(any_frame(&frames, Banner::INCOMPATIBLE), "no banner");
    assert!(
        any_frame(&frames, "restarting the daemon…"),
        "no restarting frame"
    );
    assert_eq!(api.actions(), vec![Cmd::RestartDaemon]);
    // Reconnected right after the restart, not at the next backoff step (3.75 s).
    assert_eq!(app.conn, Conn::Connected);
    assert_eq!(app.banner, None);
    assert_eq!(app.downloads.len(), 1);
    assert!(started.elapsed() <= Duration::from_millis(2600));
    assert!(!last(&frames).contains("different puppy version"));
}

#[tokio::test(start_paused = true)]
async fn u_does_nothing_without_the_banner() {
    let api = Arc::new(FakeApi::new(vec![ok(vec![snap(
        1,
        "a.bin",
        WireState::Paused,
    )])]));
    let (tx, input) = input();
    send_after(&tx, Duration::from_millis(100), vec![key('U')]);
    run_for(&api, input, 0.5).await;
    assert!(api.actions().is_empty());
}

#[tokio::test(start_paused = true)]
async fn quit_returns_promptly() {
    // The daemon never answers; q still quits at once.
    let api = Arc::new(FakeApi::new(Vec::new()));
    let (tx, input) = input();
    tx.send(key('q')).unwrap();
    let (mut term, _) = Recorder::terminal(120, 40);
    let started = Instant::now();
    let app = run_with(api.clone(), theme(), &mut term, input, None)
        .await
        .unwrap();
    assert!(app.should_quit);
    assert!(started.elapsed() < Duration::from_millis(50));

    // Ctrl+C with a command stuck in flight: waits at most the short grace period.
    let api = Arc::new(
        FakeApi::new(vec![ok(vec![snap(1, "a.bin", WireState::Active)])]).with_exec(|_| Exec::Hang),
    );
    let (tx, input) = self::input();
    send_after(&tx, Duration::from_millis(100), vec![key('p'), ctrl_c()]);
    let started = Instant::now();
    let app = run_with(api.clone(), theme(), &mut term, input, None)
        .await
        .unwrap();
    assert!(app.should_quit);
    assert_eq!(api.actions(), vec![Cmd::Pause(1)]);
    assert!(
        started.elapsed() <= Duration::from_millis(700),
        "{:?}",
        started.elapsed()
    );
}

#[tokio::test(start_paused = true)]
async fn input_end_stops_without_deadline() {
    let api = Arc::new(FakeApi::new(Vec::new()));
    let (mut term, _) = Recorder::terminal(80, 24);
    let app = run_with(api, theme(), &mut term, futures_util::stream::empty(), None)
        .await
        .unwrap();
    assert!(!app.should_quit);
}

#[tokio::test(start_paused = true)]
async fn restart_ignores_stale_failures_and_retries_fast() {
    // Connects take 1 s; a restart lands while one (doomed to fail) is in flight.
    let mut api = FakeApi::new(Vec::new())
        .with_fallback(Err(ApiError::Incompatible("old".into())))
        .with_exec(|_| Exec::Slow(Duration::from_millis(100), Ok(None)));
    api.connect_delay = Duration::from_secs(1);
    *api.after_restart.lock().unwrap() = Some(ok(vec![snap(1, "a.bin", WireState::Paused)]));
    let api = Arc::new(api);
    let (tx, input) = input();
    // Attempt 1: 0–1 s; attempt 2: 1.25–2.25 s; U at 2.1 s, restarted at 2.2 s.
    send_after(&tx, Duration::from_millis(2100), vec![key('U')]);
    let (app, frames) = run_for(&api, input, 4.0).await;
    assert_eq!(app.conn, Conn::Connected);
    {
        let frames = frames.lock().unwrap();
        let restarting = frames
            .iter()
            .position(|f| f.contains(Banner::RESTARTING))
            .expect("no restarting frame");
        assert!(
            frames[restarting..]
                .iter()
                .all(|f| !f.contains("different puppy version")),
            "a stale failure brought the banner back"
        );
    }

    // The old daemon takes a moment to go: two more mismatches, then the new one.
    let api = FakeApi::new(Vec::new())
        .with_fallback(Err(ApiError::Incompatible("old".into())))
        .with_exec(|_| Exec::Slow(Duration::from_millis(100), Ok(None)));
    *api.after_restart_script.lock().unwrap() = vec![
        Err(ApiError::Incompatible("old".into())),
        Err(ApiError::Incompatible("old".into())),
    ];
    *api.after_restart.lock().unwrap() = Some(ok(Vec::new()));
    let api = Arc::new(api);
    let (tx, input) = self::input();
    send_after(&tx, Duration::from_secs(1), vec![key('U')]);
    // Restart done at 1.1 s; retries at 1.1, 1.3, 1.5 s (normal backoff: 1.85 s).
    let (app, frames) = run_for(&api, input, 1.7).await;
    assert_eq!(app.conn, Conn::Connected);
    let frames = frames.lock().unwrap();
    let restarting = frames
        .iter()
        .position(|f| f.contains(Banner::RESTARTING))
        .expect("no restarting frame");
    assert!(
        frames[restarting..]
            .iter()
            .all(|f| !f.contains("different puppy version"))
    );
}

#[tokio::test(start_paused = true)]
async fn one_detail_fetch_in_flight_per_download() {
    let api = Arc::new(
        FakeApi::new(vec![ok(vec![snap(1, "a.bin", WireState::Active)])]).with_exec(|_| Exec::Hang),
    );
    let (_tx, input) = input();
    run_for(&api, input, 3.0).await;
    let fetches = api
        .executed
        .lock()
        .unwrap()
        .iter()
        .filter(|c| **c == Cmd::FetchDetail(1))
        .count();
    assert_eq!(fetches, 1);

    // Once answered, the next tick fetches again.
    let api = Arc::new(
        FakeApi::new(vec![ok(vec![snap(1, "a.bin", WireState::Active)])])
            .with_exec(|_| Exec::Slow(Duration::from_millis(700), Err("slow".into()))),
    );
    let (_tx, input) = self::input();
    // Ticks at .5 (fetch until 1.2), 1.0 (skipped), 1.5 (fetch), 2.0 (skipped), 2.5 (fetch).
    run_for(&api, input, 2.7).await;
    assert_eq!(api.executed.lock().unwrap().len(), 3);
}

#[tokio::test(start_paused = true)]
async fn background_task_panic_is_fatal() {
    let api = Arc::new(
        FakeApi::new(vec![ok(vec![snap(1, "a.bin", WireState::Active)])])
            .with_exec(|_| panic!("exec blew up")),
    );
    let (tx, input) = input();
    send_after(&tx, Duration::from_millis(100), vec![key('p')]);
    let (mut term, _) = Recorder::terminal(120, 40);
    let started = Instant::now();
    let e = run_with(
        api,
        theme(),
        &mut term,
        input,
        Some(Duration::from_secs(10)),
    )
    .await
    .unwrap_err();
    assert!(e.to_string().contains("panicked"), "{e}");
    assert!(started.elapsed() < Duration::from_millis(200));

    // The connection task too.
    let mut api = FakeApi::new(Vec::new());
    api.connect_panics = true;
    let (_tx, input) = self::input();
    let e = run_with(
        Arc::new(api),
        theme(),
        &mut term,
        input,
        Some(Duration::from_secs(10)),
    )
    .await
    .unwrap_err();
    assert!(e.to_string().contains("internal error"), "{e}");
}

#[tokio::test(start_paused = true)]
async fn wall_clock_set_before_the_first_frame() {
    let api = Arc::new(FakeApi::new(Vec::new()));
    let (_tx, input) = input();
    let (app, frames) = run_for(&api, input, 0.1).await;
    assert!(app.now_unix > 1_700_000_000);
    assert!(!frames.lock().unwrap().is_empty());
}

#[test]
fn banner_messages_are_one_line() {
    let mut app = App::new(theme());
    let now = std::time::Instant::now();
    app.update(
        Msg::ConnectError("\n  permission denied\nsecond line\x1b[31m".into()),
        now,
    );
    assert_eq!(app.banner, Some(Banner::Error("permission denied".into())));
    app.update(Msg::NoDaemon("see\tlog\nmore".into()), now);
    let Some(Banner::NoDaemon(t)) = &app.banner else {
        panic!()
    };
    assert!(!t.contains('\t') && !t.contains("more"), "{t:?}");
}

// ---------------------------------------------------------------------------------
// Real daemon

mod real {
    use super::*;
    use puppy_core::config::HttpConfig;
    use puppy_daemon::lifecycle::{DaemonOptions, run_daemon};
    use puppy_daemon::{Paths, SocketName};
    use puppy_proto::Request;
    use puppy_testserver::{ServerCfg, TestServer};
    use puppy_tui::api::ClientApi;
    use std::path::PathBuf;

    /// The socket inside `home` (removed with it); `home` is under `/tmp` so the path
    /// stays well within `sun_path` (~104 bytes on macOS).
    fn socket_name(home: &std::path::Path) -> SocketName {
        if cfg!(windows) {
            SocketName::Pipe(format!("puppy-tui-{}", std::process::id()))
        } else {
            let p = home.join("d.sock");
            assert!(p.as_os_str().len() < 100, "{}", p.display());
            SocketName::Path(p)
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn real_daemon_smoke() {
        let home = if cfg!(windows) {
            tempfile::tempdir().unwrap()
        } else {
            tempfile::Builder::new()
                .prefix("pz")
                .tempdir_in("/tmp")
                .unwrap()
        };
        let socket = socket_name(home.path());
        let paths = Paths::with_home(home.path(), socket.clone()).unwrap();
        let dl = home.path().join("dl");
        let daemon = tokio::spawn(run_daemon(DaemonOptions {
            idle_exit: None,
            paths: Some(paths.clone()),
            http: Some(HttpConfig {
                backoff_base_ms: 10,
                backoff_max_ms: 100,
                flush_interval_ms: 100,
                default_dir: dl.clone(),
                ..HttpConfig::default()
            }),
            engine: None,
            torrent_test: Some(Default::default()),
        }));
        let server = TestServer::start(ServerCfg {
            len: 256 * 1024,
            ..ServerCfg::default()
        })
        .await;
        let url = server.url("/file.bin").to_string();

        // `exe` is never spawned: the daemon is already listening (connect retries
        // until it does). A bogus path makes an accidental spawn fail loudly.
        let api: Arc<dyn DaemonApi> = Arc::new(ClientApi::new(
            paths.clone(),
            PathBuf::from("/nonexistent/puppy"),
        ));
        let (mut term, frames) = Recorder::terminal(120, 40);
        let (tx, input) = input();
        let watcher = {
            let frames = frames.clone();
            tokio::spawn(async move {
                // Wait for the connected dashboard, add the download, wait for ✓.
                let wait = |needle: &'static str| {
                    let frames = frames.clone();
                    async move {
                        while !last(&frames).contains(needle) {
                            sleep(Duration::from_millis(20)).await;
                        }
                    }
                };
                wait("● connected").await;
                tx.send(key('a')).unwrap();
                tx.send(Msg::Paste(url)).unwrap();
                tx.send(Msg::Key(KeyEvent::from(KeyCode::Enter))).unwrap();
                wait("file.bin").await;
                wait("✓").await;
                tx.send(ctrl_c()).unwrap();
            })
        };
        let app = run_with(
            api,
            theme(),
            &mut term,
            input,
            Some(Duration::from_secs(60)),
        )
        .await
        .unwrap();
        assert!(app.should_quit, "timed out; last frame:\n{}", last(&frames));
        watcher.await.unwrap();
        assert!(
            app.toasts.iter().all(|t| !t.text.starts_with("add:")),
            "{:?}",
            app.toasts
        );
        let file = dl.join("file.bin");
        assert_eq!(std::fs::metadata(&file).unwrap().len(), 256 * 1024);

        // Quitting left the daemon running.
        let c = puppy_client::Client::connect(&socket, "test")
            .await
            .unwrap();
        c.call(Request::Shutdown).await.unwrap();
        daemon.await.unwrap().unwrap();
    }
}

// ---------------------------------------------------------------------------------
// Terminal

#[tokio::test]
async fn run_refuses_without_a_terminal() {
    use std::io::IsTerminal;
    if std::io::stdout().is_terminal() && std::io::stdin().is_terminal() {
        return; // Run interactively: nothing to check here.
    }
    let api: Arc<dyn DaemonApi> = Arc::new(FakeApi::new(Vec::new()));
    let e = puppy_tui::runtime::run(api, theme()).await.unwrap_err();
    assert_eq!(e.to_string(), puppy_tui::runtime::NOT_A_TERMINAL);
}

#[test]
fn terminal_events_to_messages() {
    use crossterm::event::{
        Event as TermEvent, KeyEventKind, MouseButton, MouseEvent, MouseEventKind,
    };
    use puppy_tui::runtime::input_msg;
    let mouse = |kind| {
        TermEvent::Mouse(MouseEvent {
            kind,
            column: 1,
            row: 2,
            modifiers: KeyModifiers::NONE,
        })
    };
    let mut release = KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE);
    release.kind = KeyEventKind::Release;
    assert_eq!(input_msg(TermEvent::Key(release)), None);
    assert_eq!(
        input_msg(TermEvent::Key(KeyEvent::new(
            KeyCode::Char('q'),
            KeyModifiers::NONE
        ))),
        Some(key('q'))
    );
    assert_eq!(input_msg(mouse(MouseEventKind::Moved)), None);
    assert_eq!(
        input_msg(mouse(MouseEventKind::Up(MouseButton::Left))),
        None
    );
    assert!(input_msg(mouse(MouseEventKind::Down(MouseButton::Left))).is_some());
    assert!(input_msg(mouse(MouseEventKind::ScrollDown)).is_some());
    assert_eq!(
        input_msg(TermEvent::Paste("x".into())),
        Some(Msg::Paste("x".into()))
    );
    assert_eq!(
        input_msg(TermEvent::Resize(90, 30)),
        Some(Msg::Resize(90, 30))
    );
    assert_eq!(input_msg(TermEvent::FocusGained), None);
}

#[test]
fn banner_sits_under_the_title_at_every_size() {
    use puppy_tui::ui::draw;
    let banners = [
        Banner::Incompatible("x".into()),
        Banner::NoDaemon("no daemon answered — see its log: /x/puppyd.log".into()),
        Banner::Error("permission denied".into()),
        Banner::Restarting,
    ];
    for (w, h) in [(60, 16), (80, 24), (120, 40)] {
        for b in &banners {
            let mut app = App::new(theme());
            app.update(
                Msg::Connected {
                    version: "1".into(),
                    settings: settings(),
                    downloads: vec![snap(1, "a.bin", WireState::Active)],
                },
                std::time::Instant::now(),
            );
            app.banner = Some(b.clone());
            let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
            term.draw(|f| draw(&mut app, f)).unwrap();
            let t = text(term.backend().buffer());
            let rows: Vec<&str> = t.lines().collect();
            let want = match b {
                Banner::Incompatible(_) => "daemon is a different",
                Banner::NoDaemon(_) => "no daemon answered",
                Banner::Error(_) => "cannot connect to the daemon",
                Banner::Restarting => "restarting the daemon",
            };
            assert!(rows[1].contains(want), "{w}x{h}:\n{t}");
            assert!(rows[2].starts_with('╭'), "{w}x{h}:\n{t}");
            assert!(t.contains("a.bin"), "{w}x{h}:\n{t}");
        }
    }
    // The full incompatible text at a usual width.
    let mut app = App::new(theme());
    app.banner = Some(Banner::Incompatible("x".into()));
    let mut term = Terminal::new(TestBackend::new(80, 24)).unwrap();
    term.draw(|f| draw(&mut app, f)).unwrap();
    assert!(text(term.backend().buffer()).contains(Banner::INCOMPATIBLE));
}

// ---------------------------------------------------------------------------------
// Final fix wave

/// A fake whose settings change with `SetGlobalLimits`/`SetMaxActive` (unless
/// `fail_max`) and whose `FetchSettings` reports them.
fn settings_api(fail_max: bool) -> Arc<FakeApi> {
    let current = Arc::new(Mutex::new(settings()));
    Arc::new(
        FakeApi::new(vec![ok(vec![snap(1, "a.bin", WireState::Completed)])]).with_exec(move |c| {
            let mut s = current.lock().unwrap();
            match c {
                Cmd::SetGlobalLimits { down, up } => {
                    s.global_down_limit = *down;
                    s.global_up_limit = *up;
                    Exec::Reply(Ok(None))
                }
                Cmd::SetMaxActive(_) if fail_max => Exec::Reply(Err("the daemon said no".into())),
                Cmd::SetMaxActive(n) => {
                    s.max_active = *n;
                    Exec::Reply(Ok(None))
                }
                Cmd::FetchSettings => Exec::Reply(Ok(Some(Msg::Settings(s.clone())))),
                _ => Exec::Reply(Ok(None)),
            }
        }),
    )
}

#[tokio::test(start_paused = true)]
async fn global_limits_refresh_the_footer() {
    let api = settings_api(false);
    let (tx, input) = input();
    send_after(
        &tx,
        Duration::from_millis(200),
        vec![
            key('L'),
            key('2'),
            key('M'),
            Msg::Key(KeyEvent::from(KeyCode::Enter)),
        ],
    );
    let (app, frames) = run_for(&api, input, 1.0).await;
    assert!(
        last(&frames).contains("limit: ↓ 2.0 MiB/s"),
        "{}",
        last(&frames)
    );
    assert_eq!(
        app.settings.as_ref().unwrap().global_down_limit,
        Some(2 << 20)
    );
    let executed = api.executed.lock().unwrap().clone();
    let set = executed
        .iter()
        .position(|c| matches!(c, Cmd::SetGlobalLimits { .. }))
        .unwrap();
    assert!(
        executed[set..].contains(&Cmd::FetchSettings),
        "{executed:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn failed_max_active_reverts_via_refresh() {
    let api = settings_api(true);
    let (tx, input) = input();
    send_after(&tx, Duration::from_millis(200), vec![key('+')]);
    let (app, frames) = run_for(&api, input, 1.0).await;
    assert_eq!(app.settings.as_ref().unwrap().max_active, 3);
    let f = last(&frames);
    assert!(f.contains("max 3"), "{f}");
    assert!(f.contains("max active: the daemon said no"), "{f}");
}

#[tokio::test(start_paused = true)]
async fn picker_commands_report_back_to_the_picker() {
    let mut t = snap(1, "Show", WireState::ChoosingFiles);
    t.kind = EngineKindTag::Torrent;
    let files = vec![
        puppy_core::types::TorrentFile {
            index: 0,
            path: "Show/a.mkv".into(),
            size: 10,
        },
        puppy_core::types::TorrentFile {
            index: 1,
            path: "Show/b.mkv".into(),
            size: 20,
        },
    ];
    let api = Arc::new(FakeApi::new(vec![ok(vec![t])]).with_exec(move |c| match c {
        Cmd::FetchFiles(id) => Exec::Reply(Ok(Some(Msg::Files(*id, files.clone())))),
        Cmd::SelectFiles { .. } => Exec::Reply(Err(
            "refusing to overwrite: a file of this torrent already exists".into(),
        )),
        _ => Exec::Reply(Ok(None)),
    }));
    let (tx, input) = input();
    // The picker opens by itself; Enter on the first file sends the selection.
    send_after(
        &tx,
        Duration::from_millis(300),
        vec![Msg::Key(KeyEvent::from(KeyCode::Enter))],
    );
    let (app, frames) = run_for(&api, input, 1.0).await;
    let actions = api.actions();
    assert_eq!(actions[0], Cmd::FetchFiles(1));
    assert!(
        matches!(actions.get(1), Some(Cmd::SelectFiles { id: 1, .. })),
        "{actions:?}"
    );
    // The refusal shows in the picker, not as a toast.
    match &app.mode {
        puppy_tui::app::Mode::Popup(puppy_tui::app::Popup::Picker(p)) => {
            assert!(!p.submitting);
            assert!(
                p.error
                    .as_deref()
                    .is_some_and(|e| e.contains("already exists"))
            );
        }
        m => panic!("{m:?}"),
    }
    assert!(app.toasts.is_empty(), "{:?}", app.toasts);
    assert!(last(&frames).contains("refusing to overwrite"));
}
