//! `puppy native-host`: the native messaging host the browser extension starts. It
//! reads framed JSON requests on stdin, answers each with exactly one framed reply on
//! stdout, and talks to the daemon (starting one if needed) over the local socket.
//!
//! Nothing but framed replies may go to stdout (the browser reads it), and the host
//! logs nothing: stderr ends up in the browser's log, and requests carry cookies.

pub mod framing;
pub mod message;

use crate::cli::{CliError, MISMATCH_HINT, is_mismatch};
use anyhow::Context;
use framing::{FrameError, MAX_OUT, read_frame, write_frame};
use message::{ErrorKind, HOST_PROTOCOL, Incoming, Outgoing, Reply, cut, error_kind};
use puppy_client::{Client, ClientError, Paths};
use puppy_proto::{ErrorCode, Request};
use std::future::Future;
use std::path::PathBuf;
use tokio::io::{AsyncRead, AsyncWrite};

/// How the host gets its daemon connection.
pub trait Connector {
    fn connect(&mut self) -> impl Future<Output = Result<Client, ClientError>> + Send;
}

/// Connects to the daemon of this environment, starting one if none runs.
struct Spawning {
    paths: Paths,
    exe: PathBuf,
}

impl Connector for Spawning {
    fn connect(&mut self) -> impl Future<Output = Result<Client, ClientError>> + Send {
        Client::connect_or_spawn(&self.paths, "puppy-native-host", &self.exe, &[])
    }
}

/// Serves stdin/stdout until the browser closes stdin.
pub async fn run() -> Result<(), CliError> {
    let paths = Paths::resolve().context("cannot set up puppy's directories")?;
    let exe = std::env::current_exe().context("cannot find the puppy executable")?;
    let served = serve(
        tokio::io::stdin(),
        tokio::io::stdout(),
        Spawning { paths, exe },
    )
    .await;
    // A broken stream is not worth a line in the browser's log: exit 1 quietly.
    served.map_err(|_| CliError::Reported(1))
}

/// Answers every request read from `input` with one reply on `output`, until
/// `input` ends (`Ok`). A message over the size limit is answered `bad_message` and
/// ends serving with its error, as does a broken stream.
pub async fn serve<R, W, C>(mut input: R, mut output: W, connector: C) -> Result<(), FrameError>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
    C: Connector,
{
    let mut host = Host {
        connector,
        client: None,
    };
    loop {
        let bytes = match read_frame(&mut input).await {
            Ok(Some(b)) => b,
            Ok(None) => return Ok(()),
            Err(e @ FrameError::TooLarge { .. }) => {
                let reply = error(None, ErrorKind::BadMessage, e.to_string());
                send(&mut output, &reply).await?;
                return Err(e);
            }
            Err(e) => return Err(e),
        };
        let reply = host.handle(&bytes).await;
        send(&mut output, &reply).await?;
    }
}

/// Writes `reply`, or a `bad_message` error in its place if it is too large to send.
async fn send<W: AsyncWrite + Unpin>(output: &mut W, reply: &Reply) -> Result<(), FrameError> {
    let mut json = serde_json::to_vec(reply).map_err(std::io::Error::other)?;
    if json.len() > MAX_OUT {
        let instead = error(
            reply.id.clone(),
            ErrorKind::BadMessage,
            format!("the reply of {} bytes is too large to send", json.len()),
        );
        json = serde_json::to_vec(&instead).map_err(std::io::Error::other)?;
    }
    write_frame(output, &json).await
}

fn error(id: Option<String>, code: ErrorKind, message: String) -> Reply {
    Reply {
        id,
        body: Outgoing::Error {
            code,
            message: cut(message),
        },
    }
}

/// What the user sees for a daemon error.
fn describe(e: &ClientError) -> String {
    if is_mismatch(e) {
        format!("{e}; {MISMATCH_HINT}")
    } else {
        e.to_string()
    }
}

struct Host<C> {
    connector: C,
    /// Reused across messages; dropped after an `unreachable` error so the next
    /// message connects again.
    client: Option<Client>,
}

impl<C: Connector> Host<C> {
    async fn client(&mut self) -> Result<&Client, ClientError> {
        if self.client.as_ref().is_some_and(|c| !c.is_connected()) {
            self.client = None;
        }
        match &mut self.client {
            Some(c) => Ok(c),
            slot @ None => Ok(slot.insert(self.connector.connect().await?)),
        }
    }

    /// The error reply for a daemon error; forgets the connection if it is unusable.
    fn failed(&mut self, id: String, e: &ClientError) -> Reply {
        let kind = error_kind(e);
        if kind == ErrorKind::Unreachable {
            self.client = None;
        }
        error(Some(id), kind, describe(e))
    }

    async fn handle(&mut self, bytes: &[u8]) -> Reply {
        let env = match message::parse(bytes) {
            Ok(env) => env,
            Err((id, why)) => return error(id, ErrorKind::BadMessage, why),
        };
        let id = env.id;
        if !matches!(env.body, Incoming::Hello { .. }) && env.protocol != Some(HOST_PROTOCOL) {
            return error(
                Some(id),
                ErrorKind::ProtocolMismatch,
                format!(
                    "the extension speaks host protocol {}, this puppy speaks {HOST_PROTOCOL}",
                    env.protocol.unwrap_or_default()
                ),
            );
        }
        let body = match env.body {
            Incoming::Hello { .. } => {
                let (daemon_version, daemon_error) = match self.client().await {
                    Ok(c) => (Some(c.daemon_version().to_string()), None),
                    Err(e) => {
                        let message = cut(describe(&e));
                        self.client = None;
                        (None, Some(message))
                    }
                };
                Outgoing::Hello {
                    protocol: HOST_PROTOCOL,
                    puppy_version: env!("CARGO_PKG_VERSION").to_string(),
                    daemon_version,
                    daemon_error,
                }
            }
            Incoming::Add(m) => {
                let req = match message::to_add_request(&m) {
                    Ok(r) => r,
                    Err(why) => return error(Some(id), ErrorKind::Refused, why),
                };
                // Never retried: the daemon may have added it before a reply was lost.
                // Nor timed out: the extension has given the download back to the
                // browser by then, and only a late `accepted` lets it drop one copy;
                // an `unreachable` here would leave the daemon's copy running too.
                let added = match self.client().await {
                    Ok(c) => c.add_unbounded(req).await,
                    Err(e) => Err(e),
                };
                match added {
                    Ok(download_id) => Outgoing::Accepted { download_id },
                    Err(e) => return self.failed(id, &e),
                }
            }
            Incoming::Remove {
                download_id,
                delete_files,
            } => {
                let removed = match self.client().await {
                    Ok(c) => {
                        c.call(Request::Remove {
                            id: download_id,
                            delete_files,
                        })
                        .await
                    }
                    Err(e) => Err(e),
                };
                match removed {
                    Ok(_)
                    | Err(ClientError::Daemon {
                        code: ErrorCode::NotFound,
                        ..
                    }) => Outgoing::Removed,
                    Err(e) => return self.failed(id, &e),
                }
            }
        };
        Reply { id: Some(id), body }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use puppy_core::config::HttpConfig;
    use puppy_daemon::SocketName;
    use puppy_daemon::lifecycle::{DaemonOptions, RunOutcome, run_daemon};
    use serde_json::{Value, json};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{Duration, Instant};
    use tokio::io::AsyncWriteExt;
    use tokio::task::JoinHandle;

    const TIMEOUT: Duration = Duration::from_secs(20);

    /// A short, unique socket name (macOS `sun_path` is ~104 bytes).
    fn socket_name() -> SocketName {
        static N: AtomicUsize = AtomicUsize::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        if cfg!(windows) {
            SocketName::Pipe(format!("puppy-nh-{pid}-{n}"))
        } else {
            SocketName::Path(PathBuf::from(format!("/tmp/pz-{pid}-nh{n}.sock")))
        }
    }

    /// An in-process daemon with its own home.
    struct Daemon {
        home: tempfile::TempDir,
        socket: SocketName,
        task: Option<JoinHandle<anyhow::Result<RunOutcome>>>,
    }

    impl Daemon {
        async fn start() -> Self {
            let home = tempfile::tempdir().unwrap();
            let socket = socket_name();
            let paths = Paths::with_home(home.path(), socket.clone()).unwrap();
            let task = tokio::spawn(run_daemon(DaemonOptions {
                paths: Some(paths),
                http: Some(HttpConfig {
                    backoff_base_ms: 10,
                    backoff_max_ms: 100,
                    flush_interval_ms: 100,
                    read_timeout_ms: 2_000,
                    validate_timeout_ms: 1_500,
                    default_dir: home.path().join("dl"),
                    ..HttpConfig::default()
                }),
                torrent_test: Some(Default::default()),
                ..DaemonOptions::default()
            }));
            let d = Daemon {
                home,
                socket,
                task: Some(task),
            };
            let deadline = Instant::now() + TIMEOUT;
            while Client::connect(&d.socket, "test").await.is_err() {
                assert!(Instant::now() < deadline, "daemon never listened");
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            d
        }

        fn counting(&self) -> Counting {
            Counting {
                socket: self.socket.clone(),
                calls: Arc::default(),
            }
        }

        async fn client(&self) -> Client {
            Client::connect(&self.socket, "test").await.unwrap()
        }

        fn state(&self) -> Value {
            let path = self.home.path().join("data").join("state.json");
            let text = std::fs::read_to_string(&path).unwrap_or_default();
            serde_json::from_str(&text).unwrap_or_default()
        }

        async fn stop(mut self) {
            let _ = self.client().await.call(Request::Shutdown).await;
            let task = self.task.take().unwrap();
            tokio::time::timeout(TIMEOUT, task)
                .await
                .expect("daemon did not stop")
                .unwrap()
                .unwrap();
        }
    }

    impl Drop for Daemon {
        fn drop(&mut self) {
            if let Some(t) = &self.task {
                t.abort();
            }
            if let SocketName::Path(p) = &self.socket {
                let _ = std::fs::remove_file(p);
            }
        }
    }

    #[derive(Clone)]
    struct Counting {
        socket: SocketName,
        calls: Arc<AtomicUsize>,
    }

    impl Connector for Counting {
        fn connect(&mut self) -> impl Future<Output = Result<Client, ClientError>> + Send {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Client::connect(&self.socket, "test")
        }
    }

    struct Failing;

    impl Connector for Failing {
        async fn connect(&mut self) -> Result<Client, ClientError> {
            Err(ClientError::Io(
                std::io::ErrorKind::ConnectionRefused.into(),
            ))
        }
    }

    fn frame(bytes: &[u8]) -> Vec<u8> {
        [&(bytes.len() as u32).to_ne_bytes()[..], bytes].concat()
    }

    fn decode(mut out: &[u8]) -> Vec<Value> {
        let mut msgs = Vec::new();
        while !out.is_empty() {
            let len = u32::from_ne_bytes(out[..4].try_into().unwrap()) as usize;
            msgs.push(serde_json::from_slice(&out[4..4 + len]).unwrap());
            out = &out[4 + len..];
        }
        msgs
    }

    /// Feeds raw message bodies to `serve` and returns its replies.
    async fn exchange_raw(connector: impl Connector, bodies: &[Vec<u8>]) -> Vec<Value> {
        let (mut browser, host_in) = tokio::io::duplex(1 << 16);
        for b in bodies {
            browser.write_all(&frame(b)).await.unwrap();
        }
        drop(browser);
        let mut out = Vec::new();
        tokio::time::timeout(TIMEOUT, serve(host_in, &mut out, connector))
            .await
            .expect("serve hung")
            .unwrap();
        decode(&out)
    }

    async fn exchange(connector: impl Connector, frames: &[Value]) -> Vec<Value> {
        let bodies: Vec<Vec<u8>> = frames
            .iter()
            .map(|v| serde_json::to_vec(v).unwrap())
            .collect();
        exchange_raw(connector, &bodies).await
    }

    fn hello(id: &str) -> Value {
        json!({"id": id, "protocol": 1, "type": "hello", "extension_version": "0.1.0"})
    }

    fn add(id: &str, url: &str) -> Value {
        json!({"id": id, "protocol": 1, "type": "add", "url": url, "cookies": "sid=SECRET"})
    }

    fn remove(id: &str, download_id: u64) -> Value {
        json!({"id": id, "protocol": 1, "type": "remove", "download_id": download_id})
    }

    fn remove_deleting(id: &str, download_id: u64) -> Value {
        json!({"id": id, "protocol": 1, "type": "remove", "download_id": download_id, "delete_files": true})
    }

    const NEVER: &str = "http://127.0.0.1:9/never";

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn hello_reports_versions() {
        let d = Daemon::start().await;
        let r = exchange(d.counting(), &[hello("h1")]).await;
        let v = env!("CARGO_PKG_VERSION");
        assert_eq!(
            r,
            vec![json!({
                "id": "h1", "type": "hello", "protocol": 1, "puppy_version": v,
                "daemon_version": v, "daemon_error": null,
            })]
        );
        d.stop().await;
    }

    #[tokio::test]
    async fn hello_without_daemon_still_answers() {
        let r = exchange(Failing, &[hello("h1")]).await;
        assert_eq!(r.len(), 1);
        assert_eq!(r[0]["id"], "h1");
        assert_eq!(r[0]["type"], "hello");
        assert_eq!(r[0]["daemon_version"], Value::Null);
        let why = r[0]["daemon_error"].as_str().unwrap();
        assert!(why.contains("cannot talk to the daemon"), "{why}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn add_is_accepted_with_an_id() {
        let d = Daemon::start().await;
        let r = exchange(d.counting(), &[add("a1", NEVER)]).await;
        assert_eq!(
            r,
            vec![json!({"id": "a1", "type": "accepted", "download_id": 1})]
        );
        let deadline = Instant::now() + TIMEOUT;
        loop {
            let state = d.state();
            let dl = &state["downloads"][0]["request"];
            if dl["context"]["cookies"] == "sid=SECRET" {
                assert_eq!(dl["output_dir"], Value::Null, "{state}");
                break;
            }
            assert!(Instant::now() < deadline, "never saved: {state}");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        d.stop().await;
    }

    #[tokio::test]
    async fn unreachable_daemon_is_reported() {
        let r = exchange(Failing, &[add("a1", NEVER), hello("h1")]).await;
        assert_eq!(r.len(), 2, "{r:?}");
        assert_eq!(r[0]["id"], "a1");
        assert_eq!(r[0]["type"], "error");
        assert_eq!(r[0]["code"], "unreachable");
        assert_eq!(r[1]["id"], "h1");
        assert_eq!(r[1]["type"], "hello");
    }

    #[tokio::test]
    async fn refused_url_never_reaches_the_daemon() {
        let c = Counting {
            socket: socket_name(),
            calls: Arc::default(),
        };
        let r = exchange(c.clone(), &[add("a1", "ftp://x/f")]).await;
        assert_eq!(r[0]["id"], "a1");
        assert_eq!(r[0]["code"], "refused");
        assert!(r[0]["message"].as_str().unwrap().contains("unsupported"));
        assert_eq!(c.calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn remove_removes_without_deleting() {
        let d = Daemon::start().await;
        let r = exchange(
            d.counting(),
            &[add("a1", NEVER), remove("r1", 1), remove("r2", 999)],
        )
        .await;
        assert_eq!(r[0]["type"], "accepted", "{r:?}");
        assert_eq!(r[1], json!({"id": "r1", "type": "removed"}));
        assert_eq!(r[2], json!({"id": "r2", "type": "removed"}));
        assert!(d.client().await.list().await.unwrap().is_empty());
        d.stop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn remove_deletes_files_only_when_asked() {
        let server = puppy_testserver::TestServer::start(puppy_testserver::ServerCfg {
            len: 64 * 1024,
            ..Default::default()
        })
        .await;
        let d = Daemon::start().await;
        let r = exchange(
            d.counting(),
            &[
                add("a1", server.url("/file.bin?k=1").as_str()),
                add("a2", server.url("/file.bin?k=2").as_str()),
            ],
        )
        .await;
        assert_eq!(r[0]["type"], "accepted", "{r:?}");
        assert_eq!(r[1]["type"], "accepted", "{r:?}");
        let deadline = Instant::now() + TIMEOUT;
        let paths = loop {
            let list = d.client().await.list().await.unwrap();
            if list.len() == 2
                && list
                    .iter()
                    .all(|s| matches!(s.state, puppy_proto::WireState::Completed))
            {
                let path = |id| {
                    let s = list.iter().find(|s| s.id == id).unwrap();
                    s.path.clone().unwrap()
                };
                break (path(1), path(2));
            }
            assert!(Instant::now() < deadline, "never completed: {list:?}");
            tokio::time::sleep(Duration::from_millis(50)).await;
        };
        assert!(paths.0.exists() && paths.1.exists(), "{paths:?}");
        let r = exchange(d.counting(), &[remove("r1", 1), remove_deleting("r2", 2)]).await;
        assert_eq!(r[0], json!({"id": "r1", "type": "removed"}));
        assert_eq!(r[1], json!({"id": "r2", "type": "removed"}));
        let deadline = Instant::now() + TIMEOUT;
        while paths.1.exists() {
            assert!(Instant::now() < deadline, "never deleted: {paths:?}");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(paths.0.exists(), "a plain remove keeps the file");
        d.stop().await;
    }

    #[tokio::test]
    async fn protocol_mismatch_is_refused() {
        let c = Counting {
            socket: socket_name(),
            calls: Arc::default(),
        };
        let mut a = add("a1", NEVER);
        a["protocol"] = json!(2);
        let mut rm = remove("r1", 1);
        rm["protocol"] = json!(2);
        let mut h = hello("h1");
        h["protocol"] = json!(2);
        let r = exchange(c.clone(), &[a, rm]).await;
        assert_eq!(r[0]["id"], "a1");
        assert_eq!(r[0]["code"], "protocol_mismatch");
        assert_eq!(r[1]["code"], "protocol_mismatch");
        assert_eq!(c.calls.load(Ordering::SeqCst), 0);
        let r = exchange(Failing, &[h]).await;
        assert_eq!(r[0]["type"], "hello");
        assert_eq!(r[0]["protocol"], 1);
    }

    #[tokio::test]
    async fn bad_message_is_reported_and_serving_goes_on() {
        let bodies = vec![
            b"not json".to_vec(),
            br#"{"id":"x1","protocol":1,"type":"nope"}"#.to_vec(),
            serde_json::to_vec(&hello("h1")).unwrap(),
        ];
        let r = exchange_raw(Failing, &bodies).await;
        assert_eq!(r.len(), 3, "{r:?}");
        assert_eq!(r[0]["id"], Value::Null);
        assert_eq!(r[0]["code"], "bad_message");
        assert_eq!(r[1]["id"], "x1");
        assert_eq!(r[1]["code"], "bad_message");
        assert_eq!(r[2]["type"], "hello");
    }

    #[tokio::test]
    async fn hello_without_protocol_gets_the_versions() {
        let r = exchange(
            Failing,
            &[json!({"id": "h1", "type": "hello", "extension_version": "9.0.0"})],
        )
        .await;
        assert_eq!(r[0]["id"], "h1");
        assert_eq!(r[0]["type"], "hello");
        assert_eq!(r[0]["protocol"], HOST_PROTOCOL);
        assert_eq!(r[0]["puppy_version"], env!("CARGO_PKG_VERSION"));
    }

    #[tokio::test]
    async fn empty_frame_is_a_bad_message_and_serving_goes_on() {
        let bodies = vec![Vec::new(), serde_json::to_vec(&hello("h1")).unwrap()];
        let r = exchange_raw(Failing, &bodies).await;
        assert_eq!(r.len(), 2, "{r:?}");
        assert_eq!(r[0]["id"], Value::Null);
        assert_eq!(r[0]["code"], "bad_message");
        assert_eq!(r[1]["type"], "hello");
    }

    #[tokio::test]
    async fn overlong_id_is_not_echoed() {
        let mut h = hello("h1");
        h["id"] = json!("i".repeat(message::MAX_ID_BYTES + 1));
        let r = exchange(Failing, &[h]).await;
        assert_eq!(r[0]["id"], Value::Null);
        assert_eq!(r[0]["code"], "bad_message");
    }

    #[tokio::test]
    async fn oversized_message_is_answered_then_ends_serving() {
        let input = ((framing::MAX_IN + 1) as u32).to_ne_bytes();
        let mut out = Vec::new();
        let r = serve(&input[..], &mut out, Failing).await;
        assert!(matches!(r, Err(FrameError::TooLarge { .. })), "{r:?}");
        let replies = decode(&out);
        assert_eq!(replies.len(), 1);
        assert_eq!(replies[0]["id"], Value::Null);
        assert_eq!(replies[0]["code"], "bad_message");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn connects_once_for_many_messages() {
        let d = Daemon::start().await;
        let c = d.counting();
        let r = exchange(c.clone(), &[add("a1", NEVER), add("a2", NEVER)]).await;
        assert_eq!(r[0]["type"], "accepted", "{r:?}");
        assert_eq!(r[1]["type"], "accepted", "{r:?}");
        assert_eq!(c.calls.load(Ordering::SeqCst), 1);
        d.stop().await;
    }

    /// Hands out one already connected client.
    struct Given(Option<Client>);

    impl Connector for Given {
        async fn connect(&mut self) -> Result<Client, ClientError> {
            self.0.take().ok_or(ClientError::Disconnected)
        }
    }

    /// A daemon stub on its own thread (real time): answers `hello`, then holds the
    /// reply to the first `add` until `go` fires, and answers it `Added { id: 7 }`.
    fn stalling_daemon(socket: SocketName, go: std::sync::mpsc::Receiver<()>) {
        use interprocess::local_socket::tokio::prelude::*;
        use interprocess::local_socket::{GenericFilePath, GenericNamespaced, ListenerOptions};
        use puppy_proto::{Envelope, MsgReader, PROTOCOL_VERSION, ReplyData, ServerMsg, write_msg};
        let (ready_tx, ready) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async move {
                let name = match &socket {
                    SocketName::Path(p) => p.as_path().to_fs_name::<GenericFilePath>().unwrap(),
                    SocketName::Pipe(n) => n.as_str().to_ns_name::<GenericNamespaced>().unwrap(),
                };
                let listener = ListenerOptions::new().name(name).create_tokio().unwrap();
                ready_tx.send(()).unwrap();
                let conn = listener.accept().await.unwrap();
                let (rd, mut wr) = tokio::io::split(conn);
                let mut rd = MsgReader::new(tokio::io::BufReader::new(rd));
                let ok = |req_id, data| ServerMsg::Response {
                    req_id,
                    result: puppy_proto::Reply::Ok { data },
                };
                while let Ok(Some(env)) = rd.next::<Envelope<Request>>().await {
                    let data = match env.body {
                        Request::Hello { .. } => ReplyData::Hello {
                            daemon_version: "stub".into(),
                            protocol: PROTOCOL_VERSION,
                        },
                        Request::Add { .. } => {
                            tokio::task::spawn_blocking(move || go.recv().unwrap())
                                .await
                                .unwrap();
                            let msg = ok(env.req_id, ReplyData::Added { id: 7 });
                            write_msg(&mut wr, &msg).await.unwrap();
                            break;
                        }
                        _ => ReplyData::Done,
                    };
                    write_msg(&mut wr, &ok(env.req_id, data)).await.unwrap();
                }
                // Keep the connection open until the host hangs up.
                let _ = rd.next::<Envelope<Request>>().await;
            });
        });
        ready.recv().unwrap();
    }

    /// A slow daemon reply to `add` is waited for, however long it takes: an
    /// `unreachable` answer would make the extension hand the download back to the
    /// browser while the daemon adds it too.
    #[tokio::test]
    async fn slow_add_reply_is_waited_for() {
        let socket = socket_name();
        let (go_tx, go) = std::sync::mpsc::channel();
        stalling_daemon(socket.clone(), go);
        let client = Client::connect(&socket, "test").await.unwrap();
        // From here on the clock only moves when everything waits.
        tokio::time::pause();
        let start = tokio::time::Instant::now();
        let release = tokio::spawn(async move {
            tokio::time::sleep(puppy_client::CALL_TIMEOUT + Duration::from_secs(5)).await;
            // Real time again: the stub's reply takes real time to arrive.
            tokio::time::resume();
            go_tx.send(()).unwrap();
        });
        let (mut browser, host_in) = tokio::io::duplex(1 << 16);
        let body = serde_json::to_vec(&add("a1", NEVER)).unwrap();
        browser.write_all(&frame(&body)).await.unwrap();
        drop(browser);
        let mut out = Vec::new();
        let served = serve(host_in, &mut out, Given(Some(client)));
        tokio::time::timeout(Duration::from_secs(300), served)
            .await
            .expect("serve hung")
            .unwrap();
        let r = decode(&out);
        release.await.unwrap();
        assert_eq!(
            r,
            vec![json!({"id": "a1", "type": "accepted", "download_id": 7})]
        );
        assert!(start.elapsed() > puppy_client::CALL_TIMEOUT);
        if let SocketName::Path(p) = &socket {
            let _ = std::fs::remove_file(p);
        }
    }

    #[tokio::test]
    async fn eof_ends_serving() {
        let (browser, host_in) = tokio::io::duplex(64);
        drop(browser);
        let mut out = Vec::new();
        serve(host_in, &mut out, Failing).await.unwrap();
        assert!(out.is_empty());
    }
}
