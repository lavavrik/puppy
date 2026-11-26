//! Local-socket server: handshake, dispatch, subscriptions, back-pressure.

use interprocess::local_socket::tokio::{Stream, prelude::*};
use interprocess::local_socket::{GenericFilePath, GenericNamespaced};
use puppy_core::config::HttpConfig;
use puppy_core::engine::Engine;
use puppy_core::http::engine::HttpEngine;
use puppy_core::types::{AddRequest, DownloadId, RequestContext, Source};
use puppy_daemon::server::{InFlightStats, QUEUE_CAP, Server};
use puppy_daemon::{Config, Manager, SocketName, Store};
use puppy_proto::{
    Envelope, ErrorCode, Event, MsgReader, PROTOCOL_VERSION, Reply, ReplyData, Request, ServerMsg,
    UpdateStatus, write_msg,
};
use puppy_testserver::{ServerCfg, TestServer};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};
use tokio::io::{AsyncWriteExt, BufReader, ReadHalf, WriteHalf};
use tokio_util::sync::CancellationToken;

const TIMEOUT: Duration = Duration::from_secs(20);

/// A short, unique socket name (macOS `sun_path` is ~104 bytes).
fn socket_name() -> SocketName {
    static N: AtomicUsize = AtomicUsize::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    if cfg!(windows) {
        SocketName::Pipe(format!("puppy-test-{pid}-{n}"))
    } else {
        let tmp = std::env::temp_dir();
        let base = if tmp.as_os_str().len() <= 40 {
            tmp
        } else {
            PathBuf::from("/tmp")
        };
        SocketName::Path(base.join(format!("pz-{pid}-{n}.sock")))
    }
}

fn http_cfg(dir: &Path) -> HttpConfig {
    HttpConfig {
        backoff_base_ms: 10,
        backoff_max_ms: 100,
        flush_interval_ms: 100,
        read_timeout_ms: 2_000,
        validate_timeout_ms: 1_500,
        default_dir: dir.to_path_buf(),
        ..HttpConfig::default()
    }
}

struct Harness {
    dir: tempfile::TempDir,
    name: SocketName,
    manager: Arc<Manager>,
    clients: Arc<AtomicUsize>,
    in_flight: Arc<InFlightStats>,
    shutdown: CancellationToken,
    stop: CancellationToken,
    task: Option<tokio::task::JoinHandle<()>>,
}

impl Harness {
    async fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let dl = dir.path().join("dl");
        let engine: Arc<dyn Engine> = Arc::new(HttpEngine::new(http_cfg(&dl)).unwrap());
        let cfg = Config {
            download_dir: dl,
            ..Config::default()
        };
        let manager = Manager::start(vec![engine], Store::new(dir.path().join("state.json")), cfg)
            .await
            .unwrap();
        let name = socket_name();
        let server = Server::bind(&name, manager.clone(), None).await.unwrap();
        #[cfg(unix)]
        if let SocketName::Path(p) = &name {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(p).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "socket file mode");
        }
        let clients = server.client_count();
        let in_flight = server.in_flight_stats();
        let shutdown = server.shutdown_requested();
        let stop = CancellationToken::new();
        let task = Some(tokio::spawn(server.run(stop.clone())));
        Harness {
            dir,
            name,
            manager,
            clients,
            in_flight,
            shutdown,
            stop,
            task,
        }
    }

    fn req(&self, s: &TestServer, name: &str) -> AddRequest {
        AddRequest {
            source: Source::Http {
                url: s.url("/file.bin"),
            },
            output_dir: Some(self.dir.path().join("dl")),
            filename: Some(name.into()),
            context: RequestContext::default(),
            files: Default::default(),
            seed: None,
            checksum: None,
            mirrors: Vec::new(),
        }
    }

    async fn connect(&self) -> Client {
        Client::connect(&self.name).await
    }

    async fn greeted(&self) -> Client {
        let mut c = self.connect().await;
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

    async fn stop(mut self) {
        self.stop.cancel();
        if let Some(t) = self.task.take() {
            tokio::time::timeout(TIMEOUT, t)
                .await
                .expect("server stops")
                .unwrap();
        }
        #[cfg(unix)]
        if let SocketName::Path(p) = &self.name {
            assert!(!p.exists(), "socket file removed when the server stops");
        }
        self.manager.shutdown().await;
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        self.stop.cancel();
        if let SocketName::Path(p) = &self.name {
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
    async fn connect(name: &SocketName) -> Self {
        let stream = match name {
            SocketName::Path(p) => {
                Stream::connect(p.as_path().to_fs_name::<GenericFilePath>().unwrap()).await
            }
            SocketName::Pipe(n) => {
                Stream::connect(n.as_str().to_ns_name::<GenericNamespaced>().unwrap()).await
            }
        }
        .expect("connect");
        let (rd, wr) = tokio::io::split(stream);
        Client {
            rd: MsgReader::new(BufReader::new(rd)),
            wr,
            next_id: 1,
        }
    }

    async fn send(&mut self, body: Request) -> u64 {
        let req_id = self.next_id;
        self.next_id += 1;
        write_msg(&mut self.wr, &Envelope { req_id, body })
            .await
            .unwrap();
        req_id
    }

    async fn send_raw(&mut self, v: serde_json::Value) {
        write_msg(&mut self.wr, &v).await.unwrap();
    }

    /// Next message, `None` on EOF/connection error; panics on timeout.
    async fn recv(&mut self) -> Option<ServerMsg> {
        tokio::time::timeout(TIMEOUT, self.rd.next::<ServerMsg>())
            .await
            .expect("timed out waiting for a server message")
            .ok()
            .flatten()
    }

    /// Next response for `req_id`, skipping events.
    async fn response(&mut self, req_id: u64) -> Reply {
        loop {
            match self.recv().await {
                Some(ServerMsg::Response { req_id: r, result }) => {
                    assert_eq!(r, req_id, "unexpected response");
                    return result;
                }
                Some(ServerMsg::Event { .. }) => {}
                None => panic!("connection closed waiting for response {req_id}"),
            }
        }
    }

    async fn request(&mut self, body: Request) -> Reply {
        let id = self.send(body).await;
        self.response(id).await
    }

    /// Asserts the server closes the connection (EOF), skipping messages.
    async fn expect_closed(&mut self) {
        while self.recv().await.is_some() {}
    }
}

fn done() -> Reply {
    Reply::Ok {
        data: ReplyData::Done,
    }
}

async fn wait_for(what: &str, mut pred: impl FnMut() -> bool) {
    let deadline = Instant::now() + TIMEOUT;
    while !pred() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn hello_handshake() {
    let h = Harness::new().await;
    let mut c = h.connect().await;
    let id = c
        .send(Request::Hello {
            client: "test".into(),
            protocol: PROTOCOL_VERSION,
        })
        .await;
    let r = c.response(id).await;
    let Reply::Ok {
        data: ReplyData::Hello {
            daemon_version,
            protocol,
        },
    } = r
    else {
        panic!("{r:?}")
    };
    assert_eq!(protocol, PROTOCOL_VERSION);
    assert_eq!(daemon_version, env!("CARGO_PKG_VERSION"));
    wait_for("client counted", || h.clients.load(Ordering::SeqCst) == 1).await;
    let mut c2 = h.greeted().await;
    wait_for("two clients", || h.clients.load(Ordering::SeqCst) == 2).await;
    // A repeated hello is rejected but the connection stays usable.
    let r = c2
        .request(Request::Hello {
            client: "test".into(),
            protocol: PROTOCOL_VERSION,
        })
        .await;
    assert!(
        matches!(
            r,
            Reply::Err {
                code: ErrorCode::BadRequest,
                ..
            }
        ),
        "{r:?}"
    );
    assert!(matches!(c2.request(Request::List).await, Reply::Ok { .. }));
    drop(c);
    drop(c2);
    wait_for("clients gone", || h.clients.load(Ordering::SeqCst) == 0).await;
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn protocol_mismatch_closes() {
    let h = Harness::new().await;
    let mut c = h.connect().await;
    let r = c
        .request(Request::Hello {
            client: "future".into(),
            protocol: PROTOCOL_VERSION + 1,
        })
        .await;
    assert!(
        matches!(
            r,
            Reply::Err {
                code: ErrorCode::ProtocolMismatch,
                ..
            }
        ),
        "{r:?}"
    );
    // Anything but a shutdown closes the connection at once.
    let t = Instant::now();
    c.send(Request::List).await;
    c.expect_closed().await;
    assert!(t.elapsed() < Duration::from_secs(4), "{:?}", t.elapsed());
    wait_for("client released", || h.clients.load(Ordering::SeqCst) == 0).await;
    assert!(!h.shutdown.is_cancelled());
    h.stop().await;
}

/// A newer client (another protocol version) can still stop this daemon: one
/// `shutdown` is accepted right after the mismatch, whatever its envelope looks like.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shutdown_accepted_after_protocol_mismatch() {
    let h = Harness::new().await;
    let mut c = h.connect().await;
    c.send_raw(
        serde_json::json!({"req_id": 1, "method": "hello", "client": "future",
        "protocol": PROTOCOL_VERSION + 1, "new_field": true}),
    )
    .await;
    assert!(matches!(
        c.response(1).await,
        Reply::Err {
            code: ErrorCode::ProtocolMismatch,
            ..
        }
    ));
    c.send_raw(serde_json::json!({"req_id": 77, "method": "shutdown", "force": true}))
        .await;
    assert_eq!(c.response(77).await, done());
    tokio::time::timeout(TIMEOUT, h.shutdown.cancelled())
        .await
        .expect("shutdown requested");
    c.expect_closed().await;
    h.stop().await;
}

/// The mismatch window ends: a silent client is disconnected after it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn mismatch_window_expires() {
    let h = Harness::new().await;
    let mut c = h.connect().await;
    let r = c
        .request(Request::Hello {
            client: "future".into(),
            protocol: PROTOCOL_VERSION + 1,
        })
        .await;
    assert!(matches!(r, Reply::Err { .. }), "{r:?}");
    let t = Instant::now();
    c.expect_closed().await;
    let waited = t.elapsed();
    assert!(
        waited >= puppy_daemon::server::MISMATCH_SHUTDOWN_WINDOW - Duration::from_millis(500),
        "{waited:?}"
    );
    assert!(!h.shutdown.is_cancelled());
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn request_before_hello_rejected() {
    let h = Harness::new().await;
    let mut c = h.connect().await;
    let r = c.request(Request::List).await;
    assert!(
        matches!(
            r,
            Reply::Err {
                code: ErrorCode::BadRequest,
                ..
            }
        ),
        "{r:?}"
    );
    c.expect_closed().await;
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn add_and_list_over_socket() {
    let h = Harness::new().await;
    let s = TestServer::start(ServerCfg {
        len: 64 * 1024,
        ..ServerCfg::default()
    })
    .await;
    let mut c = h.greeted().await;
    let r = c
        .request(Request::Add {
            request: h.req(&s, "a.bin"),
            start_paused: true,
            limit: None,
        })
        .await;
    let Reply::Ok {
        data: ReplyData::Added { id },
    } = r
    else {
        panic!("{r:?}")
    };
    let r = c.request(Request::List).await;
    let Reply::Ok {
        data: ReplyData::Downloads { downloads },
    } = r
    else {
        panic!("{r:?}")
    };
    assert_eq!(downloads.len(), 1);
    assert_eq!(downloads[0].id, id);
    assert_eq!(downloads[0].name, "a.bin");
    let r = c.request(Request::Get { id }).await;
    assert!(
        matches!(&r, Reply::Ok { data: ReplyData::Download { download, .. } } if download.id == id),
        "{r:?}"
    );
    assert_eq!(c.request(Request::Pause { id }).await, done());
    let r = c.request(Request::GetSettings).await;
    assert!(
        matches!(
            r,
            Reply::Ok {
                data: ReplyData::Settings { .. }
            }
        ),
        "{r:?}"
    );
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn errors_have_codes() {
    let h = Harness::new().await;
    let mut c = h.greeted().await;
    let r = c.request(Request::Get { id: 999 }).await;
    assert!(
        matches!(
            r,
            Reply::Err {
                code: ErrorCode::NotFound,
                ..
            }
        ),
        "{r:?}"
    );
    // Unknown method: bad_request with the caller's req_id; the connection survives.
    c.send_raw(serde_json::json!({"req_id": 77, "method": "teleport"}))
        .await;
    let r = c.response(77).await;
    assert!(
        matches!(
            r,
            Reply::Err {
                code: ErrorCode::BadRequest,
                ..
            }
        ),
        "{r:?}"
    );
    // Invalid JSON: bad_request with req_id 0.
    c.wr.write_all(b"not json\n").await.unwrap();
    let r = c.response(0).await;
    assert!(
        matches!(
            r,
            Reply::Err {
                code: ErrorCode::BadRequest,
                ..
            }
        ),
        "{r:?}"
    );
    assert!(matches!(c.request(Request::List).await, Reply::Ok { .. }));
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn subscribe_receives_snapshots() {
    let h = Harness::new().await;
    let s = TestServer::start(ServerCfg {
        len: 64 * 1024,
        ..ServerCfg::default()
    })
    .await;
    let mut c = h.greeted().await;
    assert_eq!(
        c.request(Request::Subscribe { interval_ms: 250 }).await,
        done()
    );
    // Right after the reply: the update status (updates are off in this server).
    assert_eq!(
        c.recv().await,
        Some(ServerMsg::Event {
            event: Event::Update {
                status: UpdateStatus::NotChecked
            }
        })
    );
    let start = Instant::now();
    let mut snapshots = 0;
    while start.elapsed() < Duration::from_secs(1) {
        let left = Duration::from_secs(1).saturating_sub(start.elapsed());
        match tokio::time::timeout(left, c.rd.next::<ServerMsg>()).await {
            Ok(Ok(Some(ServerMsg::Event {
                event: Event::Snapshot { .. },
            }))) => snapshots += 1,
            Ok(other) => panic!("unexpected {other:?}"),
            Err(_) => break,
        }
    }
    assert!(snapshots >= 3, "only {snapshots} snapshots in 1 s");

    let add = c
        .send(Request::Add {
            request: h.req(&s, "b.bin"),
            start_paused: false,
            limit: None,
        })
        .await;
    let mut id: Option<DownloadId> = None;
    let mut added_event = false;
    loop {
        match c.recv().await.expect("connection open") {
            ServerMsg::Response { req_id, result } => {
                assert_eq!(req_id, add);
                let Reply::Ok {
                    data: ReplyData::Added { id: got },
                } = result
                else {
                    panic!("{result:?}")
                };
                id = Some(got);
            }
            ServerMsg::Event {
                event: Event::Added { .. },
            } => added_event = true,
            ServerMsg::Event {
                event: Event::Completed { id: done_id, name },
            } => {
                assert_eq!(Some(done_id), id);
                assert_eq!(name, "b.bin");
                break;
            }
            ServerMsg::Event { .. } => {}
        }
    }
    assert!(added_event, "Added event forwarded");

    // Unsubscribe: no snapshots after the reply.
    assert_eq!(c.request(Request::Unsubscribe).await, done());
    let quiet = tokio::time::timeout(Duration::from_millis(600), c.rd.next::<ServerMsg>()).await;
    assert!(quiet.is_err(), "message after unsubscribe: {quiet:?}");
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn slow_subscriber_does_not_block() {
    let h = Harness::new().await;
    let s = TestServer::start(ServerCfg::default()).await;
    let mut b = h.greeted().await;
    // Many downloads make each snapshot large, so the slow client's socket buffer
    // fills up within the test and its writer really blocks.
    for i in 0..200 {
        let r = b
            .request(Request::Add {
                request: h.req(&s, &format!("download-with-a-long-name-{i:04}.bin")),
                start_paused: true,
                limit: None,
            })
            .await;
        assert!(matches!(r, Reply::Ok { .. }), "{r:?}");
    }
    let mut a = h.greeted().await;
    assert_eq!(
        a.request(Request::Subscribe { interval_ms: 100 }).await,
        done()
    );
    // A never reads again. Keep generating discrete events (for A's queue) while B
    // measures request latency.
    let start = Instant::now();
    let mut worst = Duration::ZERO;
    let mut n = 0;
    while start.elapsed() < Duration::from_secs(3) {
        let t = Instant::now();
        let r = b.request(Request::List).await;
        let lat = t.elapsed();
        assert!(matches!(r, Reply::Ok { .. }), "{r:?}");
        worst = worst.max(lat);
        assert!(lat < Duration::from_millis(200), "List took {lat:?}");
        let r = b
            .request(Request::Add {
                request: h.req(&s, &format!("extra-{n}.bin")),
                start_paused: true,
                limit: None,
            })
            .await;
        assert!(matches!(r, Reply::Ok { .. }), "{r:?}");
        n += 1;
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(n > 10, "only {n} rounds (worst List latency {worst:?})");
    drop(a);
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn oversized_line_closes_connection() {
    let h = Harness::new().await;
    let mut c = h.greeted().await;
    let mut wr = c.wr;
    let writer = tokio::spawn(async move {
        let chunk = vec![b'a'; 64 * 1024];
        for _ in 0..20 {
            if wr.write_all(&chunk).await.is_err() {
                return;
            }
        }
        let _ = wr.write_all(b"\n").await;
        // Keep the write half open until the server closes the connection.
        tokio::time::sleep(TIMEOUT).await;
    });
    let mut saw_error = false;
    loop {
        match tokio::time::timeout(TIMEOUT, c.rd.next::<ServerMsg>())
            .await
            .expect("server closes the connection")
        {
            Ok(Some(ServerMsg::Response {
                req_id: 0,
                result:
                    Reply::Err {
                        code: ErrorCode::BadRequest,
                        ..
                    },
            })) => saw_error = true,
            Ok(Some(other)) => panic!("unexpected {other:?}"),
            Ok(None) | Err(_) => break,
        }
    }
    assert!(saw_error, "error reply before close");
    writer.abort();
    wait_for("client released", || h.clients.load(Ordering::SeqCst) == 0).await;
    // The server still serves others.
    let mut c2 = h.greeted().await;
    assert!(matches!(c2.request(Request::List).await, Reply::Ok { .. }));
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shutdown_request_cancels_token() {
    let h = Harness::new().await;
    let mut c = h.greeted().await;
    assert!(!h.shutdown.is_cancelled());
    assert_eq!(c.request(Request::Shutdown).await, done());
    tokio::time::timeout(TIMEOUT, h.shutdown.cancelled())
        .await
        .expect("shutdown requested");
    h.stop().await;
    // Stopping the server closes remaining connections.
    c.expect_closed().await;
}

/// The daemon stops the server as soon as a shutdown is requested; the `Done` reply
/// must still reach the client (also when the writer is mid-write at that moment).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shutdown_reply_survives_an_immediate_stop() {
    let mut runs = tokio::task::JoinSet::new();
    for _ in 0..40 {
        runs.spawn(async {
            let h = Harness::new().await;
            let mut c = h.greeted().await;
            let shutdown = h.shutdown.clone();
            let stop = h.stop.clone();
            let stopper = tokio::spawn(async move {
                shutdown.cancelled().await;
                stop.cancel();
            });
            assert_eq!(c.request(Request::Shutdown).await, done());
            stopper.await.unwrap();
            h.stop().await;
        });
    }
    while let Some(r) = runs.join_next().await {
        r.unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bind_fails_if_name_in_use() {
    let h = Harness::new().await;
    assert!(
        Server::bind(&h.name, h.manager.clone(), None)
            .await
            .is_err()
    );
    // The first server is unaffected.
    let mut c = h.greeted().await;
    assert!(matches!(c.request(Request::List).await, Reply::Ok { .. }));
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pipelined_flood_is_bounded() {
    let h = Harness::new().await;
    let s = TestServer::start(ServerCfg::default()).await;
    let mut b = h.greeted().await;
    // Large List replies fill the flooding client's socket buffer quickly.
    for i in 0..200 {
        let r = b
            .request(Request::Add {
                request: h.req(&s, &format!("download-with-a-long-name-{i:04}.bin")),
                start_paused: true,
                limit: None,
            })
            .await;
        assert!(matches!(r, Reply::Ok { .. }), "{r:?}");
    }
    let mut a = h.greeted().await;
    let mut line = serde_json::to_vec(&Envelope {
        req_id: 1,
        body: Request::List,
    })
    .unwrap();
    line.push(b'\n');
    let batch: Vec<u8> = line.repeat(1000);
    // A pipelines requests and never reads; its writes stall once the server
    // stops reading from it.
    let flood = tokio::spawn(async move {
        for _ in 0..20 {
            if a.wr.write_all(&batch).await.is_err() {
                break;
            }
        }
        a
    });
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(3) {
        let t = Instant::now();
        let r = b.request(Request::List).await;
        let lat = t.elapsed();
        assert!(matches!(r, Reply::Ok { .. }), "{r:?}");
        assert!(lat < Duration::from_millis(500), "List took {lat:?}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let peak = h.in_flight.peak();
    assert!(peak <= QUEUE_CAP + 1, "{peak} requests in flight at once");
    // The stuck client is eventually disconnected; B stays connected.
    wait_for("flooding client dropped", || {
        h.clients.load(Ordering::SeqCst) == 1
    })
    .await;
    flood.abort();
    assert!(matches!(b.request(Request::List).await, Reply::Ok { .. }));
    h.stop().await;
}
