//! Daemon lifecycle: single instance, stale socket, idle exit, graceful shutdown.
//!
//! Each test runs `run_daemon` in-process with explicit paths (its own home tempdir and
//! a short socket name), so tests can run in parallel without touching the environment.

use interprocess::local_socket::tokio::{Stream, prelude::*};
use interprocess::local_socket::{GenericFilePath, GenericNamespaced};
use puppy_core::config::HttpConfig;
use puppy_core::types::{AddRequest, DownloadId, DownloadState, RequestContext, Source};
use puppy_daemon::lifecycle::{DaemonOptions, RunOutcome, run_daemon};
use puppy_daemon::{Paths, SocketName};
use puppy_proto::{
    Envelope, MsgReader, PROTOCOL_VERSION, Reply, ReplyData, Request, ServerMsg, write_msg,
};
use puppy_testserver::{ServerCfg, TestServer};
use std::path::PathBuf;
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

    async fn get(&mut self, id: DownloadId) -> (DownloadState, u64) {
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
        if state == DownloadState::Active && done > 0 {
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
