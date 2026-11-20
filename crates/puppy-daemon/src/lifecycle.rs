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
use std::fs::File;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

/// How often the idle-exit condition is checked.
const IDLE_CHECK: Duration = Duration::from_millis(250);
/// How long a probe of an existing Unix socket may take before whatever listens there
/// is considered alive.
#[cfg(unix)]
const STALE_PROBE: Duration = Duration::from_secs(1);

#[derive(Debug, Default)]
pub struct DaemonOptions {
    /// Exit after being idle (no queued/starting/active downloads and no connected
    /// clients) for this long. `None`: never idle-exit.
    pub idle_exit: Option<Duration>,
    /// Use these paths instead of resolving them from the environment.
    pub paths: Option<Paths>,
    /// Use this HTTP engine configuration instead of the one derived from the config file.
    pub http: Option<HttpConfig>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunOutcome {
    /// Ran and shut down gracefully.
    Exited,
    /// Another daemon holds the lock; nothing was started.
    AlreadyRunning,
}

/// Resolves the paths (unless given), sets up logging to `log_dir/puppyd.log` (plus
/// stderr when it is a terminal), takes an exclusive non-blocking lock on `lock_file`
/// (held by another daemon ⇒ `AlreadyRunning`), writes its pid to `pid_file` (removed
/// on exit), removes a dead Unix socket file (failing
/// instead if something listens on it or it is not a socket), loads config and state, builds the engine, manager and server, and runs until a `Shutdown`
/// request, Ctrl-C, SIGTERM (Unix) or — with `idle_exit` — that long without work or
/// clients. Then it stops the server (removing the socket file), shuts the manager
/// down (pausing downloads, persisting state) and releases the lock.
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

    let cfg = Config::load(&paths.config_file())?;
    let http = opts.http.unwrap_or_else(|| cfg.http_config());
    let engine: Arc<dyn Engine> =
        Arc::new(HttpEngine::new(http).context("cannot create the HTTP engine")?);
    let store = Store::new(paths.state_file.clone());
    let manager = Manager::start(vec![engine], store, cfg)
        .await
        .context("refusing to start")?;
    let server = match Server::bind(&paths.socket, manager.clone()).await {
        Ok(s) => s,
        Err(e) => {
            manager.shutdown().await;
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

    let clients = server.client_count();
    let shutdown_requested = server.shutdown_requested();
    let stop = CancellationToken::new();
    let mut server_task = tokio::spawn(server.run(stop.clone()));
    let mut server_done = false;
    let reason = tokio::select! {
        _ = shutdown_requested.cancelled() => "shutdown requested",
        _ = ctrl_c() => "interrupted",
        _ = terminate() => "terminated",
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

    // Stop accepting first so no client connects to a daemon on its way out; the
    // server removes its socket file when it is dropped at the end of `run`.
    stop.cancel();
    if !server_done && let Err(e) = server_task.await {
        warn!("the server task failed: {e}");
    }
    manager.shutdown().await;
    remove_socket_file(&paths.socket);
    info!("daemon stopped");
    Ok(RunOutcome::Exited)
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

/// Makes sure the socket file is gone (the server normally removes it already).
#[cfg(unix)]
fn remove_socket_file(name: &SocketName) {
    if let SocketName::Path(p) = name
        && let Err(e) = std::fs::remove_file(p)
        && e.kind() != io::ErrorKind::NotFound
    {
        warn!(path = %p.display(), "cannot remove the socket file: {e}");
    }
}

#[cfg(not(unix))]
fn remove_socket_file(_name: &SocketName) {}

async fn ctrl_c() {
    if let Err(e) = tokio::signal::ctrl_c().await {
        warn!("cannot listen for Ctrl-C: {e}");
        std::future::pending::<()>().await;
    }
}

#[cfg(unix)]
async fn terminate() {
    use tokio::signal::unix::{SignalKind, signal};
    match signal(SignalKind::terminate()) {
        Ok(mut s) => {
            s.recv().await;
        }
        Err(e) => {
            warn!("cannot listen for SIGTERM: {e}");
            std::future::pending::<()>().await;
        }
    }
}

#[cfg(not(unix))]
async fn terminate() {
    std::future::pending::<()>().await;
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
