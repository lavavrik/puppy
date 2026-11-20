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
/// At most one daemon is spawned per this period while waiting for one to listen.
const RESPAWN_EVERY: Duration = Duration::from_secs(1);

#[derive(thiserror::Error, Debug)]
pub enum ClientError {
    /// Connecting or talking to the daemon failed (e.g. no daemon is listening).
    #[error("cannot talk to the daemon: {0}")]
    Io(#[from] io::Error),
    /// The socket or pipe belongs to a process running as another user.
    #[error("the daemon socket is owned by another user; refusing to use it")]
    NotOwnedByUser,
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
    /// The daemon executable could not be started.
    #[error("cannot start the daemon: {0}")]
    Spawn(io::Error),
}

pub struct Client {
    shared: Arc<Shared>,
    /// `None` once a write failed or was torn: nothing more may be written.
    writer: tokio::sync::Mutex<Option<WriteHalf<Stream>>>,
    next_id: AtomicU64,
    events: mpsc::Receiver<Event>,
    reader: JoinHandle<()>,
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
    calls: HashMap<u64, oneshot::Sender<Reply>>,
    open: bool,
}

impl Shared {
    fn lock(&self) -> std::sync::MutexGuard<'_, Pending> {
        self.pending.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn register(&self, req_id: u64) -> Result<oneshot::Receiver<Reply>, ClientError> {
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
        if !peer::owned_by_current_user(&stream)? {
            return Err(ClientError::NotOwnedByUser);
        }
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
        match reply_data(reply)? {
            ReplyData::Hello {
                protocol: PROTOCOL_VERSION,
                ..
            } => {}
            other => {
                return Err(ClientError::Protocol(format!(
                    "unexpected hello reply: {other:?}"
                )));
            }
        }

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
            events,
            reader,
        })
    }

    /// Like [`connect`](Self::connect), but when no daemon answers, starts
    /// `exe daemon --idle-exit …` detached ([`spawn_daemon_detached`]) and retries with
    /// backoff (50 ms doubling to 500 ms) for up to 5 s. While a daemon is on its way
    /// out (refusing connections or hanging up) another may be spawned, at most once
    /// per second; extra ones find the lock taken and exit. Errors that retrying cannot
    /// fix (another user's socket, a protocol mismatch, the executable failing to
    /// start) are returned at once; otherwise the last connection error after 5 s.
    pub async fn connect_or_spawn(
        name: &SocketName,
        client_name: &str,
        exe: &Path,
    ) -> Result<Self, ClientError> {
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
                spawn_daemon_detached(exe, &[]).map_err(ClientError::Spawn)?;
                last_spawn = Some(Instant::now());
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(err);
            }
            tokio::time::sleep(delay.min(left)).await;
            delay = (delay * 2).min(RETRY_MAX);
        }
    }

    /// Sends a request and waits (up to [`CALL_TIMEOUT`]) for its reply. Error replies
    /// become [`ClientError::Daemon`].
    pub async fn call(&self, req: Request) -> Result<ReplyData, ClientError> {
        let req_id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let rx = self.shared.register(req_id)?;
        let _unregister = Unregister(&self.shared, req_id);
        let exchange = async {
            self.send(&Envelope { req_id, body: req }).await?;
            rx.await.map_err(|_| ClientError::Disconnected)
        };
        let reply = timeout(CALL_TIMEOUT, exchange)
            .await
            .map_err(|_| ClientError::Timeout)??;
        reply_data(reply)
    }

    async fn send(&self, msg: &Envelope<Request>) -> Result<(), ClientError> {
        let mut writer = self.writer.lock().await;
        let Some(w) = writer.as_mut() else {
            return Err(ClientError::Disconnected);
        };
        if self.shared.close.is_cancelled() {
            return Err(ClientError::Disconnected);
        }
        // A write abandoned half-way (the call timed out or was dropped) leaves a torn
        // frame on the wire, after which nothing can be framed: close the connection.
        let torn = self.shared.close.clone().drop_guard();
        let r = write_msg(w, msg).await;
        torn.disarm();
        match r {
            Ok(()) => Ok(()),
            Err(CodecError::Io(e)) => {
                debug!("writing to the daemon failed: {e}");
                *writer = None;
                self.shared.close.cancel();
                Err(ClientError::Disconnected)
            }
            // Nothing was written; the connection is fine.
            Err(e) => Err(ClientError::Protocol(format!("cannot send request: {e}"))),
        }
    }

    /// Adds a download and returns its id.
    pub async fn add(
        &self,
        req: AddRequest,
        start_paused: bool,
        limit: Option<u64>,
    ) -> Result<DownloadId, ClientError> {
        match self
            .call(Request::Add {
                request: req,
                start_paused,
                limit,
            })
            .await?
        {
            ReplyData::Added { id } => Ok(id),
            other => Err(unexpected(other)),
        }
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
    /// read.
    pub async fn next_event(&mut self) -> Option<Event> {
        self.events.recv().await
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
            m = rd.next::<ServerMsg>() => m,
        };
        match msg {
            Ok(Some(ServerMsg::Response { req_id, result })) => {
                let call = shared.lock().calls.remove(&req_id);
                match call {
                    // The caller may have given up meanwhile.
                    Some(tx) => drop(tx.send(result)),
                    None => debug!(req_id, "reply for no pending call: {result:?}"),
                }
            }
            Ok(Some(ServerMsg::Event { event })) => {
                if let Err(mpsc::error::TrySendError::Full(_)) = events.try_send(event) {
                    shared.dropped_events.fetch_add(1, Ordering::Relaxed);
                }
            }
            Ok(None) => break,
            // A message kind this client does not know (newer daemon): skip it.
            Err(CodecError::Json(e)) => debug!("ignoring an unreadable message: {e}"),
            Err(e) => {
                debug!("reading from the daemon failed: {e}");
                break;
            }
        }
    }
    shared.disconnect();
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
