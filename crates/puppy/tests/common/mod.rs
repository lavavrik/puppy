//! Shared helpers for the integration tests: a per-test `PUPPY_HOME`/`PUPPY_SOCKET`
//! environment ([`Env`]) that never leaks a daemon.
#![allow(dead_code)]

use puppy_client::{Client, ClientError};
use puppy_core::types::{AddRequest, DownloadId, RequestContext, Source};
use puppy_daemon::{Paths, SocketName};
use puppy_proto::{DownloadSnapshot, ReplyData, Request, WireState};
use puppy_testserver::{TestServer, body_byte};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

pub const BIN: &str = env!("CARGO_BIN_EXE_puppy");
pub const TIMEOUT: Duration = Duration::from_secs(30);

pub const KIB: u64 = 1024;

/// A short, unique socket name (macOS `sun_path` is ~104 bytes).
pub fn socket_name() -> SocketName {
    static N: AtomicUsize = AtomicUsize::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    if cfg!(windows) {
        SocketName::Pipe(format!("puppy-e2e-{pid}-{n}"))
    } else {
        SocketName::Path(PathBuf::from(format!("/tmp/pz-{pid}-e{n}.sock")))
    }
}

pub struct Env {
    pub home: tempfile::TempDir,
    pub socket: SocketName,
}

impl Env {
    pub fn new() -> Self {
        Env {
            home: tempfile::tempdir().unwrap(),
            socket: socket_name(),
        }
    }

    pub fn paths(&self) -> Paths {
        Paths::with_home(self.home.path(), self.socket.clone()).unwrap()
    }

    pub fn dl_dir(&self) -> PathBuf {
        self.home.path().join("dl")
    }

    /// The environment every `puppy` process of this test gets.
    pub fn vars(&self) -> Vec<(&'static str, String)> {
        let socket = match &self.socket {
            SocketName::Path(p) => p.to_str().unwrap().to_string(),
            SocketName::Pipe(n) => n.clone(),
        };
        vec![
            ("PUPPY_HOME", self.home.path().to_str().unwrap().to_string()),
            ("PUPPY_SOCKET", socket),
            ("PUPPY_IDLE_EXIT", "1".to_string()),
            // The daemon's torrent engine stays off the network.
            ("PUPPY_TORRENT_TEST_MODE", "1".to_string()),
        ]
    }

    /// `Client::connect_or_spawn` with the real binary.
    pub async fn spawn_client(&self) -> Result<Client, ClientError> {
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
    pub fn command(&self, args: &[&str]) -> Command {
        let mut cmd = Command::new(BIN);
        cmd.args(args).stdin(Stdio::null());
        for (k, v) in self.vars() {
            cmd.env(k, v);
        }
        // Table widths must not depend on the developer's shell.
        cmd.env("COLUMNS", "100");
        cmd
    }

    /// Runs `puppy <args>` to completion: (status, stdout, stderr). Output goes
    /// through files, not pipes: a daemon the command starts may inherit them, and
    /// must not keep this waiting.
    pub fn run(&self, args: &[&str]) -> (ExitStatus, String, String) {
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

    pub fn daemon_pid(&self) -> Option<u32> {
        let text = std::fs::read_to_string(self.paths().pid_file).ok()?;
        text.trim().parse().ok()
    }

    /// Whether some daemon holds this home's lock.
    pub fn lock_held(&self) -> bool {
        self.try_lock_held()
            .unwrap_or_else(|e| panic!("cannot test the lock: {e}"))
    }

    pub fn try_lock_held(&self) -> std::io::Result<bool> {
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
    pub fn live_daemon_pid(&self) -> Option<u32> {
        let pid = self.daemon_pid()?;
        self.try_lock_held().ok()?.then_some(pid)
    }

    /// Waits until the daemon is gone: lock free, pid file and socket file removed.
    pub async fn wait_daemon_gone(&self, within: Duration) {
        let deadline = Instant::now() + within;
        while self.lock_held() || self.paths().pid_file.exists() || self.socket_file_exists() {
            assert!(Instant::now() < deadline, "the daemon did not exit");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    pub fn socket_file_exists(&self) -> bool {
        match &self.socket {
            SocketName::Path(p) => p.exists(),
            SocketName::Pipe(_) => false,
        }
    }

    pub fn paused_req(&self, name: &str) -> AddRequest {
        AddRequest {
            source: Source::Http {
                url: "http://127.0.0.1:9/never".parse().unwrap(),
            },
            output_dir: Some(self.dl_dir()),
            filename: Some(name.into()),
            context: RequestContext::default(),
            files: Default::default(),
            seed: None,
        }
    }

    pub fn req(&self, s: &TestServer, name: &str) -> AddRequest {
        AddRequest {
            source: Source::Http {
                url: s.url("/file.bin"),
            },
            output_dir: Some(self.dl_dir()),
            filename: Some(name.into()),
            context: RequestContext::default(),
            files: Default::default(),
            seed: None,
        }
    }

    pub fn assert_file(&self, name: &str, len: u64) {
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
pub fn wait_exit(mut child: Child, within: Duration) -> ExitStatus {
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

pub async fn snapshot(c: &Client, id: DownloadId) -> DownloadSnapshot {
    match c.call(Request::Get { id }).await.unwrap() {
        ReplyData::Download { download, .. } => download,
        r => panic!("unexpected reply {r:?}"),
    }
}

/// Polls until download `id` is in a state `done` accepts.
pub async fn wait_state(
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

pub async fn wait_completed(c: &Client, id: DownloadId) {
    wait_state(c, id, "completed", |d| d.state == WireState::Completed).await;
}
