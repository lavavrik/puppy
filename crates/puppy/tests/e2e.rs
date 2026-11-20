//! End-to-end: the real `puppy` binary as daemon and client.
//!
//! Each test has its own `PUPPY_HOME` tempdir and a short socket name, passed to every
//! process it starts (never set in this process's environment), with
//! `PUPPY_IDLE_EXIT=1` so auto-spawned daemons go away quickly. [`Env`]'s drop makes
//! sure no daemon outlives its test: it asks the daemon to shut down and, failing
//! that, kills the process named in `<data_dir>/puppyd.pid`.

use puppy_client::{Client, ClientError};
use puppy_core::types::{AddRequest, DownloadId, RequestContext, Source};
use puppy_daemon::{Paths, SocketName};
use puppy_proto::{DownloadSnapshot, ReplyData, Request, WireState};
use puppy_testserver::{ServerCfg, TestServer, body_byte};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_puppy");
const TIMEOUT: Duration = Duration::from_secs(30);
const KIB: u64 = 1024;

/// A short, unique socket name (macOS `sun_path` is ~104 bytes).
fn socket_name() -> SocketName {
    static N: AtomicUsize = AtomicUsize::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    if cfg!(windows) {
        SocketName::Pipe(format!("puppy-e2e-{pid}-{n}"))
    } else {
        SocketName::Path(PathBuf::from(format!("/tmp/pz-{pid}-e{n}.sock")))
    }
}

struct Env {
    home: tempfile::TempDir,
    socket: SocketName,
}

impl Env {
    fn new() -> Self {
        Env {
            home: tempfile::tempdir().unwrap(),
            socket: socket_name(),
        }
    }

    fn paths(&self) -> Paths {
        Paths::with_home(self.home.path(), self.socket.clone()).unwrap()
    }

    fn dl_dir(&self) -> PathBuf {
        self.home.path().join("dl")
    }

    /// The environment every `puppy` process of this test gets.
    fn vars(&self) -> Vec<(&'static str, String)> {
        let socket = match &self.socket {
            SocketName::Path(p) => p.to_str().unwrap().to_string(),
            SocketName::Pipe(n) => n.clone(),
        };
        vec![
            ("PUPPY_HOME", self.home.path().to_str().unwrap().to_string()),
            ("PUPPY_SOCKET", socket),
            ("PUPPY_IDLE_EXIT", "1".to_string()),
        ]
    }

    /// `Client::connect_or_spawn` with the real binary.
    async fn spawn_client(&self) -> Result<Client, ClientError> {
        let vars = self.vars();
        let vars: Vec<(&str, &str)> = vars.iter().map(|(k, v)| (*k, v.as_str())).collect();
        tokio::time::timeout(
            TIMEOUT,
            Client::connect_or_spawn(&self.paths(), "e2e", Path::new(BIN), &vars),
        )
        .await
        .expect("connect_or_spawn hung")
    }

    /// `puppy <args>` with this test's environment.
    fn command(&self, args: &[&str]) -> Command {
        let mut cmd = Command::new(BIN);
        cmd.args(args).stdin(Stdio::null());
        for (k, v) in self.vars() {
            cmd.env(k, v);
        }
        cmd
    }

    /// Runs `puppy <args>` to completion: (status, stdout, stderr). Output goes
    /// through files, not pipes: a daemon the command starts may inherit them, and
    /// must not keep this waiting.
    fn run(&self, args: &[&str]) -> (ExitStatus, String, String) {
        static N: AtomicUsize = AtomicUsize::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let out = self.home.path().join(format!("out-{n}"));
        let err = self.home.path().join(format!("err-{n}"));
        let child = self
            .command(args)
            .stdout(std::fs::File::create(&out).unwrap())
            .stderr(std::fs::File::create(&err).unwrap())
            .spawn()
            .unwrap();
        let status = wait_exit(child, TIMEOUT);
        (
            status,
            std::fs::read_to_string(out).unwrap(),
            std::fs::read_to_string(err).unwrap(),
        )
    }

    fn daemon_pid(&self) -> Option<u32> {
        let text = std::fs::read_to_string(self.paths().pid_file).ok()?;
        text.trim().parse().ok()
    }

    /// Whether some daemon holds this home's lock.
    fn lock_held(&self) -> bool {
        self.try_lock_held()
            .unwrap_or_else(|e| panic!("cannot test the lock: {e}"))
    }

    fn try_lock_held(&self) -> std::io::Result<bool> {
        let f = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(self.paths().lock_file)?;
        match fs4::FileExt::try_lock(&f) {
            Ok(()) => Ok(false),
            Err(fs4::TryLockError::WouldBlock) => Ok(true),
            Err(fs4::TryLockError::Error(e)) => Err(e),
        }
    }

    /// The pid of a live daemon of this home: the pid file is only trusted while the
    /// lock is held (a crashed daemon leaves a stale file whose pid may since belong
    /// to an unrelated process).
    fn live_daemon_pid(&self) -> Option<u32> {
        let pid = self.daemon_pid()?;
        self.try_lock_held().ok()?.then_some(pid)
    }

    /// Waits until the daemon is gone: lock free, pid file and socket file removed.
    async fn wait_daemon_gone(&self, within: Duration) {
        let deadline = Instant::now() + within;
        while self.lock_held() || self.paths().pid_file.exists() || self.socket_file_exists() {
            assert!(Instant::now() < deadline, "the daemon did not exit");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    fn socket_file_exists(&self) -> bool {
        match &self.socket {
            SocketName::Path(p) => p.exists(),
            SocketName::Pipe(_) => false,
        }
    }

    fn paused_req(&self, name: &str) -> AddRequest {
        AddRequest {
            source: Source::Http {
                url: "http://127.0.0.1:9/never".parse().unwrap(),
            },
            output_dir: Some(self.dl_dir()),
            filename: Some(name.into()),
            context: RequestContext::default(),
        }
    }

    fn req(&self, s: &TestServer, name: &str) -> AddRequest {
        AddRequest {
            source: Source::Http {
                url: s.url("/file.bin"),
            },
            output_dir: Some(self.dl_dir()),
            filename: Some(name.into()),
            context: RequestContext::default(),
        }
    }

    fn assert_file(&self, name: &str, len: u64) {
        let data = std::fs::read(self.dl_dir().join(name)).unwrap();
        assert_eq!(data.len() as u64, len);
        for (i, b) in data.iter().enumerate() {
            assert_eq!(*b, body_byte(i as u64, 0), "byte {i} of {name}");
        }
    }
}

impl Drop for Env {
    /// Never leaks a daemon: asks it to shut down, then kills it if it still holds
    /// the lock. A stale pid file (no lock holder) is never acted on.
    ///
    /// Known windows (time-of-check to time-of-use), acceptable for a test guard:
    /// - Between `live_daemon_pid` seeing the lock held and `kill`, the daemon may exit
    ///   and its pid be reused by an unrelated process, which would then be killed. The
    ///   daemon only gets here after ignoring a shutdown for 10 s, so it is hung, not
    ///   exiting.
    /// - The pid file is written after the lock is taken and removed before it is
    ///   released, so a lock holder without a pid file (just starting or stopping) is
    ///   not killed; one that is hung in that window leaks.
    /// - A daemon another test process started for the same home could be targeted;
    ///   homes are per-test tempdirs, so none exists.
    fn drop(&mut self) {
        if self.live_daemon_pid().is_some() {
            let socket = self.socket.clone();
            let _ = std::thread::spawn(move || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap();
                rt.block_on(async {
                    let shutdown = async {
                        let c = Client::connect(&socket, "e2e-cleanup").await.ok()?;
                        c.call(Request::Shutdown).await.ok()
                    };
                    let _ = tokio::time::timeout(Duration::from_secs(5), shutdown).await;
                });
            })
            .join();
            let deadline = Instant::now() + Duration::from_secs(10);
            while self.live_daemon_pid().is_some() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(50));
            }
            if let Some(pid) = self.live_daemon_pid() {
                eprintln!("killing daemon {pid} that did not shut down");
                kill(pid);
            }
        }
        if let SocketName::Path(p) = &self.socket {
            let _ = std::fs::remove_file(p);
        }
    }
}

#[cfg(unix)]
fn kill(pid: u32) {
    // SAFETY: plain syscall. Only called with the pid recorded by the daemon that
    // holds this test home's lock right now, i.e. a live puppy daemon.
    unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) };
}

#[cfg(windows)]
fn kill(pid: u32) {
    let _ = Command::new("taskkill")
        .args(["/F", "/PID", &pid.to_string()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

/// Waits for `child` to exit (killing it after `within`).
fn wait_exit(mut child: Child, within: Duration) -> ExitStatus {
    let deadline = Instant::now() + within;
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            panic!("puppy did not exit within {within:?}");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

async fn snapshot(c: &Client, id: DownloadId) -> DownloadSnapshot {
    match c.call(Request::Get { id }).await.unwrap() {
        ReplyData::Download { download, .. } => download,
        r => panic!("unexpected reply {r:?}"),
    }
}

/// Polls until download `id` is in a state `done` accepts.
async fn wait_state(
    c: &Client,
    id: DownloadId,
    what: &str,
    done: impl Fn(&DownloadSnapshot) -> bool,
) -> DownloadSnapshot {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        let d = snapshot(c, id).await;
        if done(&d) {
            return d;
        }
        if let WireState::Failed { reason, .. } = &d.state {
            panic!("download failed: {reason}");
        }
        assert!(Instant::now() < deadline, "never {what}: {d:?}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn wait_completed(c: &Client, id: DownloadId) {
    wait_state(c, id, "completed", |d| d.state == WireState::Completed).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn autospawn_then_idle_exit() {
    let env = Env::new();
    let s = TestServer::start(ServerCfg {
        len: 64 * KIB,
        ..ServerCfg::default()
    })
    .await;
    assert!(!env.lock_held());
    let c = env
        .spawn_client()
        .await
        .expect("hello with a spawned daemon");
    let pid = env.daemon_pid().expect("the daemon wrote its pid");
    assert_ne!(pid, std::process::id(), "a separate daemon process");
    assert!(env.lock_held());
    let id = c.add(env.req(&s, "a.bin"), false, None).await.unwrap();
    wait_completed(&c, id).await;
    env.assert_file("a.bin", 64 * KIB);
    // No clients and no work: the daemon idle-exits after 1 s.
    drop(c);
    env.wait_daemon_gone(Duration::from_secs(15)).await;
    assert!(
        Client::connect(&env.socket, "e2e").await.is_err(),
        "nothing listens any more"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_autospawn_single_daemon() {
    let env = std::sync::Arc::new(Env::new());
    let mut tasks = tokio::task::JoinSet::new();
    for _ in 0..4 {
        let env = env.clone();
        tasks.spawn(async move { env.spawn_client().await.expect("connected") });
    }
    let mut clients = Vec::new();
    while let Some(c) = tasks.join_next().await {
        clients.push(c.unwrap());
    }
    // Exactly one daemon: it holds the lock, and every client talks to it.
    assert!(env.lock_held());
    let pid = env.daemon_pid().expect("the daemon wrote its pid");
    let id = clients[0]
        .add(env.paused_req("a.bin"), true, None)
        .await
        .unwrap();
    for c in &clients {
        let list = c.list().await.unwrap();
        assert_eq!(list.len(), 1, "{list:?}");
        assert_eq!(list[0].id, id);
    }
    assert_eq!(env.daemon_pid(), Some(pid));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn restart_daemon_resumes_download() {
    let env = Env::new();
    let len = 1024 * KIB;
    let s = TestServer::start(ServerCfg {
        len,
        bytes_per_sec: Some(32 * KIB),
        ..ServerCfg::default()
    })
    .await;
    let c = env.spawn_client().await.unwrap();
    let id = c.add(env.req(&s, "a.bin"), false, None).await.unwrap();
    wait_state(&c, id, "transferring", |d| {
        d.state == WireState::Active && d.done_bytes > 0
    })
    .await;
    assert_eq!(c.call(Request::Shutdown).await.unwrap(), ReplyData::Done);
    drop(c);
    env.wait_daemon_gone(TIMEOUT).await;
    let saved = std::fs::read_to_string(env.paths().state_file).unwrap();
    assert!(saved.contains("a.bin"), "{saved}");

    s.set_cfg(|cfg| cfg.bytes_per_sec = None);
    let c = env.spawn_client().await.unwrap();
    let first = snapshot(&c, id).await;
    assert!(first.done_bytes > 0, "progress restored: {first:?}");
    wait_completed(&c, id).await;
    env.assert_file("a.bin", len);
}

/// A daemon killed outright mid-transfer: the next client starts a new one, which
/// resumes the download from its files, and ids keep growing.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sigkill_mid_transfer_then_respawn_completes() {
    let env = Env::new();
    let len = 1024 * KIB;
    let s = TestServer::start(ServerCfg {
        len,
        bytes_per_sec: Some(64 * KIB),
        ..ServerCfg::default()
    })
    .await;
    let c = env.spawn_client().await.unwrap();
    let first = c.add(env.paused_req("p.bin"), true, None).await.unwrap();
    let id = c.add(env.req(&s, "a.bin"), false, None).await.unwrap();
    wait_state(&c, id, "transferring", |d| {
        d.state == WireState::Active && d.done_bytes > 0
    })
    .await;
    // Let the debounced state save and a sidecar flush land, as they would in use.
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let pid = env.live_daemon_pid().expect("a daemon runs");
    drop(c);
    // SAFETY: plain syscall on the daemon this test started (its lock is held).
    assert_eq!(unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) }, 0);
    let deadline = Instant::now() + TIMEOUT;
    while env.lock_held() {
        assert!(Instant::now() < deadline, "the killed daemon kept its lock");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    s.set_cfg(|cfg| cfg.bytes_per_sec = None);
    let c = env.spawn_client().await.unwrap();
    assert_ne!(env.live_daemon_pid(), Some(pid));
    wait_completed(&c, id).await;
    env.assert_file("a.bin", len);
    let next = c.add(env.paused_req("q.bin"), true, None).await.unwrap();
    assert!(
        next > id && next > first,
        "id {next} reused (had {first}, {id})"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn status_prints_summary() {
    let env = Env::new();
    let (status, out, err) = env.run(&["status"]);
    assert!(status.success(), "{status:?}: {err}");
    assert_eq!(out, "0 downloads, 0 active\n");
    // It started a daemon (which may already have idle-exited): add something and
    // ask again.
    let c = env.spawn_client().await.unwrap();
    c.add(env.paused_req("a.bin"), true, None).await.unwrap();
    let (status, out, err) = env.run(&["status"]);
    assert!(status.success(), "{status:?}: {err}");
    assert_eq!(out, "1 download, 0 active\n");
}

#[test]
fn tui_flag_parses() {
    let env = Env::new();
    let (status, out, err) = env.run(&["--theme", "light", "--help"]);
    assert!(status.success(), "{status:?}: {err}");
    assert!(out.contains("--theme"), "{out}");
    let (status, _, err) = env.run(&["--theme", "neon", "status"]);
    assert_eq!(status.code(), Some(2), "{err}");
}

#[test]
fn no_tty_fails_gracefully() {
    let env = Env::new();
    // Piped stdio (not a terminal) for stdin, stdout and stderr.
    let out = env
        .command(&[])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2), "{out:?}");
    assert!(out.stdout.is_empty(), "{out:?}");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("puppy's dashboard needs an interactive terminal"),
        "{err}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn second_daemon_exits_zero() {
    let env = Env::new();
    let _c = env.spawn_client().await.unwrap();
    let (status, _, err) = env.run(&["daemon"]);
    assert!(status.success(), "{status:?}: {err}");
    assert!(err.contains("already running"), "{err}");
    assert_eq!(err.lines().count(), 1, "{err}");
}

#[test]
fn version_flag() {
    let env = Env::new();
    let (status, out, _) = env.run(&["--version"]);
    assert!(status.success());
    assert_eq!(out.trim(), format!("puppy {}", env!("CARGO_PKG_VERSION")));
}

#[test]
fn bad_arguments_fail() {
    let env = Env::new();
    let (status, _, err) = env.run(&["daemon", "--idle-exit", "soon"]);
    assert!(!status.success());
    assert!(err.contains("--idle-exit"), "{err}");
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sigterm_shuts_down_gracefully() {
    signal_shuts_down_gracefully(libc::SIGTERM).await;
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sighup_shuts_down_gracefully() {
    signal_shuts_down_gracefully(libc::SIGHUP).await;
}

/// Two signals in a row: the daemon exits promptly either way, releasing its lock and
/// leaving a readable state file.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn second_signal_exits_promptly() {
    let env = Env::new();
    let s = TestServer::start(ServerCfg {
        len: 4096 * KIB,
        bytes_per_sec: Some(256 * KIB),
        ..ServerCfg::default()
    })
    .await;
    let child = env
        .command(&["daemon"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + TIMEOUT;
    let c = loop {
        if let Ok(c) = Client::connect(&env.socket, "e2e").await {
            break c;
        }
        assert!(Instant::now() < deadline, "the daemon never listened");
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    let id = c.add(env.req(&s, "a.bin"), false, None).await.unwrap();
    wait_state(&c, id, "transferring", |d| d.done_bytes > 0).await;
    let pid = child.id() as libc::pid_t;
    for _ in 0..2 {
        // SAFETY: plain syscall on our own child.
        assert_eq!(unsafe { libc::kill(pid, libc::SIGTERM) }, 0);
    }
    let t = Instant::now();
    tokio::task::spawn_blocking(move || wait_exit(child, TIMEOUT))
        .await
        .unwrap();
    assert!(t.elapsed() < Duration::from_secs(5), "{:?}", t.elapsed());
    assert!(!env.lock_held());
    let state: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(env.paths().state_file).unwrap()).unwrap();
    assert_eq!(state["downloads"][0]["id"], id, "{state:#}");
    drop(c);
}

#[cfg(unix)]
async fn signal_shuts_down_gracefully(signal: libc::c_int) {
    let env = Env::new();
    let child = env
        .command(&["daemon"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + TIMEOUT;
    let c = loop {
        if let Ok(c) = Client::connect(&env.socket, "e2e").await {
            break c;
        }
        assert!(Instant::now() < deadline, "the daemon never listened");
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    let id = c.add(env.paused_req("a.bin"), true, None).await.unwrap();
    // SAFETY: plain syscall on our own child.
    assert_eq!(unsafe { libc::kill(child.id() as libc::pid_t, signal) }, 0);
    let status = tokio::task::spawn_blocking(move || wait_exit(child, TIMEOUT))
        .await
        .unwrap();
    assert!(status.success(), "{status:?}");
    assert!(!env.socket_file_exists(), "socket removed");
    assert!(env.daemon_pid().is_none(), "pid file removed");
    let state: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(env.paths().state_file).unwrap()).unwrap();
    assert_eq!(state["downloads"][0]["id"], id, "{state:#}");
    drop(c);
}

/// The cleanup guard never kills the process a stale pid file names (a crashed
/// daemon's pid may since belong to an unrelated process).
#[cfg(unix)]
#[test]
fn cleanup_ignores_stale_pid_file() {
    let mut bystander = Command::new("sleep").arg("30").spawn().unwrap();
    let env = Env::new();
    std::fs::write(env.paths().pid_file, format!("{}\n", bystander.id())).unwrap();
    assert!(!env.lock_held());
    drop(env);
    // A signal is delivered asynchronously: give a kill time to land.
    std::thread::sleep(Duration::from_millis(500));
    let alive = bystander.try_wait().unwrap().is_none();
    let _ = bystander.kill();
    let _ = bystander.wait();
    assert!(alive, "the cleanup killed an unrelated process");
}
