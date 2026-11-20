//! The dashboard's runtime: the terminal (raw mode, alternate screen, mouse, bracketed
//! paste; restored on every exit path, panics included) and the event loop that feeds
//! terminal input, daemon events, command results and ticks into the pure [`App`],
//! runs the [`Cmd`]s it returns on spawned tasks and redraws at most every [`FRAME`].
//!
//! This is the only module that does terminal I/O.

use std::collections::HashSet;
use std::io::{IsTerminal, Write};
use std::panic::{self, PanicHookInfo};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, bail};
use crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture, Event,
    EventStream, KeyEventKind, MouseEventKind,
};
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use crossterm::{cursor, execute};
use futures_util::{Stream, StreamExt};
use ratatui::Terminal;
use ratatui::backend::{Backend, CrosstermBackend};
use tokio::sync::{mpsc, watch};
use tokio::task::{JoinHandle, JoinSet};
use tokio::time::{Instant, MissedTickBehavior, interval_at, sleep, sleep_until, timeout};

use puppy_core::types::DownloadId;

use crate::api::{ApiError, DaemonApi};
use crate::app::{App, Cmd, CmdOutcome, Msg};
use crate::theme::Theme;
use crate::ui::draw;

/// How often [`Msg::Tick`] is sent.
pub const TICK: Duration = Duration::from_millis(500);
/// At most one frame per this period (~30 fps); bursts of messages are coalesced.
pub const FRAME: Duration = Duration::from_millis(33);
/// Reconnect backoff: the first delay, doubling up to the last.
pub const BACKOFF_FIRST: Duration = Duration::from_millis(250);
pub const BACKOFF_MAX: Duration = Duration::from_secs(5);
/// After a daemon restart: this many retries this far apart before the backoff.
pub const FAST_RETRIES: u32 = 5;
pub const FAST_RETRY: Duration = Duration::from_millis(200);
/// On quit, commands still running (e.g. an add just submitted) get this long to finish.
const QUIT_GRACE: Duration = Duration::from_millis(500);
/// Daemon messages waiting for the loop.
const DAEMON_QUEUE: usize = 256;

pub const NOT_A_TERMINAL: &str = "puppy's dashboard needs an interactive terminal";

/// Runs the dashboard in the real terminal until the user quits. The daemon is left
/// running.
pub async fn run(api: Arc<dyn DaemonApi>, theme: Theme) -> anyhow::Result<()> {
    if !std::io::stdout().is_terminal() || !std::io::stdin().is_terminal() {
        bail!(NOT_A_TERMINAL);
    }
    let guard = TerminalGuard::enter().context("cannot set up the terminal")?;
    let mut terminal = Terminal::new(CrosstermBackend::new(std::io::stdout()))?;
    terminal.clear()?;
    // A failing terminal ends the input (which ends the loop) and is the error.
    let read_error: Arc<std::sync::Mutex<Option<std::io::Error>>> = Arc::default();
    let slot = read_error.clone();
    let input = EventStream::new()
        .take_while(move |e| {
            let ok = e.is_ok();
            if let Err(e) = e {
                let e = std::io::Error::new(e.kind(), e.to_string());
                *slot.lock().unwrap_or_else(|p| p.into_inner()) = Some(e);
            }
            std::future::ready(ok)
        })
        .filter_map(|e| std::future::ready(e.ok().and_then(input_msg)));
    // A termination signal quits like `q` (the terminal is restored; the daemon keeps
    // running). The input's end still ends the loop.
    let input = futures_util::stream::select(
        input
            .map(Some)
            .chain(futures_util::stream::once(std::future::ready(None))),
        termination_signals().map(Some),
    )
    .take_while(|m| std::future::ready(m.is_some()))
    .filter_map(std::future::ready);
    let result = run_loop(
        api,
        theme,
        &mut terminal,
        Box::pin(input),
        None,
        Some(&TERMINAL_ACTIVE),
    )
    .await;
    drop(terminal);
    drop(guard);
    let read_error = read_error.lock().unwrap_or_else(|p| p.into_inner()).take();
    match (result, read_error) {
        (Err(e), _) => Err(e),
        (Ok(_), Some(e)) => Err(anyhow::Error::new(e).context("cannot read from the terminal")),
        (Ok(_), None) => Ok(()),
    }
}

/// The message for a terminal event; `None` for what the app ignores (key releases,
/// mouse motion and button releases), so it causes no redraw.
pub fn input_msg(e: Event) -> Option<Msg> {
    match e {
        Event::Key(k) if k.kind != KeyEventKind::Release => Some(Msg::Key(k)),
        Event::Mouse(m)
            if matches!(
                m.kind,
                MouseEventKind::Down(_) | MouseEventKind::ScrollUp | MouseEventKind::ScrollDown
            ) =>
        {
            Some(Msg::Mouse(m))
        }
        Event::Paste(s) => Some(Msg::Paste(s)),
        Event::Resize(w, h) => Some(Msg::Resize(w, h)),
        _ => None,
    }
}

/// The event loop on any backend, with `input` as the terminal's messages. Returns the
/// final [`App`] when the user quits, when `input` ends (without `stop_after`) or after
/// `stop_after`.
pub async fn run_with<B: Backend>(
    api: Arc<dyn DaemonApi>,
    theme: Theme,
    terminal: &mut Terminal<B>,
    input: impl Stream<Item = Msg> + Unpin,
    stop_after: Option<Duration>,
) -> anyhow::Result<App> {
    run_loop(api, theme, terminal, input, stop_after, None).await
}

/// [`Msg::Quit`] for each SIGTERM or SIGHUP (Unix; nothing elsewhere, where closing
/// the console ends the process).
pub fn termination_signals() -> impl Stream<Item = Msg> + Send + Unpin + 'static {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let signals: Vec<_> = [SignalKind::terminate(), SignalKind::hangup()]
            .into_iter()
            .filter_map(|kind| signal(kind).ok())
            .map(|mut s| {
                Box::pin(futures_util::stream::poll_fn(move |cx| {
                    s.poll_recv(cx).map(|r| r.map(|()| Msg::Quit))
                }))
            })
            .collect();
        futures_util::stream::select_all(signals)
    }
    #[cfg(not(unix))]
    {
        futures_util::stream::pending()
    }
}

/// The loop; `live`, when given, says whether the terminal is still in dashboard
/// mode (after the panic hook restored it, drawing would scribble over the shell).
async fn run_loop<B: Backend>(
    api: Arc<dyn DaemonApi>,
    theme: Theme,
    terminal: &mut Terminal<B>,
    mut input: impl Stream<Item = Msg> + Unpin,
    stop_after: Option<Duration>,
    live: Option<&AtomicBool>,
) -> anyhow::Result<App> {
    let can_draw = || live.is_none_or(|l| l.load(Ordering::SeqCst));
    let term_err = |e: B::Error| anyhow::anyhow!("terminal: {e}");
    let start = Instant::now();
    let deadline = stop_after.map(|d| start + d);
    let mut app = App::new(theme);
    app.now_unix = unix_now();
    let size = terminal.size().map_err(term_err)?;
    app.update(Msg::Resize(size.width, size.height), start.into_std());

    let (daemon_tx, mut daemon_rx) = mpsc::channel(DAEMON_QUEUE);
    let (result_tx, mut result_rx) = mpsc::unbounded_channel::<Done>();
    let (restarts, restarts_rx) = watch::channel(0u64);
    let restarts = Arc::new(restarts);
    let mut connection = AbortOnDrop(tokio::spawn(connection(
        api.clone(),
        daemon_tx,
        restarts_rx,
    )));
    // Detail fetches in flight: one per download at a time.
    let mut fetching: HashSet<DownloadId> = HashSet::new();
    let mut tasks = JoinSet::new();
    let mut tick = interval_at(start + TICK, TICK);
    tick.set_missed_tick_behavior(MissedTickBehavior::Delay);

    let mut input_open = true;
    let mut dirty = true;
    let mut last_draw: Option<Instant> = None;
    // Far enough to never fire.
    let never = start + Duration::from_secs(86_400 * 365);
    loop {
        let draw_at = last_draw.map_or(start, |t| t + FRAME);
        let msg = tokio::select! {
            m = input.next(), if input_open => match m {
                Some(m) => Some(m),
                None if deadline.is_none() => break,
                None => {
                    input_open = false;
                    None
                }
            },
            Some(m) = daemon_rx.recv() => Some(m),
            Some(done) = result_rx.recv() => {
                if let Some(id) = done.detail {
                    fetching.remove(&id);
                }
                done.msg
            }
            Some(r) = tasks.join_next(), if !tasks.is_empty() => match r {
                Err(e) if e.is_panic() => bail!("internal error: a background task panicked"),
                _ => None,
            },
            _ = &mut connection.0 => bail!("internal error: the daemon connection task ended"),
            _ = tick.tick() => {
                app.now_unix = unix_now();
                Some(Msg::Tick)
            }
            _ = sleep_until(draw_at), if dirty => {
                if can_draw() {
                    terminal.draw(|f| draw(&mut app, f)).map_err(term_err)?;
                }
                last_draw = Some(Instant::now());
                dirty = false;
                None
            }
            _ = sleep_until(deadline.unwrap_or(never)), if deadline.is_some() => break,
        };
        let Some(msg) = msg else { continue };
        dirty = true;
        for cmd in app.update(msg, Instant::now().into_std()) {
            if let Cmd::FetchDetail(id) = cmd
                && !fetching.insert(id)
            {
                continue;
            }
            if cmd != Cmd::Quit {
                spawn_cmd(
                    &mut tasks,
                    api.clone(),
                    result_tx.clone(),
                    restarts.clone(),
                    cmd,
                );
            }
        }
        if app.should_quit {
            break;
        }
    }
    drop(connection);
    if !app.should_quit && dirty && can_draw() {
        // Leave the final state on screen (tests read it).
        terminal.draw(|f| draw(&mut app, f)).map_err(term_err)?;
    }
    let _ = timeout(QUIT_GRACE, async {
        while tasks.join_next().await.is_some() {}
    })
    .await;
    Ok(app)
}

/// A finished command: the detail fetch it was (if one) and what to tell the app.
struct Done {
    detail: Option<DownloadId>,
    msg: Option<Msg>,
}

/// Runs `cmd` on its own task; its outcome comes back on `results`. Detail fetches
/// report only what they fetched (a failure just leaves the pane as it is).
fn spawn_cmd(
    tasks: &mut JoinSet<()>,
    api: Arc<dyn DaemonApi>,
    results: mpsc::UnboundedSender<Done>,
    restarts: Arc<watch::Sender<u64>>,
    cmd: Cmd,
) {
    let label = label(&cmd);
    let detail_id = match cmd {
        Cmd::FetchDetail(id) => Some(id),
        _ => None,
    };
    let detail = detail_id.is_some();
    let restart = cmd == Cmd::RestartDaemon;
    // The daemon may adjust or refuse new settings: the footer and the `L` popup show
    // what it really has (this also undoes the app's optimistic update on failure).
    let refresh = matches!(cmd, Cmd::SetGlobalLimits { .. } | Cmd::SetMaxActive(_));
    tasks.spawn(async move {
        let r = api.exec(cmd).await;
        if restart {
            // Connect now: that starts this version's daemon.
            restarts.send_modify(|n| *n += 1);
        }
        let msg = match r {
            Ok(Some(msg)) => Some(msg),
            Ok(None) | Err(_) if detail => None,
            Ok(None) => Some(Msg::CmdResult(CmdOutcome {
                label: label.into(),
                result: Ok(()),
            })),
            Err(e) => Some(Msg::CmdResult(CmdOutcome {
                label: label.into(),
                result: Err(e),
            })),
        };
        let _ = results.send(Done {
            detail: detail_id,
            msg,
        });
        if refresh && let Ok(Some(msg @ Msg::Settings(_))) = api.exec(Cmd::FetchSettings).await {
            let _ = results.send(Done {
                detail: None,
                msg: Some(msg),
            });
        }
    });
}

/// What a command attempted, for its error toast (`pause: …`).
fn label(cmd: &Cmd) -> &'static str {
    match cmd {
        Cmd::Add { .. } => "add",
        Cmd::Pause(_) => "pause",
        Cmd::Resume(_) => "resume",
        Cmd::Remove { .. } => "delete",
        Cmd::Restart(_) => "restart",
        Cmd::Move { .. } => "move",
        Cmd::SetLimit { .. } => "limit",
        Cmd::SetGlobalLimits { .. } => "global limits",
        Cmd::SetMaxActive(_) => "max active",
        Cmd::FetchDetail(_) => "details",
        Cmd::FetchSettings => "settings",
        Cmd::RestartDaemon => "restart daemon",
        Cmd::Quit => "quit",
    }
}

/// Keeps a connection: connects, forwards its events, and after it ends (or a failed
/// attempt) waits with backoff and connects again. Every failed attempt and every lost
/// connection sends one [`Msg::Disconnected`].
///
/// A daemon restart (`restarts` changes) reconnects at once with fresh backoff and a few
/// fast retries (the old daemon may still be on its way out); their failures, and those
/// of attempts begun before the restart, leave the "restarting" banner alone — only the
/// last fast retry reports. Returns when the loop stops listening.
async fn connection(
    api: Arc<dyn DaemonApi>,
    tx: mpsc::Sender<Msg>,
    mut restarts: watch::Receiver<u64>,
) {
    let mut delay = BACKOFF_FIRST;
    let mut fast = 0u32;
    loop {
        let generation = *restarts.borrow_and_update();
        let result = api.connect().await;
        let stale = *restarts.borrow() != generation;
        let msgs = match result {
            Ok((version, settings, downloads)) => {
                delay = BACKOFF_FIRST;
                fast = 0;
                let connected = Msg::Connected {
                    version,
                    settings,
                    downloads,
                };
                if tx.send(connected).await.is_err() {
                    return;
                }
                while let Some(event) = api.next_event().await {
                    if tx.send(Msg::Daemon(event)).await.is_err() {
                        return;
                    }
                }
                vec![Msg::Disconnected]
            }
            // Begun before the restart: says nothing about the new daemon.
            Err(_) if stale => Vec::new(),
            Err(e) => {
                let mut msgs = vec![Msg::Disconnected];
                if fast <= 1 {
                    msgs.push(match e {
                        ApiError::Incompatible(s) => Msg::Incompatible(s),
                        ApiError::NoDaemon(s) => Msg::NoDaemon(s),
                        ApiError::Other(s) => Msg::ConnectError(s),
                    });
                }
                msgs
            }
        };
        for msg in msgs {
            if tx.send(msg).await.is_err() {
                return;
            }
        }
        let pause = if fast > 0 {
            fast -= 1;
            FAST_RETRY
        } else {
            let d = delay;
            delay = (delay * 2).min(BACKOFF_MAX);
            d
        };
        let restarted = stale
            || tokio::select! {
                _ = sleep(pause) => false,
                r = restarts.changed() => match r {
                    Ok(()) => true,
                    // The runtime is gone.
                    Err(_) => return,
                },
            };
        if restarted {
            delay = BACKOFF_FIRST;
            fast = FAST_RETRIES;
        }
    }
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// Aborts the task when dropped (every exit path of the loop).
struct AbortOnDrop(JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Whether the terminal is in dashboard mode (restoring it is then due).
static TERMINAL_ACTIVE: AtomicBool = AtomicBool::new(false);

type PanicHook = Box<dyn Fn(&PanicHookInfo<'_>) + Sync + Send + 'static>;

/// The terminal in dashboard mode while alive: raw mode, alternate screen, mouse
/// capture, bracketed paste, hidden cursor. Dropping it (any exit path, unwinding
/// included) restores the terminal; a panic hook restores it before the panic message
/// is printed, so the message is readable.
struct TerminalGuard {
    /// The hook in place before ours, put back on drop.
    previous_hook: Option<Arc<PanicHook>>,
}

impl TerminalGuard {
    fn enter() -> std::io::Result<Self> {
        let previous: Arc<PanicHook> = Arc::new(panic::take_hook());
        let chained = previous.clone();
        panic::set_hook(Box::new(move |info| {
            restore_terminal();
            chained(info);
        }));
        TERMINAL_ACTIVE.store(true, Ordering::SeqCst);
        // Built first, so a failure below still restores what was set up.
        let guard = TerminalGuard {
            previous_hook: Some(previous),
        };
        enable_raw_mode()?;
        let mut out = std::io::stdout();
        execute!(out, EnterAlternateScreen, EnableMouseCapture, cursor::Hide)?;
        // Not available on legacy Windows consoles: pasting then arrives as keys.
        let _ = execute!(out, EnableBracketedPaste);
        Ok(guard)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        restore_terminal();
        // Replacing the hook is not allowed while panicking; ours is harmless then
        // (restoring twice is a no-op).
        if !std::thread::panicking()
            && let Some(previous) = self.previous_hook.take()
        {
            drop(panic::take_hook());
            panic::set_hook(Box::new(move |info| previous(info)));
        }
    }
}

/// Leaves dashboard mode; idempotent. Each step is attempted even if one fails.
fn restore_terminal() {
    if !TERMINAL_ACTIVE.swap(false, Ordering::SeqCst) {
        return;
    }
    let mut out = std::io::stdout();
    let _ = execute!(out, DisableBracketedPaste);
    let _ = execute!(out, DisableMouseCapture);
    let _ = execute!(out, LeaveAlternateScreen);
    let _ = execute!(out, cursor::Show);
    let _ = disable_raw_mode();
    let _ = out.flush();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::Connected;
    use crate::theme::ThemeChoice;
    use async_trait::async_trait;
    use puppy_proto::Event as DaemonEvent;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Buffer;

    /// A daemon that never answers.
    struct Silent;

    #[async_trait]
    impl DaemonApi for Silent {
        async fn connect(&self) -> Result<Connected, ApiError> {
            std::future::pending().await
        }
        async fn next_event(&self) -> Option<DaemonEvent> {
            std::future::pending().await
        }
        async fn exec(&self, _: Cmd) -> Result<Option<Msg>, crate::app::CmdError> {
            Ok(None)
        }
    }

    #[tokio::test(start_paused = true)]
    async fn no_drawing_once_the_terminal_was_restored() {
        let theme = Theme::detect(ThemeChoice::Dark, &|_| None);
        let blank = Buffer::empty(ratatui::layout::Rect::new(0, 0, 80, 24));
        // The panic hook already restored the terminal: nothing is drawn.
        let restored = AtomicBool::new(false);
        let mut term = Terminal::new(TestBackend::new(80, 24)).unwrap();
        let input = futures_util::stream::pending();
        let stop = Some(Duration::from_secs(1));
        run_loop(
            Arc::new(Silent),
            theme,
            &mut term,
            input,
            stop,
            Some(&restored),
        )
        .await
        .unwrap();
        assert_eq!(term.backend().buffer(), &blank);
        // While it is active, frames are drawn.
        let active = AtomicBool::new(true);
        let mut term = Terminal::new(TestBackend::new(80, 24)).unwrap();
        let input = futures_util::stream::pending();
        run_loop(
            Arc::new(Silent),
            theme,
            &mut term,
            input,
            stop,
            Some(&active),
        )
        .await
        .unwrap();
        assert_ne!(term.backend().buffer(), &blank);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn sigterm_and_sighup_quit() {
        for sig in ["-TERM", "-HUP"] {
            let mut signals = termination_signals();
            // Registered before the signal is sent (the handler replaces the default
            // action, which would end this test process).
            let status = std::process::Command::new("kill")
                .args([sig, &std::process::id().to_string()])
                .status()
                .unwrap();
            assert!(status.success());
            let msg = timeout(Duration::from_secs(5), signals.next()).await;
            assert_eq!(msg, Ok(Some(Msg::Quit)), "{sig}");
        }
    }
}
