//! Daemon lifecycle: single instance, stale socket, idle exit, graceful shutdown.
//!
//! Each test runs `run_daemon` in-process with explicit paths (its own home tempdir and
//! a short socket name), so tests can run in parallel without touching the environment.

use interprocess::local_socket::tokio::{Stream, prelude::*};
use interprocess::local_socket::{GenericFilePath, GenericNamespaced};
use puppy_core::config::HttpConfig;
use puppy_core::types::{AddRequest, DownloadId, RequestContext, Source};
use puppy_daemon::lifecycle::{DaemonOptions, RunOutcome, run_daemon};
use puppy_daemon::notify::{Notification, NotifyKind, RecordingNotifier};
use puppy_daemon::{Paths, SocketName};
use puppy_proto::{
    Envelope, MsgReader, PROTOCOL_VERSION, Reply, ReplyData, Request, ServerMsg, WireState,
    write_msg,
};
use puppy_testserver::{ServerCfg, TestServer};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};
use tokio::io::{BufReader, ReadHalf, WriteHalf};
use tokio::task::JoinHandle;

const KIB: u64 = 1024;
const TIMEOUT: Duration = Duration::from_secs(20);

type Daemon = JoinHandle<anyhow::Result<RunOutcome>>;

/// A short, unique socket name (macOS `sun_path` is ~104 bytes).
fn socket_name() -> SocketName {
    static N: AtomicUsize = AtomicUsize::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    if cfg!(windows) {
        SocketName::Pipe(format!("puppy-lc-{pid}-{n}"))
    } else {
        SocketName::Path(PathBuf::from(format!("/tmp/pz-{pid}-{n}.sock")))
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

    fn dl_dir(&self) -> PathBuf {
        self.home.path().join("dl")
    }

    fn paths(&self) -> Paths {
        Paths::with_home(self.home.path(), self.socket.clone()).unwrap()
    }

    fn opts(&self, idle_exit: Option<Duration>) -> DaemonOptions {
        DaemonOptions {
            idle_exit,
            paths: Some(self.paths()),
            http: Some(HttpConfig {
                backoff_base_ms: 10,
                backoff_max_ms: 100,
                flush_interval_ms: 100,
                read_timeout_ms: 2_000,
                validate_timeout_ms: 1_500,
                default_dir: self.dl_dir(),
                ..HttpConfig::default()
            }),
            engine: None,
            torrent_test: Some(Default::default()),
            notifier: None,
            update: None,
            update_off_reason: None,
            lock_wait: None,
        }
    }

    /// Like `opts`, with notifications going to `sink`.
    fn opts_notify(&self, sink: RecordingNotifier) -> DaemonOptions {
        DaemonOptions {
            notifier: Some(Arc::new(sink)),
            ..self.opts(None)
        }
    }

    /// Starts a daemon with notifications going to `sink`.
    async fn start_notify(&self, sink: RecordingNotifier) -> Daemon {
        let d = tokio::spawn(run_daemon(self.opts_notify(sink)));
        let deadline = Instant::now() + TIMEOUT;
        loop {
            assert!(!d.is_finished(), "daemon exited early: {:?}", d.await);
            if self.try_connect().await.is_some() {
                return d;
            }
            assert!(Instant::now() < deadline, "daemon never listened");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// Starts a daemon and waits until it accepts connections.
    async fn start(&self, idle_exit: Option<Duration>) -> Daemon {
        let d = tokio::spawn(run_daemon(self.opts(idle_exit)));
        let deadline = Instant::now() + TIMEOUT;
        loop {
            assert!(!d.is_finished(), "daemon exited early: {:?}", d.await);
            if self.try_connect().await.is_some() {
                return d;
            }
            assert!(Instant::now() < deadline, "daemon never listened");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    async fn try_connect(&self) -> Option<Client> {
        let stream = match &self.socket {
            SocketName::Path(p) => {
                Stream::connect(p.as_path().to_fs_name::<GenericFilePath>().ok()?).await
            }
            SocketName::Pipe(n) => {
                Stream::connect(n.as_str().to_ns_name::<GenericNamespaced>().ok()?).await
            }
        }
        .ok()?;
        let (rd, wr) = tokio::io::split(stream);
        Some(Client {
            rd: MsgReader::new(BufReader::new(rd)),
            wr,
            next_id: 1,
        })
    }

    /// A connected client that has completed the handshake.
    async fn client(&self) -> Client {
        let mut c = self.try_connect().await.expect("connect");
        let r = c
            .request(Request::Hello {
                client: "test".into(),
                protocol: PROTOCOL_VERSION,
            })
            .await;
        assert!(
            matches!(
                r,
                Reply::Ok {
                    data: ReplyData::Hello { .. }
                }
            ),
            "{r:?}"
        );
        c
    }

    fn req(&self, s: &TestServer, name: &str) -> AddRequest {
        AddRequest {
            source: Source::Http {
                url: s.url("/file.bin"),
            },
            output_dir: Some(self.dl_dir()),
            filename: Some(name.into()),
            context: RequestContext::default(),
            files: Default::default(),
            seed: None,
            checksum: None,
        }
    }

    fn state_json(&self) -> serde_json::Value {
        let text = std::fs::read_to_string(self.paths().state_file).unwrap();
        serde_json::from_str(&text).unwrap()
    }

    #[cfg(unix)]
    fn socket_path(&self) -> &std::path::Path {
        match &self.socket {
            SocketName::Path(p) => p,
            SocketName::Pipe(_) => unreachable!(),
        }
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        if let SocketName::Path(p) = &self.socket {
            let _ = std::fs::remove_file(p);
        }
    }
}

/// Minimal raw protocol client.
struct Client {
    rd: MsgReader<BufReader<ReadHalf<Stream>>>,
    wr: WriteHalf<Stream>,
    next_id: u64,
}

impl Client {
    async fn request(&mut self, body: Request) -> Reply {
        let req_id = self.next_id;
        self.next_id += 1;
        write_msg(&mut self.wr, &Envelope { req_id, body })
            .await
            .unwrap();
        loop {
            let msg = tokio::time::timeout(TIMEOUT, self.rd.next::<ServerMsg>())
                .await
                .expect("timed out waiting for a response")
                .ok()
                .flatten();
            match msg {
                Some(ServerMsg::Response { req_id: r, result }) if r == req_id => return result,
                Some(_) => {}
                None => panic!("connection closed waiting for response {req_id}"),
            }
        }
    }

    async fn add(&mut self, req: AddRequest) -> DownloadId {
        match self
            .request(Request::Add {
                request: req,
                start_paused: false,
                limit: None,
            })
            .await
        {
            Reply::Ok {
                data: ReplyData::Added { id },
            } => id,
            r => panic!("add failed: {r:?}"),
        }
    }

    async fn get(&mut self, id: DownloadId) -> (WireState, u64) {
        match self.request(Request::Get { id }).await {
            Reply::Ok {
                data: ReplyData::Download { download, .. },
            } => (download.state, download.done_bytes),
            r => panic!("get failed: {r:?}"),
        }
    }
}

/// Waits for the daemon to exit (within `within`) and returns its outcome.
async fn exited(d: Daemon, within: Duration) -> RunOutcome {
    tokio::time::timeout(within, d)
        .await
        .expect("daemon did not exit in time")
        .expect("daemon task panicked")
        .expect("daemon failed")
}

async fn shutdown(env: &Env, d: Daemon) {
    let mut c = env.client().await;
    let r = c.request(Request::Shutdown).await;
    assert!(
        matches!(
            r,
            Reply::Ok {
                data: ReplyData::Done
            }
        ),
        "{r:?}"
    );
    assert_eq!(exited(d, TIMEOUT).await, RunOutcome::Exited);
    #[cfg(unix)]
    assert!(!env.socket_path().exists(), "socket file removed on exit");
}

async fn server(f: impl FnOnce(&mut ServerCfg)) -> TestServer {
    let mut c = ServerCfg::default();
    f(&mut c);
    TestServer::start(c).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn second_daemon_exits_when_locked() {
    let env = Env::new();
    let d = env.start(None).await;
    let second = tokio::time::timeout(Duration::from_secs(1), run_daemon(env.opts(None)))
        .await
        .expect("second daemon returned within 1 s")
        .unwrap();
    assert_eq!(second, RunOutcome::AlreadyRunning);
    // The second daemon left the first one's socket alone.
    env.client().await;
    shutdown(&env, d).await;
    // Once the first one is gone, a new daemon starts.
    let d = env.start(None).await;
    shutdown(&env, d).await;
}

/// With `lock_wait`, a daemon whose predecessor is still exiting takes over once the
/// lock is released, and gives up after the wait otherwise.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn lock_wait_takes_over_a_departing_daemon() {
    let env = Env::new();
    let d = env.start(None).await;
    let mut opts = env.opts(None);
    opts.lock_wait = Some(Duration::from_millis(300));
    let started = std::time::Instant::now();
    assert_eq!(run_daemon(opts).await.unwrap(), RunOutcome::AlreadyRunning);
    assert!(started.elapsed() >= Duration::from_millis(250));

    let mut opts = env.opts(None);
    opts.lock_wait = Some(Duration::from_secs(10));
    let second = tokio::spawn(run_daemon(opts));
    tokio::time::sleep(Duration::from_millis(300)).await;
    shutdown(&env, d).await;
    // The waiting daemon now runs; stop it.
    let mut c = tokio::time::timeout(TIMEOUT, async {
        loop {
            if let Some(c) = env.try_connect().await {
                break c;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("the waiting daemon never took over");
    let _ = c
        .request(Request::Hello {
            client: "test".into(),
            protocol: PROTOCOL_VERSION,
        })
        .await;
    let _ = c.request(Request::Shutdown).await;
    let outcome = tokio::time::timeout(TIMEOUT, second)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(outcome.unwrap(), RunOutcome::Exited);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pid_file_written_while_running() {
    let env = Env::new();
    let pid_file = env.paths().pid_file;
    assert_eq!(pid_file, env.paths().data_dir.join("puppyd.pid"));
    let d = env.start(None).await;
    let pid = std::fs::read_to_string(&pid_file).unwrap();
    assert_eq!(pid.trim(), std::process::id().to_string());
    // A second daemon (lock taken) leaves it alone.
    let second = run_daemon(env.opts(None)).await.unwrap();
    assert_eq!(second, RunOutcome::AlreadyRunning);
    assert!(pid_file.exists());
    shutdown(&env, d).await;
    assert!(!pid_file.exists(), "pid file removed on exit");
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn regular_file_at_socket_path_is_not_removed() {
    let env = Env::new();
    std::fs::write(env.socket_path(), b"user data").unwrap();
    let r = tokio::time::timeout(TIMEOUT, run_daemon(env.opts(None)))
        .await
        .unwrap();
    let e = r.expect_err("daemon must refuse a regular file at the socket path");
    assert!(format!("{e:#}").contains("not a socket"), "{e:#}");
    assert_eq!(std::fs::read(env.socket_path()).unwrap(), b"user data");
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn live_socket_not_stolen() {
    let env = Env::new();
    // Something else (e.g. a daemon with another home) is listening there.
    let live = std::os::unix::net::UnixListener::bind(env.socket_path()).unwrap();
    let r = tokio::time::timeout(TIMEOUT, run_daemon(env.opts(None)))
        .await
        .unwrap();
    let e = r.expect_err("daemon must refuse a socket someone is listening on");
    assert!(format!("{e:#}").contains("already listening"), "{e:#}");
    assert!(env.socket_path().exists(), "the live socket was left alone");
    // The listener still works.
    let _c = std::os::unix::net::UnixStream::connect(env.socket_path()).unwrap();
    live.accept().unwrap();
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stale_socket_is_replaced() {
    let env = Env::new();
    // A stale socket (left by a killed daemon) is replaced.
    let stale = std::os::unix::net::UnixListener::bind(env.socket_path()).unwrap();
    drop(stale);
    assert!(env.socket_path().exists());
    let d = env.start(None).await;
    env.client().await;
    shutdown(&env, d).await;
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn directory_at_socket_path_is_not_removed() {
    let env = Env::new();
    std::fs::create_dir(env.socket_path()).unwrap();
    std::fs::write(env.socket_path().join("keep"), b"x").unwrap();
    let r = tokio::time::timeout(TIMEOUT, run_daemon(env.opts(None)))
        .await
        .unwrap();
    let e = r.expect_err("daemon must refuse a directory at the socket path");
    assert!(format!("{e:#}").contains("not a socket"), "{e:#}");
    assert!(env.socket_path().join("keep").exists());
    std::fs::remove_dir_all(env.socket_path()).unwrap();
}

/// A state file written by a newer puppy is never touched: the daemon refuses to start.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn newer_state_refuses_to_start() {
    let env = Env::new();
    let paths = env.paths();
    let text = r#"{"version": 2, "next_id": 9, "order": [], "downloads": []}"#;
    std::fs::write(&paths.state_file, text).unwrap();
    let r = tokio::time::timeout(TIMEOUT, run_daemon(env.opts(None)))
        .await
        .unwrap();
    let e = r.expect_err("daemon must refuse to start");
    assert!(format!("{e:#}").contains("newer puppy"), "{e:#}");
    assert_eq!(std::fs::read_to_string(&paths.state_file).unwrap(), text);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unreadable_state_refuses_to_start() {
    let env = Env::new();
    let paths = env.paths();
    // A state file that exists but cannot be read (a corrupt one is quarantined instead).
    std::fs::create_dir(&paths.state_file).unwrap();
    std::fs::write(paths.state_file.join("keep"), b"x").unwrap();
    let r = tokio::time::timeout(TIMEOUT, run_daemon(env.opts(None)))
        .await
        .unwrap();
    let e = r.expect_err("daemon must refuse to start");
    assert!(format!("{e:#}").contains("unreadable"), "{e:#}");
    assert!(paths.state_file.join("keep").exists());
    // It released the lock and did not leave a socket behind.
    #[cfg(unix)]
    assert!(!env.socket_path().exists());
    std::fs::remove_dir_all(&paths.state_file).unwrap();
    let d = env.start(None).await;
    shutdown(&env, d).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn idle_exit_when_no_work_and_no_clients() {
    let env = Env::new();
    let idle = Some(Duration::from_millis(500));
    let d = env.start(idle).await;
    assert_eq!(exited(d, Duration::from_secs(2)).await, RunOutcome::Exited);
    #[cfg(unix)]
    assert!(!env.socket_path().exists(), "socket file removed on exit");
    assert!(env.home.path().join("data/logs/puppyd.log").exists());

    // A connected client keeps it alive until it disconnects.
    let d = env.start(idle).await;
    let c = env.client().await;
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert!(!d.is_finished(), "exited while a client was connected");
    drop(c);
    assert_eq!(exited(d, Duration::from_secs(2)).await, RunOutcome::Exited);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn no_idle_exit_while_downloading() {
    let env = Env::new();
    let s = server(|c| {
        c.len = 256 * KIB;
        c.bytes_per_sec = Some(96 * KIB);
    })
    .await;
    let d = env.start(Some(Duration::from_millis(500))).await;
    let id = {
        let mut c = env.client().await;
        c.add(env.req(&s, "a.bin")).await
    };
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert!(!d.is_finished(), "exited while downloading");
    // Once the download completes it goes idle and exits.
    assert_eq!(exited(d, TIMEOUT).await, RunOutcome::Exited);
    let st = env.state_json();
    let rec = &st["downloads"][0];
    assert_eq!(rec["id"], id);
    assert_eq!(rec["state"]["kind"], "completed", "{st:#}");
    let len = std::fs::metadata(env.dl_dir().join("a.bin")).unwrap().len();
    assert_eq!(len, 256 * KIB);
}

/// A download still being probed (`Starting`) is work: no idle exit meanwhile.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn no_idle_exit_while_starting() {
    let env = Env::new();
    let s = server(|c| c.stall_headers = true).await;
    let d = env.start(Some(Duration::from_millis(300))).await;
    let id = {
        let mut c = env.client().await;
        let id = c.add(env.req(&s, "a.bin")).await;
        assert_eq!(c.get(id).await.0, WireState::Starting);
        id
    };
    tokio::time::sleep(Duration::from_millis(1200)).await;
    assert!(!d.is_finished(), "exited while a download was starting");
    let mut c = env.client().await;
    assert_eq!(c.get(id).await.0, WireState::Starting);
    drop(c);
    shutdown(&env, d).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shutdown_persists_and_pauses() {
    let env = Env::new();
    let s = server(|c| {
        c.len = 2048 * KIB;
        c.bytes_per_sec = Some(64 * KIB);
    })
    .await;
    let d = env.start(None).await;
    let mut c = env.client().await;
    let id = c.add(env.req(&s, "a.bin")).await;
    let deadline = Instant::now() + TIMEOUT;
    loop {
        let (state, done) = c.get(id).await;
        if state == WireState::Active && done > 0 {
            break;
        }
        assert!(Instant::now() < deadline, "download never transferred");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    shutdown(&env, d).await;
    let st = env.state_json();
    let rec = &st["downloads"][0];
    assert_eq!(rec["id"], id);
    assert_eq!(rec["state"]["kind"], "active", "{st:#}");
    let path = PathBuf::from(rec["path"].as_str().expect("path recorded"));
    assert_eq!(path, env.dl_dir().join("a.bin"));
    let sidecar = puppy_core::http::resume::sidecar_path(&path);
    assert!(sidecar.exists(), "sidecar flushed at {}", sidecar.display());
}

/// An engine whose restores take 3 s.
struct SlowRestore(puppy_core::http::engine::HttpEngine);

#[async_trait::async_trait]
impl puppy_core::engine::Engine for SlowRestore {
    fn kind(&self) -> puppy_core::engine::EngineKind {
        self.0.kind()
    }
    fn accepts(&self, source: &Source) -> bool {
        self.0.accepts(source)
    }
    async fn add(
        &self,
        id: DownloadId,
        req: AddRequest,
        mode: puppy_core::engine::StartMode,
        opts: puppy_core::engine::AddOptions,
    ) -> Result<puppy_core::engine::Added, puppy_core::engine::EngineError> {
        if matches!(mode, puppy_core::engine::StartMode::Restore { .. }) {
            tokio::time::sleep(Duration::from_secs(3)).await;
        }
        self.0.add(id, req, mode, opts).await
    }
    async fn pause(&self, id: DownloadId) -> Result<(), puppy_core::engine::EngineError> {
        self.0.pause(id).await
    }
    async fn resume(&self, id: DownloadId) -> Result<(), puppy_core::engine::EngineError> {
        self.0.resume(id).await
    }
    async fn remove(
        &self,
        id: DownloadId,
        delete_files: bool,
    ) -> Result<(), puppy_core::engine::EngineError> {
        self.0.remove(id, delete_files).await
    }
    fn status(&self, id: DownloadId) -> Option<puppy_core::types::DownloadStatus> {
        self.0.status(id)
    }
    fn detail(&self, id: DownloadId) -> Option<puppy_core::types::EngineDetail> {
        self.0.detail(id)
    }
    fn set_download_limit(
        &self,
        id: DownloadId,
        bps: Option<u64>,
    ) -> Result<(), puppy_core::engine::EngineError> {
        self.0.set_download_limit(id, bps)
    }
    fn set_global_download_budget(&self, bps: Option<u64>) {
        self.0.set_global_download_budget(bps)
    }
    fn subscribe(&self) -> tokio::sync::broadcast::Receiver<puppy_core::engine::EngineEvent> {
        self.0.subscribe()
    }
}

/// Restoring downloads never delays the socket: `hello` is answered within 500 ms
/// although the restore takes 3 s.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn slow_restore_does_not_block_the_socket() {
    let env = Env::new();
    let s = server(|c| {
        c.len = 512 * KIB;
        c.bytes_per_sec = Some(128 * KIB);
    })
    .await;
    // A download paused mid-way, with files to restore.
    let d = env.start(None).await;
    let mut c = env.client().await;
    let id = c.add(env.req(&s, "a.bin")).await;
    let deadline = Instant::now() + TIMEOUT;
    while c.get(id).await.1 == 0 {
        assert!(Instant::now() < deadline, "no progress");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(matches!(
        c.request(Request::Pause { id }).await,
        Reply::Ok { .. }
    ));
    drop(c);
    shutdown(&env, d).await;

    let mut opts = env.opts(None);
    let http = opts.http.clone().unwrap();
    opts.engine = Some(Arc::new(SlowRestore(
        puppy_core::http::engine::HttpEngine::new(http).unwrap(),
    )));
    let t = Instant::now();
    let d = tokio::spawn(run_daemon(opts));
    let mut c = loop {
        if let Some(c) = env.try_connect().await {
            break c;
        }
        assert!(t.elapsed() < TIMEOUT, "daemon never listened");
        tokio::time::sleep(Duration::from_millis(10)).await;
    };
    let r = c
        .request(Request::Hello {
            client: "test".into(),
            protocol: PROTOCOL_VERSION,
        })
        .await;
    let waited = t.elapsed();
    assert!(matches!(r, Reply::Ok { .. }), "{r:?}");
    assert!(waited < Duration::from_millis(500), "hello took {waited:?}");
    // Shown with its stored state while the restore runs.
    assert_eq!(c.get(id).await.0, WireState::Paused);
    drop(c);
    shutdown(&env, d).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn torrent_engine_failure_fails_torrents_transiently() {
    let env = Env::new();
    // The torrent session cannot create its state dir: a regular file is in the way.
    std::fs::write(env.paths().data_dir.join("torrent"), b"in the way").unwrap();
    let d = env.start(None).await;
    let mut c = env.client().await;
    let magnet = c
        .add(AddRequest {
            source: Source::Magnet {
                uri: format!("magnet:?xt=urn:btih:{}", "ab".repeat(20)),
            },
            output_dir: Some(env.dl_dir()),
            filename: None,
            context: RequestContext::default(),
            files: Default::default(),
            seed: None,
            checksum: None,
        })
        .await;
    let deadline = Instant::now() + TIMEOUT;
    loop {
        match c.get(magnet).await.0 {
            WireState::Failed { reason, transient } => {
                assert_eq!(reason, "torrent engine unavailable (see daemon log)");
                assert!(transient, "retried automatically");
                break;
            }
            other => assert!(Instant::now() < deadline, "magnet never failed: {other:?}"),
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    // HTTP downloads still work.
    let s = server(|_| {}).await;
    let id = c
        .add(AddRequest {
            source: Source::Http {
                url: s.url("/file.bin"),
            },
            output_dir: Some(env.dl_dir()),
            filename: Some("h.bin".into()),
            context: RequestContext::default(),
            files: Default::default(),
            seed: None,
            checksum: None,
        })
        .await;
    let deadline = Instant::now() + TIMEOUT;
    while c.get(id).await.0 != WireState::Completed {
        assert!(Instant::now() < deadline, "http download never completed");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    drop(c);
    shutdown(&env, d).await;
}

/// Waits until download `id` reaches `want`.
async fn wait_state(c: &mut Client, id: DownloadId, want: fn(&WireState) -> bool) {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        let (st, _) = c.get(id).await;
        if want(&st) {
            return;
        }
        assert!(Instant::now() < deadline, "state never reached: {st:?}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// A download that finishes just before the daemon stops still gets its popup.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pending_notification_flushed_on_shutdown() {
    let env = Env::new();
    let s = server(|c| c.len = 16 * KIB).await;
    let rec = RecordingNotifier::default();
    let d = env.start_notify(rec.clone()).await;
    let mut c = env.client().await;
    let id = c.add(env.req(&s, "a.bin")).await;
    wait_state(&mut c, id, |s| matches!(s, WireState::Completed)).await;
    drop(c);
    shutdown(&env, d).await;
    assert_eq!(
        rec.take(),
        [Notification {
            kind: NotifyKind::Completed,
            title: "puppy".into(),
            body: "Finished: a.bin".into()
        }]
    );
}

/// Runs a download that fails permanently with notifications on; returns the popups.
async fn failing_run(env: &Env) -> Vec<Notification> {
    let s = server(|_| {}).await;
    let rec = RecordingNotifier::default();
    let d = env.start_notify(rec.clone()).await;
    let mut c = env.client().await;
    let mut req = env.req(&s, "a.bin");
    req.source = Source::Http {
        url: s.url("/missing"),
    };
    let id = c.add(req).await;
    wait_state(&mut c, id, |s| matches!(s, WireState::Failed { .. })).await;
    drop(c);
    shutdown(env, d).await;
    rec.take()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn permanent_failure_notifies_and_config_disables() {
    let env = Env::new();
    let got = failing_run(&env).await;
    assert_eq!(got.len(), 1, "{got:?}");
    assert_eq!(got[0].kind, NotifyKind::Failed);
    assert!(
        got[0].body.starts_with("Failed: a.bin — "),
        "{}",
        got[0].body
    );

    let env = Env::new();
    let cfg = env.paths().config_file();
    std::fs::create_dir_all(cfg.parent().unwrap()).unwrap();
    std::fs::write(&cfg, "[notifications]\nenabled = false\n").unwrap();
    assert!(failing_run(&env).await.is_empty());
}
