//! The daemon's local-socket server: a Unix domain socket (Linux/macOS) or a Windows
//! named pipe, speaking the newline-delimited JSON protocol of `puppy-proto`.
//!
//! Per connection:
//! - The first message must be `hello` with protocol [`PROTOCOL_VERSION`]; anything else
//!   gets an error reply and the connection is closed.
//! - One reader loop parses requests leniently (unknown methods get `bad_request` with
//!   the caller's `req_id`; unparsable lines get `bad_request` with `req_id` 0) and spawns
//!   a task per manager request, so slow requests never hold up others.
//! - One writer task owns the write half. It is fed by a bounded queue (capacity
//!   [`QUEUE_CAP`]) for responses and discrete events, and by a latest-value slot for
//!   snapshots, so a slow client only ever gets the newest snapshot. Queued messages
//!   are written before a pending snapshot.
//! - Nothing waits on a slow client except that client's own tasks: discrete events
//!   are dropped (with a rate-limited warning) when its queue is full, and a response
//!   that cannot be queued within [`RESPONSE_TIMEOUT`] closes the connection.

use crate::manager::{Manager, ManagerError};
use crate::paths::SocketName;
use interprocess::local_socket::tokio::{Listener, Stream, prelude::*};
use interprocess::local_socket::{GenericFilePath, GenericNamespaced, ListenerOptions};
use puppy_proto::{
    CodecError, ErrorCode, Event, MsgReader, PROTOCOL_VERSION, Reply, ReplyData, Request,
    ServerMsg, parse_request, write_msg,
};
use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};
use tokio::io::{AsyncWrite, BufReader};
use tokio::sync::mpsc::error::{SendTimeoutError, TrySendError};
use tokio::sync::{broadcast, mpsc, watch};
use tokio::task::{JoinHandle, JoinSet};
use tokio::time::{MissedTickBehavior, timeout};
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

/// Per-connection queue for responses and discrete events.
pub const QUEUE_CAP: usize = 64;
/// How long a response may wait for room in a client's queue before the client is
/// considered stuck and its connection closed. Also bounds how long a connection
/// lingers after the client stopped sending to deliver outstanding replies.
pub const RESPONSE_TIMEOUT: Duration = Duration::from_secs(5);
/// A client must say `hello` within this time.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
/// When the server stops, queued messages get this long to be written.
const DRAIN_TIMEOUT: Duration = Duration::from_secs(1);
/// At most one "dropped events" warning per client per this period.
const DROP_WARN_EVERY: Duration = Duration::from_secs(5);
const MIN_INTERVAL_MS: u64 = 100;
const MAX_INTERVAL_MS: u64 = 1000;

pub struct Server {
    listener: Listener,
    manager: Arc<Manager>,
    clients: Arc<AtomicUsize>,
    shutdown: CancellationToken,
}

impl Server {
    /// Binds the local socket. Fails if the name is in use (stale sockets are the
    /// caller's business). On Unix the socket file is made mode 0600 and removed again
    /// when the server is dropped (i.e. when [`run`](Self::run) returns).
    pub async fn bind(name: &SocketName, manager: Arc<Manager>) -> io::Result<Self> {
        let sock_name = match name {
            SocketName::Path(p) => p.as_path().to_fs_name::<GenericFilePath>()?,
            SocketName::Pipe(n) => n.as_str().to_ns_name::<GenericNamespaced>()?,
        };
        let listener = ListenerOptions::new().name(sock_name).create_tokio()?;
        // On error the listener is dropped, which removes the socket file again.
        restrict_socket(name)?;
        Ok(Self {
            listener,
            manager,
            clients: Arc::new(AtomicUsize::new(0)),
            shutdown: CancellationToken::new(),
        })
    }

    /// Number of currently open connections (handshaken or not).
    pub fn client_count(&self) -> Arc<AtomicUsize> {
        self.clients.clone()
    }

    /// Cancelled when a client sends `Shutdown` (after its `Done` reply is queued).
    pub fn shutdown_requested(&self) -> CancellationToken {
        self.shutdown.clone()
    }

    /// Accepts connections until `stop` is cancelled, then closes every connection
    /// (giving queued messages up to [`DRAIN_TIMEOUT`] to be written).
    pub async fn run(self, stop: CancellationToken) {
        let conns = CancellationToken::new();
        let mut tasks = JoinSet::new();
        loop {
            tokio::select! {
                _ = stop.cancelled() => break,
                Some(_) = tasks.join_next(), if !tasks.is_empty() => {}
                r = self.listener.accept() => match r {
                    Ok(stream) => {
                        let guard = ClientGuard::new(&self.clients);
                        let conn = serve(
                            stream,
                            self.manager.clone(),
                            self.shutdown.clone(),
                            conns.child_token(),
                        );
                        tasks.spawn(async move {
                            let _guard = guard;
                            conn.await;
                        });
                    }
                    Err(e) => {
                        // E.g. out of file descriptors: back off instead of spinning.
                        warn!("accepting a connection failed: {e}");
                        tokio::time::sleep(Duration::from_millis(100)).await;
                    }
                },
            }
        }
        conns.cancel();
        let all_done = async { while tasks.join_next().await.is_some() {} };
        if timeout(DRAIN_TIMEOUT * 2, all_done).await.is_err() {
            debug!("aborting connections that did not finish in time");
        }
        // Dropping `tasks` aborts any stragglers.
    }
}

#[cfg(unix)]
fn restrict_socket(name: &SocketName) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    match name {
        SocketName::Path(p) => std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o600)),
        SocketName::Pipe(_) => Ok(()),
    }
}

#[cfg(not(unix))]
fn restrict_socket(_name: &SocketName) -> io::Result<()> {
    // Named pipes keep the default security descriptor: full control for the creator
    // owner, SYSTEM and administrators; everyone else may only open the pipe for
    // reading, which is not enough to send `hello`.
    Ok(())
}

/// Counts a connection for as long as it lives (also if its task is aborted).
struct ClientGuard(Arc<AtomicUsize>);

impl ClientGuard {
    fn new(count: &Arc<AtomicUsize>) -> Self {
        count.fetch_add(1, Ordering::SeqCst);
        Self(count.clone())
    }
}

impl Drop for ClientGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

fn response(req_id: u64, result: Reply) -> ServerMsg {
    ServerMsg::Response { req_id, result }
}

fn err(code: ErrorCode, message: impl Into<String>) -> Reply {
    Reply::Err {
        code,
        message: message.into(),
    }
}

fn ok(data: ReplyData) -> Reply {
    Reply::Ok { data }
}

/// The reply to the first message of a connection and whether the connection may
/// continue. `None`: close silently (EOF or I/O error).
fn handshake(first: Result<Option<serde_json::Value>, CodecError>) -> Option<(ServerMsg, bool)> {
    let v = match first {
        Ok(Some(v)) => v,
        Ok(None) | Err(CodecError::Io(_)) => return None,
        Err(e) => {
            return Some((
                response(0, err(ErrorCode::BadRequest, e.to_string())),
                false,
            ));
        }
    };
    Some(match parse_request(v) {
        Err((req_id, msg)) => (
            response(req_id.unwrap_or(0), err(ErrorCode::BadRequest, msg)),
            false,
        ),
        Ok(env) => match env.body {
            Request::Hello {
                protocol: PROTOCOL_VERSION,
                ..
            } => (
                response(
                    env.req_id,
                    ok(ReplyData::Hello {
                        daemon_version: env!("CARGO_PKG_VERSION").into(),
                        protocol: PROTOCOL_VERSION,
                    }),
                ),
                true,
            ),
            Request::Hello { protocol, .. } => (
                response(
                    env.req_id,
                    err(
                        ErrorCode::ProtocolMismatch,
                        format!(
                            "protocol {protocol} is not supported (the daemon speaks {PROTOCOL_VERSION})"
                        ),
                    ),
                ),
                false,
            ),
            _ => (
                response(
                    env.req_id,
                    err(ErrorCode::BadRequest, "the first message must be hello"),
                ),
                false,
            ),
        },
    })
}

/// Serves one connection until the client goes away or `cancel` fires.
async fn serve(
    stream: Stream,
    manager: Arc<Manager>,
    shutdown: CancellationToken,
    cancel: CancellationToken,
) {
    let (rd, mut wr) = tokio::io::split(stream);
    let mut reader = MsgReader::new(BufReader::new(rd));

    let first = tokio::select! {
        _ = cancel.cancelled() => return,
        r = timeout(HANDSHAKE_TIMEOUT, reader.next::<serde_json::Value>()) => match r {
            Ok(r) => r,
            Err(_) => {
                debug!("client did not say hello in time");
                return;
            }
        },
    };
    let Some((reply, accepted)) = handshake(first) else {
        return;
    };
    tokio::select! {
        _ = cancel.cancelled() => return,
        r = timeout(RESPONSE_TIMEOUT, write_msg(&mut wr, &reply)) => {
            if !matches!(r, Ok(Ok(()))) {
                return;
            }
        }
    }
    if !accepted {
        return;
    }

    let (tx, rx) = mpsc::channel(QUEUE_CAP);
    let (snap_tx, snap_rx) = watch::channel(None);
    let mut writer = tokio::spawn(writer_task(wr, rx, snap_rx, cancel.clone()));
    let mut conn = Conn {
        manager,
        shutdown,
        cancel: cancel.clone(),
        tx,
        snaps: Arc::new(snap_tx),
        sub: None,
    };
    loop {
        let next = tokio::select! {
            _ = cancel.cancelled() => break,
            r = reader.next::<serde_json::Value>() => r,
        };
        match next {
            Ok(Some(v)) => conn.handle(v).await,
            Ok(None) => break,
            Err(CodecError::Json(e)) => {
                conn.respond(0, err(ErrorCode::BadRequest, format!("invalid json: {e}")))
                    .await;
            }
            Err(e @ CodecError::TooLong) => {
                // The stream is mid-line and unusable: report and close.
                conn.respond(0, err(ErrorCode::BadRequest, e.to_string()))
                    .await;
                break;
            }
            Err(CodecError::Io(e)) => {
                debug!("connection read failed: {e}");
                break;
            }
        }
    }
    conn.unsubscribe().await;
    drop(conn);
    // The writer ends once in-flight requests have replied and everything is written.
    if timeout(RESPONSE_TIMEOUT, &mut writer).await.is_err() {
        cancel.cancel();
        let _ = writer.await;
    }
}

/// Per-connection state owned by the reader loop.
struct Conn {
    manager: Arc<Manager>,
    shutdown: CancellationToken,
    cancel: CancellationToken,
    tx: mpsc::Sender<ServerMsg>,
    snaps: Arc<watch::Sender<Option<ServerMsg>>>,
    sub: Option<JoinHandle<()>>,
}

impl Conn {
    async fn respond(&self, req_id: u64, result: Reply) {
        send_response(&self.tx, &self.cancel, response(req_id, result)).await;
    }

    async fn handle(&mut self, v: serde_json::Value) {
        let env = match parse_request(v) {
            Ok(env) => env,
            Err((req_id, msg)) => {
                return self
                    .respond(req_id.unwrap_or(0), err(ErrorCode::BadRequest, msg))
                    .await;
            }
        };
        let req_id = env.req_id;
        match env.body {
            Request::Hello { .. } => {
                self.respond(req_id, err(ErrorCode::BadRequest, "already said hello"))
                    .await;
            }
            Request::Subscribe { interval_ms } => {
                self.unsubscribe().await;
                // Queued before the first snapshot exists, so the reply comes first.
                self.respond(req_id, ok(ReplyData::Done)).await;
                let every =
                    Duration::from_millis(interval_ms.clamp(MIN_INTERVAL_MS, MAX_INTERVAL_MS));
                self.sub = Some(tokio::spawn(subscription(
                    self.manager.clone(),
                    every,
                    self.tx.clone(),
                    self.snaps.clone(),
                )));
            }
            Request::Unsubscribe => {
                self.unsubscribe().await;
                self.respond(req_id, ok(ReplyData::Done)).await;
            }
            Request::Shutdown => {
                self.respond(req_id, ok(ReplyData::Done)).await;
                self.shutdown.cancel();
            }
            body => {
                let manager = self.manager.clone();
                let tx = self.tx.clone();
                let cancel = self.cancel.clone();
                // Detached on purpose: aborting a manager command half-way is never
                // wanted; if the client is gone the reply is simply discarded.
                tokio::spawn(async move {
                    let result = match dispatch(&manager, body).await {
                        Ok(data) => ok(data),
                        Err(e) => err(e.code(), e.to_string()),
                    };
                    send_response(&tx, &cancel, response(req_id, result)).await;
                });
            }
        }
    }

    /// Stops the subscription (if any) and clears a pending snapshot, so nothing
    /// subscription-related is written after this returns.
    async fn unsubscribe(&mut self) {
        if let Some(sub) = self.sub.take() {
            sub.abort();
            let _ = sub.await;
        }
        self.snaps.send_replace(None);
    }
}

/// Runs a manager request (connection-level methods are handled by [`Conn`]).
async fn dispatch(m: &Manager, body: Request) -> Result<ReplyData, ManagerError> {
    use ReplyData::Done;
    Ok(match body {
        Request::Add {
            request,
            start_paused,
            limit,
        } => ReplyData::Added {
            id: m.add(request, start_paused, limit).await?,
        },
        Request::List => ReplyData::Downloads {
            downloads: m.snapshot().0,
        },
        Request::Get { id } => {
            let (download, detail) = m.get(id)?;
            ReplyData::Download { download, detail }
        }
        Request::Pause { id } => m.pause(id).await.map(|()| Done)?,
        Request::Resume { id } => m.resume(id).await.map(|()| Done)?,
        Request::Remove { id, delete_files } => m.remove(id, delete_files).await.map(|()| Done)?,
        Request::Restart { id } => m.restart(id).await.map(|()| Done)?,
        Request::Move { id, position } => m.move_to(id, position).await.map(|()| Done)?,
        Request::SetLimit { id, bps } => m.set_limit(id, bps).map(|()| Done)?,
        Request::SetGlobalLimits { down, up } => {
            m.set_global_limits(down, up);
            Done
        }
        Request::SetMaxActive { n } => m.set_max_active(n).map(|()| Done)?,
        Request::GetSettings => ReplyData::Settings {
            settings: m.settings(),
        },
        Request::Hello { .. }
        | Request::Subscribe { .. }
        | Request::Unsubscribe
        | Request::Shutdown => {
            return Err(ManagerError::BadRequest("not a manager request".into()));
        }
    })
}

/// Queues a response; never drops it. A client that leaves no room for
/// [`RESPONSE_TIMEOUT`] is hopelessly stuck and gets disconnected.
async fn send_response(tx: &mpsc::Sender<ServerMsg>, cancel: &CancellationToken, msg: ServerMsg) {
    match tx.send_timeout(msg, RESPONSE_TIMEOUT).await {
        Ok(()) | Err(SendTimeoutError::Closed(_)) => {}
        Err(SendTimeoutError::Timeout(_)) => {
            warn!(
                "client has not read its replies for {RESPONSE_TIMEOUT:?}; closing the connection"
            );
            cancel.cancel();
        }
    }
}

/// Publishes a snapshot every `every` into the latest-value slot and forwards the
/// manager's discrete events into the queue, dropping them when it is full.
async fn subscription(
    manager: Arc<Manager>,
    every: Duration,
    tx: mpsc::Sender<ServerMsg>,
    snaps: Arc<watch::Sender<Option<ServerMsg>>>,
) {
    let mut events = manager.events();
    let mut tick = tokio::time::interval(every);
    tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut dropped = 0u64;
    let mut last_warn: Option<Instant> = None;
    loop {
        tokio::select! {
            // The writer is gone (connection closed).
            _ = tx.closed() => return,
            _ = tick.tick() => {
                let (downloads, totals) = manager.snapshot();
                snaps.send_replace(Some(ServerMsg::Event {
                    event: Event::Snapshot { downloads, totals },
                }));
            }
            ev = events.recv() => match ev {
                Ok(event) => match tx.try_send(ServerMsg::Event { event }) {
                    Ok(()) => {}
                    Err(TrySendError::Full(_)) => {
                        dropped += 1;
                        if last_warn.is_none_or(|t| t.elapsed() >= DROP_WARN_EVERY) {
                            warn!("slow client: dropped {dropped} event(s), its queue is full");
                            dropped = 0;
                            last_warn = Some(Instant::now());
                        }
                    }
                    Err(TrySendError::Closed(_)) => return,
                },
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    warn!("subscriber missed {n} manager event(s)");
                }
                Err(broadcast::error::RecvError::Closed) => return,
            },
        }
    }
}

/// Writes queued messages first, then the newest snapshot if it changed. Ends when
/// every queue sender is gone, on a write error (cancelling the connection), or on
/// `cancel` (then flushing what is queued for up to [`DRAIN_TIMEOUT`]).
async fn writer_task<W: AsyncWrite + Unpin>(
    mut wr: W,
    mut rx: mpsc::Receiver<ServerMsg>,
    mut snaps: watch::Receiver<Option<ServerMsg>>,
    cancel: CancellationToken,
) {
    let mut snaps_open = true;
    loop {
        let msg = tokio::select! {
            biased;
            _ = cancel.cancelled() => break,
            m = rx.recv() => match m {
                Some(m) => m,
                None => return,
            },
            r = snaps.changed(), if snaps_open => {
                if r.is_err() {
                    snaps_open = false;
                    continue;
                }
                match snaps.borrow_and_update().clone() {
                    Some(m) => m,
                    None => continue,
                }
            }
        };
        tokio::select! {
            // Cancelled mid-write: the frame may be torn, so write nothing more.
            _ = cancel.cancelled() => return,
            r = write_one(&mut wr, &msg) => if r.is_err() {
                cancel.cancel();
                return;
            }
        }
    }
    let drain = async {
        while let Ok(m) = rx.try_recv() {
            if write_one(&mut wr, &m).await.is_err() {
                return;
            }
        }
    };
    let _ = timeout(DRAIN_TIMEOUT, drain).await;
}

/// Writes one message. A message too large to frame is not a connection error: a
/// response becomes an `internal` error reply, an event is skipped.
async fn write_one<W: AsyncWrite + Unpin>(wr: &mut W, msg: &ServerMsg) -> io::Result<()> {
    match write_msg(wr, msg).await {
        Ok(()) => Ok(()),
        Err(CodecError::Io(e)) => {
            debug!("connection write failed: {e}");
            Err(e)
        }
        Err(e) => {
            warn!("cannot send message: {e}");
            if let ServerMsg::Response { req_id, .. } = msg {
                let reply = response(*req_id, err(ErrorCode::Internal, e.to_string()));
                write_msg(wr, &reply).await.map_err(io::Error::other)?;
            }
            Ok(())
        }
    }
}
