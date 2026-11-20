//! The daemon's lifecycle: logging, the single-instance lock, stale socket cleanup,
//! wiring config + store + engines + manager + server, and running until a shutdown
//! request, a signal or (optionally) an idle timeout, then shutting down gracefully.

use crate::config::Config;
use crate::manager::Manager;
use crate::paths::{Paths, SocketName};
use crate::server::Server;
use crate::store::Store;
use anyhow::Context;
use puppy_core::config::HttpConfig;
use puppy_core::engine::Engine;
use puppy_core::http::engine::HttpEngine;
use puppy_torrent::{TorrentEngine, TorrentEngineOptions};
use std::fs::File;
use std::io::{self, Write};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

/// After a second shutdown signal, how long the state may take to be saved before the
/// daemon exits anyway.
const FORCED_FLUSH: Duration = Duration::from_secs(1);
/// How long the torrent session may take to stop (it normally takes about 1 s). Taken
/// from the manager's pause phase, so shutdown stays within its overall bound.
const TORRENT_SHUTDOWN: Duration = Duration::from_millis(1500);
/// How often the idle-exit condition is checked.
const IDLE_CHECK: Duration = Duration::from_millis(250);
/// How long a probe of an existing Unix socket may take before whatever listens there
/// is considered alive.
#[cfg(unix)]
const STALE_PROBE: Duration = Duration::from_secs(1);

#[derive(Default)]
pub struct DaemonOptions {
    /// Exit after being idle (no queued/starting/active downloads and no connected
    /// clients) for this long. `None`: never idle-exit.
    pub idle_exit: Option<Duration>,
    /// Use these paths instead of resolving them from the environment.
    pub paths: Option<Paths>,
    /// Use this HTTP engine configuration instead of the one derived from the config file.
    pub http: Option<HttpConfig>,
    /// Use this engine instead of building an HTTP engine (tests).
    pub engine: Option<Arc<dyn Engine>>,
    /// Tests: run the torrent engine network-free (no DHT, trackers, LSD or UPnP;
    /// listening on 127.0.0.1) with these peers given to every torrent.
    #[doc(hidden)]
    pub torrent_test: Option<TorrentTestOptions>,
}

/// See [`DaemonOptions::torrent_test`].
#[doc(hidden)]
#[derive(Debug, Clone, Default)]
pub struct TorrentTestOptions {
    pub initial_peers: Vec<SocketAddr>,
    /// Filled with the daemon's torrent engine once it is built.
    pub probe: TorrentProbe,
}

/// A look at the daemon's torrent engine (tests).
#[doc(hidden)]
#[derive(Clone, Default)]
pub struct TorrentProbe(Arc<std::sync::OnceLock<Arc<TorrentEngine>>>);

impl TorrentProbe {
    /// Whether the engine's librqbit session has been started (`None`: no engine yet).
    pub fn session_started(&self) -> Option<bool> {
        self.0.get().map(|e| e.session_started())
    }
}

impl std::fmt::Debug for TorrentProbe {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("TorrentProbe")
            .field(&self.session_started())
            .finish()
    }
}

impl std::fmt::Debug for DaemonOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DaemonOptions")
            .field("idle_exit", &self.idle_exit)
            .field("paths", &self.paths)
            .field("http", &self.http)
            .field("engine", &self.engine.as_ref().map(|e| e.kind()))
            .field("torrent_test", &self.torrent_test)
            .finish()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunOutcome {
    /// Ran and shut down gracefully.
    Exited,
    /// Another daemon holds the lock; nothing was started.
    AlreadyRunning,
    /// A second signal cut the graceful shutdown short: the state was saved on a best
    /// effort basis, and downloads may resume from an earlier point.
    Forced,
}

/// Resolves the paths (unless given), sets up logging to `log_dir/puppyd.log` (plus
/// stderr when it is a terminal), takes an exclusive non-blocking lock on `lock_file`
/// (held by another daemon ⇒ `AlreadyRunning`), writes its pid to `pid_file` (removed
/// on exit), removes a dead Unix socket file (failing
/// instead if something listens on it or it is not a socket), loads config and state,
/// builds the engines (HTTP, and torrent unless its engine fails to start: then the
/// daemon runs without torrents), manager and server, and runs until a `Shutdown`
/// request, a signal (Ctrl-C/SIGINT, SIGTERM, SIGHUP on Unix; Ctrl-C, console close,
/// logoff and system shutdown on Windows) or — with `idle_exit` — that long without
/// work or clients. Then it stops the server (removing the socket file), shuts the
/// manager down (pausing downloads, then persisting state), stops the torrent session
/// (about 1 s) and releases the lock. Pausing plus stopping the session take at most
/// 10 s together: the session's 1.5 s are taken from the pause phase. Another signal during that shutdown makes it save the state for at most
/// 1 s and return [`RunOutcome::Forced`] at once.
///
/// Fails without touching it if the state file exists but cannot be read.
pub async fn run_daemon(opts: DaemonOptions) -> anyhow::Result<RunOutcome> {
    let paths = match opts.paths {
        Some(p) => p,
        None => Paths::resolve().context("cannot set up the daemon's directories")?,
    };
    init_logging(&paths.log_dir);

    // Held (open) until this function returns; closing it releases the lock.
    let Some(_lock) = acquire_lock(&paths.lock_file)? else {
        info!(lock = %paths.lock_file.display(), "another daemon is running");
        return Ok(RunOutcome::AlreadyRunning);
    };
    // Dropped (removing the file) before the lock is released.
    let _pid_file = PidFile::write(&paths.pid_file);
    // Only the lock holder may touch the socket.
    remove_stale_socket(&paths.socket).await?;

    let mut cfg = Config::load(&paths.config_file())?;
    let http = opts.http.unwrap_or_else(|| cfg.http_config());
    let download_dir = http.default_dir.clone();
    let engine: Arc<dyn Engine> = match opts.engine {
        Some(engine) => engine,
        None => Arc::new(HttpEngine::new(http).context("cannot create the HTTP engine")?),
    };
    let torrent = start_torrent_engine(&cfg, &paths, download_dir, opts.torrent_test).await;
    let mut engines = vec![engine];
    if let Some(t) = &torrent {
        engines.push(t.clone());
        // The session stops after the pause phase, within the same overall bound.
        cfg.shutdown_pause_timeout = cfg.shutdown_pause_timeout.saturating_sub(TORRENT_SHUTDOWN);
    }
    let store = Store::new(paths.state_file.clone());
    let manager = match Manager::start(engines, store, cfg).await {
        Ok(m) => m,
        Err(e) => {
            shutdown_torrent(torrent.as_deref()).await;
            return Err(e).context("refusing to start");
        }
    };
    let server = match Server::bind(&paths.socket, manager.clone()).await {
        Ok(s) => s,
        Err(e) => {
            manager.shutdown().await;
            shutdown_torrent(torrent.as_deref()).await;
            return Err(e)
                .with_context(|| format!("cannot listen on {}", socket_display(&paths.socket)));
        }
    };
    info!(
        pid = std::process::id(),
        socket = %socket_display(&paths.socket),
        idle_exit = ?opts.idle_exit,
        "daemon started"
    );

    let mut signals = Signals::new();
    let clients = server.client_count();
    let shutdown_requested = server.shutdown_requested();
    let stop = CancellationToken::new();
    let mut server_task = tokio::spawn(server.run(stop.clone()));
    let mut server_done = false;
    let reason = tokio::select! {
        _ = shutdown_requested.cancelled() => "shutdown requested",
        signal = signals.recv() => signal,
        _ = idle(opts.idle_exit, &manager, &clients) => "idle",
        r = &mut server_task => {
            server_done = true;
            if let Err(e) = r {
                warn!("the server task failed: {e}");
            }
            "server stopped"
        }
    };
    info!(reason, "shutting down");

    let graceful = async {
        // Stop accepting first so no client connects to a daemon on its way out; the
        // server removes its socket file when it is dropped at the end of `run`.
        stop.cancel();
        if !server_done && let Err(e) = (&mut server_task).await {
            warn!("the server task failed: {e}");
        }
        manager.shutdown().await;
        // After the manager paused its torrents: stops the session (listener, DHT).
        shutdown_torrent(torrent.as_deref()).await;
    };
    tokio::select! {
        _ = graceful => {}
        signal = signals.recv() => {
            warn!(signal, "second signal: exiting without waiting for downloads to pause");
            stop.cancel();
            server_task.abort();
            if !manager.save_state(FORCED_FLUSH).await {
                warn!("the state could not be saved in time");
            }
            return Ok(RunOutcome::Forced);
        }
    }
    info!("daemon stopped");
    Ok(RunOutcome::Exited)
}

/// Starts the torrent engine from the `[torrent]` config (network-free with
/// `test`). A failure (e.g. its state dir cannot be created) is logged and the daemon
/// runs without torrents instead of refusing to start.
async fn start_torrent_engine(
    cfg: &Config,
    paths: &Paths,
    download_dir: PathBuf,
    test: Option<TorrentTestOptions>,
) -> Option<Arc<TorrentEngine>> {
    let t = &cfg.torrent;
    let opts = TorrentEngineOptions {
        dht: t.dht,
        upnp: t.upnp,
        lsd: t.lsd,
        listen_port: t.listen_port,
        seed_ratio: t.seed_ratio,
        seed_time: t.seed_time,
        data_dir: paths.data_dir.clone(),
        default_dir: download_dir,
        test_mode: test.is_some(),
        test_initial_peers: test
            .as_ref()
            .map(|t| t.initial_peers.clone())
            .unwrap_or_default(),
    };
    match TorrentEngine::new(opts).await {
        Ok(engine) => {
            let engine = Arc::new(engine);
            if let Some(t) = test {
                let _ = t.probe.0.set(engine.clone());
            }
            Some(engine)
        }
        Err(e) => {
            tracing::error!("cannot start the torrent engine, running without torrents: {e}");
            None
        }
    }
}

/// Stops the torrent session, waiting at most [`TORRENT_SHUTDOWN`].
async fn shutdown_torrent(torrent: Option<&TorrentEngine>) {
    if let Some(t) = torrent
        && tokio::time::timeout(TORRENT_SHUTDOWN, t.shutdown())
            .await
            .is_err()
    {
        warn!("the torrent session did not stop in time");
    }
}

fn socket_display(s: &SocketName) -> String {
    match s {
        SocketName::Path(p) => p.display().to_string(),
        SocketName::Pipe(n) => format!(r"\\.\pipe\{n}"),
    }
}

/// The daemon's pid in `<data_dir>/puppyd.pid` while it holds the lock; removed on
/// drop. Failing to write it is logged, not fatal.
struct PidFile(Option<PathBuf>);

impl PidFile {
    fn write(path: &Path) -> Self {
        match std::fs::write(path, format!("{}\n", std::process::id())) {
            Ok(()) => PidFile(Some(path.to_path_buf())),
            Err(e) => {
                warn!(path = %path.display(), "cannot write the pid file: {e}");
                PidFile(None)
            }
        }
    }
}

impl Drop for PidFile {
    fn drop(&mut self) {
        if let Some(p) = &self.0
            && let Err(e) = std::fs::remove_file(p)
            && e.kind() != io::ErrorKind::NotFound
        {
            warn!(path = %p.display(), "cannot remove the pid file: {e}");
        }
    }
}

/// Opens (creating) the lock file and takes an exclusive lock without blocking.
/// `None` if another process (or another daemon in this process) holds it.
fn acquire_lock(path: &Path) -> anyhow::Result<Option<File>> {
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .with_context(|| format!("cannot open lock file {}", path.display()))?;
    match fs4::FileExt::try_lock(&file) {
        Ok(()) => Ok(Some(file)),
        Err(fs4::TryLockError::WouldBlock) => Ok(None),
        Err(fs4::TryLockError::Error(e)) => {
            Err(e).with_context(|| format!("cannot lock {}", path.display()))
        }
    }
}

/// Removes a dead socket left at the Unix socket path, e.g. by a daemon that was
/// killed. Refuses (without touching it) a socket something is still listening on
/// (another daemon, e.g. one with a different home, owns it) and anything that is not
/// a socket (a user's file or directory, a symlink).
#[cfg(unix)]
async fn remove_stale_socket(name: &SocketName) -> anyhow::Result<()> {
    use std::os::unix::fs::FileTypeExt;
    let SocketName::Path(p) = name else {
        return Ok(());
    };
    match std::fs::symlink_metadata(p) {
        Ok(m) if m.file_type().is_socket() => {}
        Ok(_) => anyhow::bail!(
            "{} exists and is not a socket; remove it or set PUPPY_SOCKET",
            p.display()
        ),
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e).with_context(|| format!("cannot inspect {}", p.display())),
    }
    match tokio::time::timeout(STALE_PROBE, tokio::net::UnixStream::connect(p)).await {
        // Answered, or alive but too busy to answer in time.
        Ok(Ok(_)) | Err(_) => anyhow::bail!(
            "another process is already listening on {}; stop it or set PUPPY_SOCKET",
            p.display()
        ),
        Ok(Err(e))
            if matches!(
                e.kind(),
                io::ErrorKind::ConnectionRefused | io::ErrorKind::NotFound
            ) => {}
        Ok(Err(e)) => {
            return Err(e).with_context(|| format!("cannot probe the socket {}", p.display()));
        }
    }
    match std::fs::remove_file(p) {
        Ok(()) => {
            info!(path = %p.display(), "removed a stale socket file");
            Ok(())
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).with_context(|| format!("cannot remove stale socket {}", p.display())),
    }
}

/// Named pipes vanish with their last handle; nothing can be stale.
#[cfg(not(unix))]
async fn remove_stale_socket(_name: &SocketName) -> anyhow::Result<()> {
    Ok(())
}

/// The signals that shut the daemon down, listened for from startup on (so a second
/// one during shutdown is caught too).
#[cfg(unix)]
struct Signals {
    int: Option<tokio::signal::unix::Signal>,
    term: Option<tokio::signal::unix::Signal>,
    hup: Option<tokio::signal::unix::Signal>,
}

#[cfg(unix)]
impl Signals {
    fn new() -> Self {
        use tokio::signal::unix::{SignalKind, signal};
        let listen = |kind: SignalKind, name: &str| match signal(kind) {
            Ok(s) => Some(s),
            Err(e) => {
                warn!("cannot listen for {name}: {e}");
                None
            }
        };
        Signals {
            int: listen(SignalKind::interrupt(), "SIGINT"),
            term: listen(SignalKind::terminate(), "SIGTERM"),
            hup: listen(SignalKind::hangup(), "SIGHUP"),
        }
    }

    /// The next signal's name.
    async fn recv(&mut self) -> &'static str {
        async fn next(s: &mut Option<tokio::signal::unix::Signal>) {
            let Some(s) = s else {
                return std::future::pending().await;
            };
            if s.recv().await.is_none() {
                std::future::pending::<()>().await;
            }
        }
        tokio::select! {
            _ = next(&mut self.int) => "SIGINT",
            _ = next(&mut self.term) => "SIGTERM",
            _ = next(&mut self.hup) => "SIGHUP",
        }
    }
}

#[cfg(windows)]
struct Signals {
    c: Option<tokio::signal::windows::CtrlC>,
    close: Option<tokio::signal::windows::CtrlClose>,
    logoff: Option<tokio::signal::windows::CtrlLogoff>,
    shutdown: Option<tokio::signal::windows::CtrlShutdown>,
}

#[cfg(windows)]
impl Signals {
    fn new() -> Self {
        use tokio::signal::windows;
        fn listen<T>(r: io::Result<T>, name: &str) -> Option<T> {
            r.map_err(|e| warn!("cannot listen for {name}: {e}")).ok()
        }
        Signals {
            c: listen(windows::ctrl_c(), "Ctrl-C"),
            close: listen(windows::ctrl_close(), "console close"),
            logoff: listen(windows::ctrl_logoff(), "logoff"),
            shutdown: listen(windows::ctrl_shutdown(), "system shutdown"),
        }
    }

    /// The next signal's name.
    async fn recv(&mut self) -> &'static str {
        macro_rules! next {
            ($s:expr) => {
                async move {
                    let Some(s) = $s.as_mut() else {
                        return std::future::pending().await;
                    };
                    if s.recv().await.is_none() {
                        std::future::pending::<()>().await;
                    }
                }
            };
        }
        // Destructured so that each future borrows only its own field.
        let Signals {
            c,
            close,
            logoff,
            shutdown,
        } = self;
        tokio::select! {
            _ = next!(c) => "Ctrl-C",
            _ = next!(close) => "console close",
            _ = next!(logoff) => "logoff",
            _ = next!(shutdown) => "system shutdown",
        }
    }
}

/// Resolves once the daemon has had no work and no clients for `after` (never if
/// `None`). Any work or client resets the timer.
async fn idle(after: Option<Duration>, manager: &Manager, clients: &AtomicUsize) {
    let Some(after) = after else {
        return std::future::pending().await;
    };
    let mut tick = tokio::time::interval(IDLE_CHECK);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut idle_since: Option<Instant> = None;
    loop {
        let now = tick.tick().await;
        if !manager.is_idle() || clients.load(Ordering::SeqCst) > 0 {
            idle_since = None;
            continue;
        }
        if now.duration_since(*idle_since.get_or_insert(now)) >= after {
            return;
        }
    }
}

/// The file log events go to. Swapped by each `init_logging`, so a process that runs
/// several daemons one after another (tests) logs each into its own `log_dir`.
static LOG_FILE: Mutex<Option<File>> = Mutex::new(None);

struct LogFile;

impl Write for LogFile {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match &mut *LOG_FILE.lock().unwrap_or_else(|e| e.into_inner()) {
            Some(f) => f.write(buf),
            None => Ok(buf.len()),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match &mut *LOG_FILE.lock().unwrap_or_else(|e| e.into_inner()) {
            Some(f) => f.flush(),
            None => Ok(()),
        }
    }
}

/// Logs to `log_dir/puppyd.log` (appending) and to stderr when it is a terminal,
/// filtered by `RUST_LOG` (default `info`). The global subscriber is installed once
/// per process; later calls only switch the log file.
fn init_logging(log_dir: &Path) {
    use std::io::IsTerminal;
    use tracing_subscriber::prelude::*;
    use tracing_subscriber::{EnvFilter, fmt};

    let path = log_dir.join("puppyd.log");
    match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        Ok(f) => *LOG_FILE.lock().unwrap_or_else(|e| e.into_inner()) = Some(f),
        Err(e) => eprintln!("puppyd: cannot open log file {}: {e}", path.display()),
    }

    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let file_layer = fmt::layer().with_ansi(false).with_writer(|| LogFile);
    let stderr_layer = io::stderr()
        .is_terminal()
        .then(|| fmt::layer().with_writer(io::stderr));
    // Fails if a subscriber is already set (an earlier daemon in this process, or the
    // embedding program's own): keep that one.
    let _ = tracing_subscriber::registry()
        .with(filter)
        .with(file_layer)
        .with(stderr_layer)
        .try_init();
}
