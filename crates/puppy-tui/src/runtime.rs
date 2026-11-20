//! The dashboard's runtime: the terminal (raw mode, alternate screen, mouse, bracketed
//! paste; restored on every exit path, panics included) and the event loop that feeds
//! terminal input, daemon events, command results and ticks into the pure [`App`],
//! runs the [`Cmd`]s it returns on spawned tasks and redraws at most every [`FRAME`].
//!
//! This is the only module that does terminal I/O.

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
use tokio::sync::{Notify, mpsc};
use tokio::task::{JoinHandle, JoinSet};
use tokio::time::{Instant, MissedTickBehavior, interval_at, sleep, sleep_until, timeout};

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
    // A failing terminal ends the input, which ends the dashboard.
    let input = EventStream::new()
        .take_while(|e| std::future::ready(e.is_ok()))
        .filter_map(|e| std::future::ready(e.ok().and_then(input_msg)));
    let result = run_with(api, theme, &mut terminal, Box::pin(input), None).await;
    drop(terminal);
    drop(guard);
    result.map(drop)
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
    mut input: impl Stream<Item = Msg> + Unpin,
    stop_after: Option<Duration>,
) -> anyhow::Result<App> {
    let term_err = |e: B::Error| anyhow::anyhow!("terminal: {e}");
    let start = Instant::now();
    let deadline = stop_after.map(|d| start + d);
    let mut app = App::new(theme);
    let size = terminal.size().map_err(term_err)?;
    app.update(Msg::Resize(size.width, size.height), start.into_std());

    let (daemon_tx, mut daemon_rx) = mpsc::channel(DAEMON_QUEUE);
    let (result_tx, mut result_rx) = mpsc::unbounded_channel();
    let reconnect_now = Arc::new(Notify::new());
    let connection = AbortOnDrop(tokio::spawn(connection(
        api.clone(),
        daemon_tx,
        reconnect_now.clone(),
    )));
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
            Some(m) = result_rx.recv() => Some(m),
            Some(_) = tasks.join_next(), if !tasks.is_empty() => None,
            _ = tick.tick() => {
                app.now_unix = unix_now();
                Some(Msg::Tick)
            }
            _ = sleep_until(draw_at), if dirty => {
                terminal.draw(|f| draw(&mut app, f)).map_err(term_err)?;
                last_draw = Some(Instant::now());
                dirty = false;
                None
            }
            _ = sleep_until(deadline.unwrap_or(never)), if deadline.is_some() => break,
        };
        let Some(msg) = msg else { continue };
        dirty = true;
        for cmd in app.update(msg, Instant::now().into_std()) {
            if cmd != Cmd::Quit {
                spawn_cmd(
                    &mut tasks,
                    api.clone(),
                    result_tx.clone(),
                    reconnect_now.clone(),
                    cmd,
                );
            }
        }
        if app.should_quit {
            break;
        }
    }
    drop(connection);
    if !app.should_quit && dirty {
        // Leave the final state on screen (tests read it).
        terminal.draw(|f| draw(&mut app, f)).map_err(term_err)?;
    }
    let _ = timeout(QUIT_GRACE, async {
        while tasks.join_next().await.is_some() {}
    })
    .await;
    Ok(app)
}

/// Runs `cmd` on its own task; its outcome comes back on `results`. Detail fetches
/// report only what they fetched (a failure just leaves the pane as it is).
fn spawn_cmd(
    tasks: &mut JoinSet<()>,
    api: Arc<dyn DaemonApi>,
    results: mpsc::UnboundedSender<Msg>,
    reconnect_now: Arc<Notify>,
    cmd: Cmd,
) {
    let label = label(&cmd);
    let detail = matches!(cmd, Cmd::FetchDetail(_));
    let restart = cmd == Cmd::RestartDaemon;
    tasks.spawn(async move {
        let r = api.exec(cmd).await;
        if restart {
            // Connect now: that starts this version's daemon.
            reconnect_now.notify_one();
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
        if let Some(msg) = msg {
            let _ = results.send(msg);
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
        Cmd::RestartDaemon => "restart daemon",
        Cmd::Quit => "quit",
    }
}

/// Keeps a connection: connects, forwards its events, and after it ends (or a failed
/// attempt) waits with backoff — or until `reconnect_now` — and connects again. Every
/// failed attempt and every lost connection sends one [`Msg::Disconnected`]. Returns
/// when the loop stops listening.
async fn connection(api: Arc<dyn DaemonApi>, tx: mpsc::Sender<Msg>, reconnect_now: Arc<Notify>) {
    let mut delay = BACKOFF_FIRST;
    loop {
        let msgs = match api.connect().await {
            Ok((version, settings, downloads)) => {
                delay = BACKOFF_FIRST;
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
            Err(e) => vec![
                Msg::Disconnected,
                match e {
                    ApiError::Incompatible(s) => Msg::Incompatible(s),
                    ApiError::NoDaemon(s) => Msg::NoDaemon(s),
                    ApiError::Other(s) => Msg::ConnectError(s),
                },
            ],
        };
        for msg in msgs {
            if tx.send(msg).await.is_err() {
                return;
            }
        }
        tokio::select! {
            _ = sleep(delay) => {}
            _ = reconnect_now.notified() => {}
        }
        delay = (delay * 2).min(BACKOFF_MAX);
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
