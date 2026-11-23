//! Client for the puppy daemon, used by the dashboard and the CLI.
//!
//! A [`Client`] owns one connection. [`Client::connect`] checks that the daemon runs as
//! the current user and performs the `hello` handshake; [`Client::connect_or_spawn`]
//! also starts a detached daemon when none is listening. Requests are tagged with a
//! fresh `req_id` and may be issued concurrently from many tasks (`call` takes `&self`):
//! a single reader task routes each response to its caller and queues events for
//! [`Client::next_event`]. The reader never blocks: when the event queue is full new
//! events are dropped and counted ([`Client::dropped_events`]).

mod peer;
mod spawn;

pub use puppy_daemon::{Paths, SocketName};
pub use spawn::spawn_daemon_detached;

use interprocess::local_socket::tokio::{Stream, prelude::*};
use interprocess::local_socket::{GenericFilePath, GenericNamespaced};
use puppy_core::types::{AddRequest, DownloadId};
use puppy_proto::{
    CodecError, DownloadSnapshot, Envelope, ErrorCode, Event, MsgReader, PROTOCOL_VERSION, Reply,
    ReplyData, Request, ServerMsg, write_msg,
};
use serde::Deserialize;
use std::collections::HashMap;
use std::io;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{BufReader, ReadHalf, WriteHalf};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio::time::{Instant, timeout};
use tokio_util::sync::CancellationToken;
use tracing::debug;

/// How long a request may take (sending it and receiving its reply).
pub const CALL_TIMEOUT: Duration = Duration::from_secs(30);
/// Capacity of the event queue read by [`Client::next_event`].
pub const EVENT_QUEUE: usize = 256;
/// How long the daemon may take to answer `hello`.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
/// [`Client::connect_or_spawn`] gives up after this long.
const SPAWN_BUDGET: Duration = Duration::from_secs(5);
const RETRY_FIRST: Duration = Duration::from_millis(50);
const RETRY_MAX: Duration = Duration::from_millis(500);
/// Windows: every instance of the pipe is busy (the daemon is there).
const ERROR_PIPE_BUSY: i32 = 231;
/// The daemon's log file, in `Paths::log_dir`.
const DAEMON_LOG: &str = "puppyd.log";
/// At most one daemon is spawned per this period while waiting for one to listen.
const RESPAWN_EVERY: Duration = Duration::from_secs(1);

#[derive(thiserror::Error, Debug)]
pub enum ClientError {
    /// Connecting or talking to the daemon failed (e.g. no daemon is listening).
    #[error("cannot talk to the daemon: {0}")]
    Io(#[from] io::Error),
    /// The socket or pipe belongs to a process running as another user (or, on
    /// Windows, one this user may not inspect, e.g. an elevated daemon: `hint` says so).
    #[error(
        "the daemon socket is owned by another user; refusing to use it{}",
        hint.map(|h| format!(" ({h})")).unwrap_or_default()
    )]
    NotOwnedByUser { hint: Option<&'static str> },
    /// The daemon said something this client does not understand.
    #[error("protocol error: {0}")]
    Protocol(String),
    /// The daemon answered with an error.
    #[error("{message} ({code:?})")]
    Daemon { code: ErrorCode, message: String },
    /// The connection is closed.
    #[error("disconnected from the daemon")]
    Disconnected,
    /// No reply within [`CALL_TIMEOUT`] (or the handshake timeout).
    #[error("the daemon did not reply in time")]
    Timeout,
    /// No daemon answered within the spawn budget after starting one.
    #[error("no daemon answered after starting one (see its log, {}): {last}", log_file.display())]
    NoDaemon {
        log_file: std::path::PathBuf,
        last: Box<ClientError>,
    },
    /// The daemon executable could not be started.
    #[error("cannot start the daemon: {0}")]
    Spawn(io::Error),
}

pub struct Client {
    shared: Arc<Shared>,
    /// `None` once a write failed or was torn: nothing more may be written.
    writer: tokio::sync::Mutex<Option<WriteHalf<Stream>>>,
    next_id: AtomicU64,
    events: Option<mpsc::Receiver<Event>>,
    reader: JoinHandle<()>,
    daemon_version: String,
}

/// State shared between the client and its reader task.
struct Shared {
    pending: Mutex<Pending>,
    connected: AtomicBool,
    dropped_events: AtomicU64,
    /// Cancelled to tear the connection down (the reader then disconnects).
    close: CancellationToken,
}

struct Pending {
    calls: HashMap<u64, oneshot::Sender<Result<Reply, String>>>,
    open: bool,
}

impl Shared {
    fn lock(&self) -> std::sync::MutexGuard<'_, Pending> {
        self.pending.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn register(
        &self,
        req_id: u64,
    ) -> Result<oneshot::Receiver<Result<Reply, String>>, ClientError> {
        let mut p = self.lock();
        if !p.open {
            return Err(ClientError::Disconnected);
        }
        let (tx, rx) = oneshot::channel();
        p.calls.insert(req_id, tx);
        Ok(rx)
    }

    /// Marks the connection closed and fails every pending call.
    fn disconnect(&self) {
        self.connected.store(false, Ordering::SeqCst);
        self.close.cancel();
        let calls = {
            let mut p = self.lock();
            p.open = false;
            std::mem::take(&mut p.calls)
        };
        // Dropping the senders makes the waiting calls return `Disconnected`.
        drop(calls);
    }
}

/// Removes a call's pending entry when the call ends in any way (also when its
/// future is dropped).
struct Unregister<'a>(&'a Shared, u64);

impl Drop for Unregister<'_> {
    fn drop(&mut self) {
        self.0.lock().calls.remove(&self.1);
    }
}

impl Client {
    /// Connects to the daemon at `name`, checks that it runs as the current user
    /// ([`ClientError::NotOwnedByUser`] otherwise) and performs the `hello` handshake.
    pub async fn connect(name: &SocketName, client_name: &str) -> Result<Self, ClientError> {
        let stream = connect_stream(name).await?;
        peer::check_owner(&stream)?;
        let (rd, mut wr) = tokio::io::split(stream);
        let mut rd = MsgReader::new(BufReader::new(rd));
        const HELLO_ID: u64 = 1;
        let hello = Envelope {
            req_id: HELLO_ID,
            body: Request::Hello {
                client: client_name.into(),
                protocol: PROTOCOL_VERSION,
            },
        };
        let handshake = async {
            write_msg(&mut wr, &hello).await.map_err(codec_error)?;
            loop {
                match rd.next::<ServerMsg>().await.map_err(codec_error)? {
                    None => return Err(ClientError::Disconnected),
                    Some(ServerMsg::Response { req_id, result }) if req_id == HELLO_ID => {
                        return Ok(result);
                    }
                    Some(other) => debug!("ignoring {other:?} before the hello reply"),
                }
            }
        };
        let reply = timeout(HANDSHAKE_TIMEOUT, handshake)
            .await
            .map_err(|_| ClientError::Timeout)??;
        let daemon_version = match reply_data(reply)? {
            ReplyData::Hello {
                protocol: PROTOCOL_VERSION,
                daemon_version,
            } => daemon_version,
            other => {
                return Err(ClientError::Protocol(format!(
                    "unexpected hello reply: {other:?}"
                )));
            }
        };

        let shared = Arc::new(Shared {
            pending: Mutex::new(Pending {
                calls: HashMap::new(),
                open: true,
            }),
            connected: AtomicBool::new(true),
            dropped_events: AtomicU64::new(0),
            close: CancellationToken::new(),
        });
        let (tx, events) = mpsc::channel(EVENT_QUEUE);
        let reader = tokio::spawn(read_loop(rd, shared.clone(), tx));
        Ok(Self {
            shared,
            writer: tokio::sync::Mutex::new(Some(wr)),
            next_id: AtomicU64::new(HELLO_ID + 1),
            events: Some(events),
            reader,
            daemon_version,
        })
    }

    /// The daemon's version, from its `hello` reply.
    pub fn daemon_version(&self) -> &str {
        &self.daemon_version
    }

    /// Stops a daemon that speaks another protocol version (e.g. one left running by
    /// an older `puppy`), at `paths.socket`. Sends `hello` with this client's protocol
    /// and, if the daemon answers `protocol_mismatch`, a `shutdown` on the same
    /// connection (daemons accept one within 5 s of the mismatch), then waits for its
    /// reply. A daemon that turns out to be compatible is left running (`Ok`); use
    /// [`call`](Self::call) with `Request::Shutdown` to stop that one. Like
    /// [`connect`](Self::connect), refuses a daemon run by another user.
    pub async fn shutdown_incompatible(paths: &Paths) -> Result<(), ClientError> {
        let stream = connect_stream(&paths.socket).await?;
        peer::check_owner(&stream)?;
        let (rd, mut wr) = tokio::io::split(stream);
        let mut rd = MsgReader::new(BufReader::new(rd));
        let exchange = async {
            let hello = Envelope {
                req_id: 1,
                body: Request::Hello {
                    client: "puppy-shutdown".into(),
                    protocol: PROTOCOL_VERSION,
                },
            };
            write_msg(&mut wr, &hello).await.map_err(codec_error)?;
            match reply_to(&mut rd, 1).await? {
                Reply::Ok { .. } => return Ok(()),
                Reply::Err {
                    code: ErrorCode::ProtocolMismatch,
                    ..
                } => {}
                Reply::Err { code, message } => return Err(ClientError::Daemon { code, message }),
            }
            let shutdown = Envelope {
                req_id: 2,
                body: Request::Shutdown,
            };
            write_msg(&mut wr, &shutdown).await.map_err(codec_error)?;
            reply_data(reply_to(&mut rd, 2).await?).map(drop)
        };
        timeout(HANDSHAKE_TIMEOUT, exchange)
            .await
            .map_err(|_| ClientError::Timeout)?
    }

    /// Like [`connect`](Self::connect) to `paths.socket`, but when no daemon answers,
    /// starts `exe daemon --idle-exit …` detached ([`spawn_daemon_detached`], in
    /// `paths.data_dir`, with `extra_env`) and retries with backoff (50 ms doubling to
    /// 500 ms) for up to 5 s. `paths` should be what the daemon resolves from its
    /// environment ([`Paths::resolve`]). While a daemon is on its way out (refusing
    /// connections or hanging up) another may be spawned, at most once per second;
    /// extra ones find the lock taken and exit. Errors that retrying cannot fix
    /// (another user's socket, a protocol mismatch, the executable failing to start)
    /// are returned at once; after 5 s, [`ClientError::NoDaemon`] naming the daemon's
    /// log file and the last connection error.
    pub async fn connect_or_spawn(
        paths: &Paths,
        client_name: &str,
        exe: &Path,
        extra_env: &[(&str, &str)],
    ) -> Result<Self, ClientError> {
        let name = &paths.socket;
        let deadline = Instant::now() + SPAWN_BUDGET;
        let mut delay = RETRY_FIRST;
        let mut last_spawn: Option<Instant> = None;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let err = match timeout(left.max(RETRY_FIRST), Self::connect(name, client_name)).await {
                Ok(Ok(c)) => return Ok(c),
                Ok(Err(e)) if retryable(&e) => e,
                Ok(Err(e)) => return Err(e),
                Err(_) => ClientError::Timeout,
            };
            if last_spawn.is_none_or(|t| t.elapsed() >= RESPAWN_EVERY) {
                debug!("no daemon answered ({err}); starting {}", exe.display());
                spawn_daemon_detached(exe, Some(&paths.data_dir), extra_env)
                    .map_err(ClientError::Spawn)?;
                last_spawn = Some(Instant::now());
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(ClientError::NoDaemon {
                    log_file: paths.log_dir.join(DAEMON_LOG),
                    last: Box::new(err),
                });
            }
            tokio::time::sleep(delay.min(left)).await;
            delay = (delay * 2).min(RETRY_MAX);
        }
    }

    /// Sends a request and waits (up to [`CALL_TIMEOUT`]) for its reply. Error replies
    /// become [`ClientError::Daemon`].
    pub async fn call(&self, req: Request) -> Result<ReplyData, ClientError> {
        self.call_within(req, Some(CALL_TIMEOUT)).await
    }

    /// Like [`call`](Self::call), but waits for the reply however long it takes (until
    /// it arrives or the connection closes). For requests whose effect must not be
    /// mistaken for a failure, e.g. an `add` the caller would otherwise hand elsewhere.
    pub async fn call_unbounded(&self, req: Request) -> Result<ReplyData, ClientError> {
        self.call_within(req, None).await
    }

    async fn call_within(
        &self,
        req: Request,
        limit: Option<Duration>,
    ) -> Result<ReplyData, ClientError> {
        let req_id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let rx = self.shared.register(req_id)?;
        let _unregister = Unregister(&self.shared, req_id);
        let exchange = async {
            self.send(&Envelope { req_id, body: req }).await?;
            match rx.await {
                Ok(Ok(reply)) => Ok(reply),
                Ok(Err(e)) => Err(ClientError::Protocol(format!("unreadable reply: {e}"))),
                Err(_) => Err(ClientError::Disconnected),
            }
        };
        let reply = match limit {
            Some(limit) => timeout(limit, exchange)
                .await
                .map_err(|_| ClientError::Timeout)??,
            None => exchange.await?,
        };
        reply_data(reply)
    }

    async fn send(&self, msg: &Envelope<Request>) -> Result<(), ClientError> {
        let mut writer = self.writer.lock().await;
        if self.shared.close.is_cancelled() {
            *writer = None;
            return Err(ClientError::Disconnected);
        }
        // Taken out while writing: a write abandoned half-way (the call timed out or
        // was dropped) leaves a torn frame on the wire, after which nothing can be
        // framed. The writer is then dropped with the future and `close` makes the
        // reader drop its half, so the OS connection closes.
        let Some(mut w) = writer.take() else {
            return Err(ClientError::Disconnected);
        };
        let torn = self.shared.close.clone().drop_guard();
        let r = write_msg(&mut w, msg).await;
        torn.disarm();
        match r {
            Ok(()) => {
                *writer = Some(w);
                Ok(())
            }
            Err(CodecError::Io(e)) => {
                debug!("writing to the daemon failed: {e}");
                self.shared.close.cancel();
                Err(ClientError::Disconnected)
            }
            // Nothing was written; the connection is fine.
            Err(e) => {
                *writer = Some(w);
                Err(ClientError::Protocol(format!("cannot send request: {e}")))
            }
        }
    }

    /// Adds a download and returns its id.
    pub async fn add(
        &self,
        req: AddRequest,
        start_paused: bool,
        limit: Option<u64>,
    ) -> Result<DownloadId, ClientError> {
        let req = Request::Add {
            request: req,
            start_paused,
            limit,
        };
        added(self.call(req).await?)
    }

    /// [`add`](Self::add) without [`CALL_TIMEOUT`]: waits for the daemon's answer.
    pub async fn add_unbounded(&self, req: AddRequest) -> Result<DownloadId, ClientError> {
        let req = Request::Add {
            request: req,
            start_paused: false,
            limit: None,
        };
        added(self.call_unbounded(req).await?)
    }

    /// All downloads, in queue order.
    pub async fn list(&self) -> Result<Vec<DownloadSnapshot>, ClientError> {
        match self.call(Request::List).await? {
            ReplyData::Downloads { downloads } => Ok(downloads),
            other => Err(unexpected(other)),
        }
    }

    /// Asks for snapshot events every `interval_ms` (the daemon clamps it to
    /// 100–1000) plus discrete events, read with [`next_event`](Self::next_event).
    pub async fn subscribe(&self, interval_ms: u64) -> Result<(), ClientError> {
        match self.call(Request::Subscribe { interval_ms }).await? {
            ReplyData::Done => Ok(()),
            other => Err(unexpected(other)),
        }
    }

    /// The next event; `None` once the connection is closed and queued events are
    /// read, or if the stream was moved out with [`take_events`](Self::take_events).
    pub async fn next_event(&mut self) -> Option<Event> {
        self.events.as_mut()?.recv().await
    }

    /// Moves the event stream out of the client, e.g. to read it in its own task while
    /// the client is shared (`Arc<Client>`) for calls. `None` if already taken; after
    /// taking, [`next_event`](Self::next_event) returns `None`.
    pub fn take_events(&mut self) -> Option<mpsc::Receiver<Event>> {
        self.events.take()
    }

    /// Whether the connection is still open.
    pub fn is_connected(&self) -> bool {
        self.shared.connected.load(Ordering::SeqCst) && !self.shared.close.is_cancelled()
    }

    /// Events dropped because the queue was full (nobody read them fast enough).
    pub fn dropped_events(&self) -> u64 {
        self.shared.dropped_events.load(Ordering::Relaxed)
    }
}

impl std::fmt::Debug for Client {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Client")
            .field("connected", &self.is_connected())
            .field("dropped_events", &self.dropped_events())
            .finish_non_exhaustive()
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        self.reader.abort();
    }
}

/// Routes responses to their callers and events into the queue until the connection
/// ends (EOF, a read error, or `close`), then fails every pending call.
async fn read_loop(
    mut rd: MsgReader<BufReader<ReadHalf<Stream>>>,
    shared: Arc<Shared>,
    events: mpsc::Sender<Event>,
) {
    loop {
        let msg = tokio::select! {
            _ = shared.close.cancelled() => break,
            m = rd.next::<serde_json::Value>() => m,
        };
        let value = match msg {
            Ok(Some(v)) => v,
            Ok(None) => break,
            // Not JSON at all: skip the line.
            Err(CodecError::Json(e)) => {
                debug!("ignoring an unreadable message: {e}");
                continue;
            }
            Err(e) => {
                debug!("reading from the daemon failed: {e}");
                break;
            }
        };
        // Parsed in two steps so a reply whose body this client cannot decode (e.g.
        // from a newer daemon) still fails its call at once instead of timing out.
        let (req_id, result) = match ServerMsg::deserialize(&value) {
            Ok(ServerMsg::Response { req_id, result }) => (req_id, Ok(result)),
            Ok(ServerMsg::Event { event }) => {
                if let Err(mpsc::error::TrySendError::Full(_)) = events.try_send(event) {
                    shared.dropped_events.fetch_add(1, Ordering::Relaxed);
                }
                continue;
            }
            Err(e) => match response_req_id(&value) {
                Some(req_id) => (req_id, Err(e.to_string())),
                // A message kind this client does not know (newer daemon): skip it.
                None => {
                    debug!("ignoring an unreadable message: {e}");
                    continue;
                }
            },
        };
        let call = shared.lock().calls.remove(&req_id);
        match call {
            // The caller may have given up meanwhile.
            Some(tx) => drop(tx.send(result)),
            None => debug!(req_id, "reply for no pending call: {result:?}"),
        }
    }
    shared.disconnect();
}

/// Reads until the response to `req_id` (skipping anything else); `Disconnected` on EOF.
/// The response is decoded leniently (only `status`, `code` and `message`), so a
/// daemon speaking another protocol version is still understood.
async fn reply_to(
    rd: &mut MsgReader<BufReader<ReadHalf<Stream>>>,
    req_id: u64,
) -> Result<Reply, ClientError> {
    loop {
        let Some(v) = rd.next::<serde_json::Value>().await.map_err(codec_error)? else {
            return Err(ClientError::Disconnected);
        };
        if response_req_id(&v) != Some(req_id) {
            debug!("ignoring {v} while waiting for reply {req_id}");
            continue;
        }
        let str_field = |k: &str| v.get(k).and_then(serde_json::Value::as_str);
        return Ok(match str_field("status") {
            Some("ok") => Reply::Ok {
                data: ReplyData::deserialize(v.get("data").unwrap_or(&serde_json::Value::Null))
                    .unwrap_or(ReplyData::Unknown),
            },
            _ => Reply::Err {
                code: v
                    .get("code")
                    .and_then(|c| ErrorCode::deserialize(c).ok())
                    .unwrap_or(ErrorCode::Unknown),
                message: str_field("message").unwrap_or_default().to_string(),
            },
        });
    }
}

/// The `req_id` of a message that claims to be a response.
fn response_req_id(v: &serde_json::Value) -> Option<u64> {
    if v.get("type")?.as_str()? != "response" {
        return None;
    }
    v.get("req_id")?.as_u64()
}

async fn connect_stream(name: &SocketName) -> io::Result<Stream> {
    match name {
        SocketName::Path(p) => Stream::connect(p.as_path().to_fs_name::<GenericFilePath>()?).await,
        SocketName::Pipe(n) => Stream::connect(n.as_str().to_ns_name::<GenericNamespaced>()?).await,
    }
}

fn codec_error(e: CodecError) -> ClientError {
    match e {
        CodecError::Io(e) => ClientError::Io(e),
        e => ClientError::Protocol(e.to_string()),
    }
}

fn reply_data(reply: Reply) -> Result<ReplyData, ClientError> {
    match reply {
        Reply::Ok { data } => Ok(data),
        Reply::Err { code, message } => Err(ClientError::Daemon { code, message }),
    }
}

fn added(data: ReplyData) -> Result<DownloadId, ClientError> {
    match data {
        ReplyData::Added { id } => Ok(id),
        other => Err(unexpected(other)),
    }
}

fn unexpected(data: ReplyData) -> ClientError {
    ClientError::Protocol(format!("unexpected reply: {data:?}"))
}

/// Whether a failed connection attempt may succeed later: nobody listening yet, or a
/// daemon on its way in or out.
fn retryable(e: &ClientError) -> bool {
    match e {
        ClientError::Disconnected | ClientError::Timeout => true,
        ClientError::Io(e) => {
            matches!(
                e.kind(),
                io::ErrorKind::NotFound
                    | io::ErrorKind::ConnectionRefused
                    | io::ErrorKind::ConnectionReset
                    | io::ErrorKind::ConnectionAborted
                    | io::ErrorKind::BrokenPipe
                    | io::ErrorKind::UnexpectedEof
                    | io::ErrorKind::WouldBlock
                    | io::ErrorKind::ResourceBusy
            ) || (cfg!(windows) && e.raw_os_error() == Some(ERROR_PIPE_BUSY))
        }
        _ => false,
    }
}
