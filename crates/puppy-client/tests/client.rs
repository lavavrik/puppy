//! The daemon client against an in-process daemon (`run_daemon`) or a fake server.
//!
//! Each test uses its own home tempdir and a short socket name, so tests run in
//! parallel without touching the environment. Tests that spawn the real `puppy`
//! binary live in `crates/puppy/tests/`.

use interprocess::local_socket::tokio::{Stream, prelude::*};
use interprocess::local_socket::{GenericFilePath, GenericNamespaced, ListenerOptions};
use puppy_client::{Client, ClientError};
use puppy_core::config::HttpConfig;
use puppy_core::types::{AddRequest, DownloadId, RequestContext, Source};
use puppy_daemon::lifecycle::{DaemonOptions, RunOutcome, run_daemon};
use puppy_daemon::{Paths, SocketName};
use puppy_proto::{
    Envelope, ErrorCode, Event, MsgReader, PROTOCOL_VERSION, Reply, ReplyData, Request, ServerMsg,
    write_msg,
};
use puppy_testserver::{ServerCfg, TestServer};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};
use tokio::io::BufReader;
use tokio::task::JoinHandle;

const TIMEOUT: Duration = Duration::from_secs(20);
const KIB: u64 = 1024;

type Daemon = JoinHandle<anyhow::Result<RunOutcome>>;

/// A short, unique socket name (macOS `sun_path` is ~104 bytes).
fn socket_name() -> SocketName {
    static N: AtomicUsize = AtomicUsize::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    if cfg!(windows) {
        SocketName::Pipe(format!("puppy-cl-{pid}-{n}"))
    } else {
        SocketName::Path(PathBuf::from(format!("/tmp/pz-{pid}-c{n}.sock")))
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

    fn opts(&self) -> DaemonOptions {
        DaemonOptions {
            idle_exit: None,
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
        }
    }

    /// Starts a daemon and waits until a client can connect.
    async fn start(&self) -> Daemon {
        let d = tokio::spawn(run_daemon(self.opts()));
        let deadline = Instant::now() + TIMEOUT;
        loop {
            assert!(!d.is_finished(), "daemon exited early: {:?}", d.await);
            if Client::connect(&self.socket, "probe").await.is_ok() {
                return d;
            }
            assert!(Instant::now() < deadline, "daemon never listened");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    async fn client(&self) -> Client {
        tokio::time::timeout(TIMEOUT, Client::connect(&self.socket, "test"))
            .await
            .expect("connect timed out")
            .expect("connect")
    }

    /// A paused download that never touches the network.
    fn paused_req(&self, name: &str) -> AddRequest {
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
        }
    }

    async fn shutdown(&self, d: Daemon) {
        let c = self.client().await;
        assert_eq!(c.call(Request::Shutdown).await.unwrap(), ReplyData::Done);
        let r = tokio::time::timeout(TIMEOUT, d)
            .await
            .expect("daemon did not exit")
            .unwrap()
            .unwrap();
        assert_eq!(r, RunOutcome::Exited);
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        if let SocketName::Path(p) = &self.socket {
            let _ = std::fs::remove_file(p);
        }
    }
}

async fn next_event(c: &mut Client) -> Option<Event> {
    tokio::time::timeout(TIMEOUT, c.next_event())
        .await
        .expect("timed out waiting for an event")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn call_roundtrip_add_list_get() {
    let env = Env::new();
    let d = env.start().await;
    let c = env.client().await;
    assert!(c.is_connected());
    let id = c
        .add(env.paused_req("a.bin"), true, Some(1000))
        .await
        .unwrap();
    let list = c.list().await.unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].id, id);
    assert_eq!(list[0].limit, Some(1000));
    match c.call(Request::Get { id }).await.unwrap() {
        ReplyData::Download { download, .. } => assert_eq!(download.id, id),
        r => panic!("unexpected reply {r:?}"),
    }
    assert!(matches!(
        c.call(Request::GetSettings).await.unwrap(),
        ReplyData::Settings { .. }
    ));
    drop(c);
    env.shutdown(d).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_calls_matched_by_req_id() {
    let env = Env::new();
    let d = env.start().await;
    let c = Arc::new(env.client().await);
    let mut ids: Vec<DownloadId> = Vec::new();
    for i in 0..10 {
        ids.push(
            c.add(env.paused_req(&format!("f{i}.bin")), true, None)
                .await
                .unwrap(),
        );
    }
    let mut tasks = tokio::task::JoinSet::new();
    for i in 0..20 {
        let c = c.clone();
        let id = ids[i % ids.len()];
        tasks.spawn(async move {
            if i % 2 == 0 {
                let list = c.list().await.unwrap();
                assert_eq!(list.len(), 10);
            } else {
                match c.call(Request::Get { id }).await.unwrap() {
                    ReplyData::Download { download, .. } => assert_eq!(download.id, id),
                    r => panic!("unexpected reply {r:?}"),
                }
            }
        });
    }
    // 20 parallel `List` calls too.
    for _ in 0..20 {
        let c = c.clone();
        tasks.spawn(async move {
            assert_eq!(c.list().await.unwrap().len(), 10);
        });
    }
    while let Some(r) = tasks.join_next().await {
        r.unwrap();
    }
    drop(c);
    env.shutdown(d).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn daemon_error_mapped() {
    let env = Env::new();
    let d = env.start().await;
    let c = env.client().await;
    match c.call(Request::Get { id: 999 }).await {
        Err(ClientError::Daemon { code, message }) => {
            assert_eq!(code, ErrorCode::NotFound);
            assert!(!message.is_empty());
        }
        r => panic!("expected a not_found error, got {r:?}"),
    }
    // The connection is still usable afterwards.
    assert!(c.list().await.unwrap().is_empty());
    drop(c);
    env.shutdown(d).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn events_after_subscribe() {
    let env = Env::new();
    let s = TestServer::start(ServerCfg {
        len: 64 * KIB,
        ..ServerCfg::default()
    })
    .await;
    let d = env.start().await;
    let mut c = env.client().await;
    c.subscribe(100).await.unwrap();
    let id = c.add(env.req(&s, "a.bin"), false, None).await.unwrap();
    let (mut added, mut snapshot, mut completed) = (false, false, false);
    while !(added && snapshot && completed) {
        match next_event(&mut c).await.expect("disconnected") {
            Event::Added { id: i } if i == id => added = true,
            Event::Snapshot { .. } => snapshot = true,
            Event::Completed { id: i, .. } if i == id => completed = true,
            Event::Failed { reason, .. } => panic!("download failed: {reason}"),
            _ => {}
        }
    }
    assert_eq!(c.dropped_events(), 0);
    drop(c);
    env.shutdown(d).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn disconnect_detected() {
    let env = Env::new();
    let d = env.start().await;
    let mut c = env.client().await;
    c.subscribe(100).await.unwrap();
    env.shutdown(d).await;
    // Buffered events may still come; then the stream ends.
    while next_event(&mut c).await.is_some() {}
    assert!(!c.is_connected());
    assert!(matches!(c.list().await, Err(ClientError::Disconnected)));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn connect_fails_without_daemon() {
    let env = Env::new();
    let e = Client::connect(&env.socket, "test").await.unwrap_err();
    assert!(matches!(e, ClientError::Io(_)), "{e:?}");
}

/// A fake daemon: answers `hello`, then runs `then` on the connection.
async fn fake_server<F, Fut>(name: &SocketName, then: F) -> JoinHandle<()>
where
    F: FnOnce(Conn) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = ()> + Send,
{
    let sock = match name {
        SocketName::Path(p) => p.as_path().to_fs_name::<GenericFilePath>().unwrap(),
        SocketName::Pipe(n) => n.as_str().to_ns_name::<GenericNamespaced>().unwrap(),
    };
    let listener = ListenerOptions::new().name(sock).create_tokio().unwrap();
    tokio::spawn(async move {
        let stream = listener.accept().await.unwrap();
        let (rd, wr) = tokio::io::split(stream);
        let mut conn = Conn {
            rd: MsgReader::new(BufReader::new(rd)),
            wr,
        };
        let hello = conn.recv().await.expect("hello");
        assert!(matches!(hello.body, Request::Hello { .. }), "{hello:?}");
        conn.send(&ServerMsg::Response {
            req_id: hello.req_id,
            result: Reply::Ok {
                data: ReplyData::Hello {
                    daemon_version: "fake".into(),
                    protocol: PROTOCOL_VERSION,
                },
            },
        })
        .await;
        then(conn).await;
        drop(listener);
    })
}

struct Conn {
    rd: MsgReader<BufReader<tokio::io::ReadHalf<Stream>>>,
    wr: tokio::io::WriteHalf<Stream>,
}

impl Conn {
    async fn recv(&mut self) -> Option<Envelope<Request>> {
        self.rd.next().await.unwrap()
    }

    async fn send(&mut self, m: &ServerMsg) {
        write_msg(&mut self.wr, m).await.unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pending_call_fails_on_disconnect() {
    let env = Env::new();
    let server = fake_server(&env.socket, |mut conn| async move {
        // Read the request, then hang up without replying.
        conn.recv().await.expect("request");
    })
    .await;
    let mut c = env.client().await;
    let r = tokio::time::timeout(TIMEOUT, c.list()).await.unwrap();
    assert!(matches!(r, Err(ClientError::Disconnected)), "{r:?}");
    assert!(next_event(&mut c).await.is_none());
    assert!(!c.is_connected());
    server.await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn event_overflow_never_blocks_replies() {
    let env = Env::new();
    let server = fake_server(&env.socket, |mut conn| async move {
        let req = conn.recv().await.expect("request");
        for id in 0..1000 {
            conn.send(&ServerMsg::Event {
                event: Event::Added { id },
            })
            .await;
        }
        conn.send(&ServerMsg::Response {
            req_id: req.req_id,
            result: Reply::Ok {
                data: ReplyData::Downloads {
                    downloads: Vec::new(),
                },
            },
        })
        .await;
        // Stay connected until the client hangs up.
        assert!(conn.recv().await.is_none());
    })
    .await;
    let mut c = env.client().await;
    // Nobody reads events while 1000 arrive: the reply still gets through.
    let list = tokio::time::timeout(TIMEOUT, c.list()).await.unwrap();
    assert!(list.unwrap().is_empty());
    assert!(c.dropped_events() > 0);
    // The oldest events are the ones kept (a full queue drops new ones).
    assert_eq!(next_event(&mut c).await, Some(Event::Added { id: 0 }));
    assert!(c.is_connected());
    drop(c);
    server.await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn undecodable_reply_fails_call_promptly() {
    let env = Env::new();
    let server = fake_server(&env.socket, |mut conn| async move {
        let req = conn.recv().await.expect("request");
        // A known kind whose body this client cannot decode (e.g. a newer daemon).
        let line = format!(
            "{{\"type\":\"response\",\"req_id\":{},\"status\":\"ok\",\"data\":{{\"kind\":\"added\",\"id\":\"x\"}}}}\n",
            req.req_id
        );
        tokio::io::AsyncWriteExt::write_all(&mut conn.wr, line.as_bytes())
            .await
            .unwrap();
        // The connection stays usable: answer the next request properly.
        let req = conn.recv().await.expect("second request");
        conn.send(&ServerMsg::Response {
            req_id: req.req_id,
            result: Reply::Ok {
                data: ReplyData::Downloads {
                    downloads: Vec::new(),
                },
            },
        })
        .await;
        assert!(conn.recv().await.is_none());
    })
    .await;
    let c = env.client().await;
    let t = Instant::now();
    let r = tokio::time::timeout(TIMEOUT, c.list()).await.unwrap();
    assert!(matches!(r, Err(ClientError::Protocol(_))), "{r:?}");
    assert!(t.elapsed() < Duration::from_secs(5), "{:?}", t.elapsed());
    assert!(c.is_connected());
    assert!(c.list().await.unwrap().is_empty());
    drop(c);
    server.await.unwrap();
}

/// Unix only: on Windows the named pipe (mio) copies every write into its own buffer
/// and completes it at once, so a write cannot be abandoned half-way there.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn torn_write_closes_connection() {
    let env = Env::new();
    let (gave_up_tx, gave_up_rx) = tokio::sync::oneshot::channel::<()>();
    let server = fake_server(&env.socket, |mut conn| async move {
        // Read nothing until the client abandoned its write half-way.
        gave_up_rx.await.unwrap();
        // The connection must end (EOF mid-line) although the client is alive.
        let r = tokio::time::timeout(TIMEOUT, conn.rd.next::<Envelope<Request>>()).await;
        let r = r.expect("the client never closed the connection");
        assert!(!matches!(r, Ok(Some(_))), "{r:?}");
    })
    .await;
    let c = env.client().await;
    // Far more than the socket buffers hold, so the write blocks half-way.
    let mut req = env.paused_req("x");
    req.filename = Some("a".repeat(900 * 1024));
    let r = tokio::time::timeout(Duration::from_millis(300), c.add(req, true, None)).await;
    assert!(r.is_err(), "the write should not have completed: {r:?}");
    assert!(!c.is_connected());
    assert!(matches!(c.list().await, Err(ClientError::Disconnected)));
    gave_up_tx.send(()).unwrap();
    server.await.unwrap();
    drop(c);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn take_events_moves_the_stream() {
    let env = Env::new();
    let d = env.start().await;
    let mut c = env.client().await;
    let mut events = c.take_events().expect("events");
    assert!(c.take_events().is_none());
    assert!(c.next_event().await.is_none());
    let c = Arc::new(c);
    c.subscribe(100).await.unwrap();
    let reader = tokio::spawn(async move {
        loop {
            match tokio::time::timeout(TIMEOUT, events.recv()).await {
                Ok(Some(Event::Added { id })) => return id,
                Ok(Some(_)) => {}
                r => panic!("no added event: {r:?}"),
            }
        }
    });
    let id = c.add(env.paused_req("a.bin"), true, None).await.unwrap();
    assert_eq!(reader.await.unwrap(), id);
    drop(c);
    env.shutdown(d).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn protocol_mismatch_reported() {
    let env = Env::new();
    let sock = match &env.socket {
        SocketName::Path(p) => p.as_path().to_fs_name::<GenericFilePath>().unwrap(),
        SocketName::Pipe(n) => n.as_str().to_ns_name::<GenericNamespaced>().unwrap(),
    };
    let listener = ListenerOptions::new().name(sock).create_tokio().unwrap();
    let server = tokio::spawn(async move {
        let stream = listener.accept().await.unwrap();
        let (rd, mut wr) = tokio::io::split(stream);
        let mut rd = MsgReader::new(BufReader::new(rd));
        let hello: Envelope<Request> = rd.next().await.unwrap().unwrap();
        let reply = ServerMsg::Response {
            req_id: hello.req_id,
            result: Reply::Err {
                code: ErrorCode::ProtocolMismatch,
                message: "nope".into(),
            },
        };
        write_msg(&mut wr, &reply).await.unwrap();
    });
    match Client::connect(&env.socket, "test").await {
        Err(ClientError::Daemon { code, .. }) => assert_eq!(code, ErrorCode::ProtocolMismatch),
        r => panic!("expected protocol mismatch, got {:?}", r.map(|_| ())),
    }
    server.await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn daemon_version_from_hello() {
    let env = Env::new();
    let d = env.start().await;
    let c = env.client().await;
    assert_eq!(c.daemon_version(), env!("CARGO_PKG_VERSION"));
    drop(c);
    env.shutdown(d).await;
}

/// A daemon speaking another protocol: `shutdown_incompatible` answers its mismatch
/// with a `shutdown` on the same connection.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shutdown_incompatible_stops_a_mismatched_daemon() {
    let env = Env::new();
    let sock = match &env.socket {
        SocketName::Path(p) => p.as_path().to_fs_name::<GenericFilePath>().unwrap(),
        SocketName::Pipe(n) => n.as_str().to_ns_name::<GenericNamespaced>().unwrap(),
    };
    let listener = ListenerOptions::new().name(sock).create_tokio().unwrap();
    let server = tokio::spawn(async move {
        let stream = listener.accept().await.unwrap();
        let (rd, mut wr) = tokio::io::split(stream);
        let mut rd = MsgReader::new(BufReader::new(rd));
        let hello: serde_json::Value = rd.next().await.unwrap().unwrap();
        assert_eq!(hello["method"], "hello");
        assert_eq!(hello["protocol"], PROTOCOL_VERSION);
        // An older daemon's reply, with a field this client does not know.
        let reply = serde_json::json!({"type": "response", "req_id": hello["req_id"],
            "status": "err", "code": "protocol_mismatch", "message": "old", "extra": 1});
        write_msg(&mut wr, &reply).await.unwrap();
        let shutdown: serde_json::Value = rd.next().await.unwrap().unwrap();
        assert_eq!(shutdown["method"], "shutdown");
        let done = serde_json::json!({"type": "response", "req_id": shutdown["req_id"],
            "status": "ok", "data": {"kind": "done"}});
        write_msg(&mut wr, &done).await.unwrap();
    });
    tokio::time::timeout(TIMEOUT, Client::shutdown_incompatible(&env.paths()))
        .await
        .unwrap()
        .unwrap();
    server.await.unwrap();
}

/// A compatible daemon is left running.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shutdown_incompatible_leaves_a_compatible_daemon() {
    let env = Env::new();
    let d = env.start().await;
    Client::shutdown_incompatible(&env.paths()).await.unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(!d.is_finished());
    let c = env.client().await;
    assert!(c.list().await.unwrap().is_empty());
    drop(c);
    env.shutdown(d).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn connect_or_spawn_uses_running_daemon() {
    let env = Env::new();
    let d = env.start().await;
    // The executable does not exist: connecting must not need it.
    let missing = env.home.path().join("no-such-puppy");
    let c = Client::connect_or_spawn(&env.paths(), "test", &missing, &[])
        .await
        .unwrap();
    assert!(c.list().await.unwrap().is_empty());
    drop(c);
    env.shutdown(d).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn connect_or_spawn_reports_spawn_failure() {
    let env = Env::new();
    let missing = env.home.path().join("no-such-puppy");
    let t = Instant::now();
    let e = Client::connect_or_spawn(&env.paths(), "test", &missing, &[])
        .await
        .unwrap_err();
    assert!(matches!(e, ClientError::Spawn(_)), "{e:?}");
    assert!(t.elapsed() < Duration::from_secs(2), "{:?}", t.elapsed());
}

/// Writes an executable stub that appends its arguments (and pid + session id on
/// Linux) to `<dir>/spawned.log` and then creates `<dir>/spawned`.
#[cfg(unix)]
fn stub_exe(dir: &std::path::Path) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let exe = dir.join("stub-puppy");
    let script = r#"#!/bin/sh
dir=$(dirname "$0")
sid=""
if [ -r /proc/$$/stat ]; then sid=$(cut -d' ' -f6 /proc/$$/stat); fi
echo "$* pid=$$ sid=$sid home=$PUPPY_HOME extra=$PUPPY_EXTRA cwd=$(pwd -P)" >> "$dir/spawned.log"
touch "$dir/spawned"
"#;
    std::fs::write(&exe, script).unwrap();
    std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
    exe
}

#[cfg(unix)]
fn expected_idle_exit() -> String {
    std::env::var("PUPPY_IDLE_EXIT")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(30)
        .to_string()
}

#[cfg(unix)]
async fn wait_for(path: &std::path::Path) {
    let deadline = Instant::now() + TIMEOUT;
    while !path.exists() {
        assert!(
            Instant::now() < deadline,
            "{} never appeared",
            path.display()
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn spawn_daemon_detached_runs_exe_in_new_session() {
    let dir = tempfile::tempdir().unwrap();
    let exe = stub_exe(dir.path());
    puppy_client::spawn_daemon_detached(&exe, None, &[("PUPPY_EXTRA", "yes")]).unwrap();
    wait_for(&dir.path().join("spawned")).await;
    let log = std::fs::read_to_string(dir.path().join("spawned.log")).unwrap();
    let line = log.lines().next().unwrap();
    let want = format!("daemon --idle-exit {} pid=", expected_idle_exit());
    assert!(line.starts_with(&want), "{line}");
    assert!(line.contains("extra=yes"), "{line}");
    // Never the caller's working directory: `/` when no data dir is given.
    assert!(line.ends_with(" cwd=/"), "{line}");
    if cfg!(target_os = "linux") {
        // setsid: the stub leads its own session.
        let field = |k: &str| {
            line.split_whitespace()
                .find_map(|w| w.strip_prefix(k))
                .unwrap()
                .to_string()
        };
        assert_eq!(field("pid="), field("sid="), "{line}");
    }
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn connect_or_spawn_spawns_then_connects() {
    let env = Env::new();
    let dir = tempfile::tempdir().unwrap();
    let exe = stub_exe(dir.path());
    let connect = tokio::spawn({
        let paths = env.paths();
        async move { Client::connect_or_spawn(&paths, "test", &exe, &[("PUPPY_EXTRA", "on")]).await }
    });
    // The stub ran: play the daemon it would have started.
    wait_for(&dir.path().join("spawned")).await;
    let d = tokio::spawn(run_daemon(env.opts()));
    let c = connect.await.unwrap().expect("connected after spawning");
    let log = std::fs::read_to_string(dir.path().join("spawned.log")).unwrap();
    let line = log.lines().next().unwrap();
    assert!(line.contains("extra=on"), "{line}");
    // The daemon runs in its data dir, not in the caller's working directory.
    let data_dir = std::fs::canonicalize(&env.paths().data_dir).unwrap();
    assert!(
        line.ends_with(&format!(" cwd={}", data_dir.display())),
        "{line}"
    );
    assert!(c.list().await.unwrap().is_empty());
    drop(c);
    env.shutdown(d).await;
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn connect_or_spawn_gives_up_after_budget() {
    let env = Env::new();
    let dir = tempfile::tempdir().unwrap();
    let exe = stub_exe(dir.path());
    let t = Instant::now();
    let paths = env.paths();
    let e = Client::connect_or_spawn(&paths, "test", &exe, &[])
        .await
        .unwrap_err();
    let took = t.elapsed();
    assert!(matches!(e, ClientError::NoDaemon { .. }), "{e:?}");
    // Points the user at the daemon's log.
    let log = paths.log_dir.join("puppyd.log");
    assert!(e.to_string().contains(&log.display().to_string()), "{e}");
    assert!(
        took >= Duration::from_millis(4900) && took < Duration::from_secs(7),
        "{took:?}"
    );
    let log = std::fs::read_to_string(dir.path().join("spawned.log")).unwrap();
    let n = log.lines().count();
    // At most one spawn per second.
    assert!((1..=6).contains(&n), "spawned {n} times:\n{log}");
}
