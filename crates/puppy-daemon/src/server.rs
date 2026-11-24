//! The daemon's local-socket server: a Unix domain socket (Linux/macOS) or a Windows
//! named pipe, speaking the newline-delimited JSON protocol of `puppy-proto`.
//!
//! Access control: on Unix the socket file is created mode 0600 (before `bind` on
//! Linux; elsewhere it is chmodded right after `bind`, which is why the default socket
//! lives in a 0700 directory) and every accepted connection's peer must run as this
//! process's effective user, otherwise it is closed at once. On Windows the pipe's DACL
//! admits the current user only.
//!
//! Per connection:
//! - The first message must be `hello` with protocol [`PROTOCOL_VERSION`]; anything else
//!   gets an error reply and the connection is closed.
//! - One reader loop parses requests leniently (unknown methods get `bad_request` with
//!   the caller's `req_id`; unparsable lines get `bad_request` with `req_id` 0) and spawns
//!   a task per manager request, so slow requests never hold up others. At most
//!   [`QUEUE_CAP`] requests are in flight per connection; beyond that the reader stops
//!   reading, so a client that pipelines without reading is pushed back by the socket.
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
    ServerMsg, UpdateStatus, parse_request, write_msg,
};
use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};
use tokio::io::{AsyncWrite, BufReader};
use tokio::sync::mpsc::error::{SendTimeoutError, TrySendError};
use tokio::sync::{Semaphore, broadcast, mpsc, watch};
use tokio::task::{JoinHandle, JoinSet};
use tokio::time::{MissedTickBehavior, timeout};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, warn};

/// Per-connection queue for responses and discrete events; also the per-connection
/// limit on manager requests in flight.
pub const QUEUE_CAP: usize = 64;
/// How long a response may wait for room in a client's queue before the client is
/// considered stuck and its connection closed. Also bounds how long a connection
/// lingers after the client stopped sending to deliver outstanding replies.
pub const RESPONSE_TIMEOUT: Duration = Duration::from_secs(5);
/// A client must say `hello` within this time.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
/// After a `protocol_mismatch`, how long the connection stays open for a `shutdown`
/// (so a newer client can stop this daemon).
pub const MISMATCH_SHUTDOWN_WINDOW: Duration = Duration::from_secs(5);
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
    in_flight: Arc<InFlightStats>,
}

/// Server-wide count of spawned manager-request tasks (for tests and diagnostics).
#[derive(Debug, Default)]
pub struct InFlightStats {
    now: AtomicUsize,
    peak: AtomicUsize,
}

impl InFlightStats {
    /// Requests currently being handled.
    pub fn current(&self) -> usize {
        self.now.load(Ordering::SeqCst)
    }

    /// The most requests ever handled at once.
    pub fn peak(&self) -> usize {
        self.peak.load(Ordering::SeqCst)
    }

    fn enter(self: &Arc<Self>) -> InFlightGuard {
        let n = self.now.fetch_add(1, Ordering::SeqCst) + 1;
        self.peak.fetch_max(n, Ordering::SeqCst);
        InFlightGuard(self.clone())
    }
}

struct InFlightGuard(Arc<InFlightStats>);

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        self.0.now.fetch_sub(1, Ordering::SeqCst);
    }
}

impl Server {
    /// Binds the local socket. Fails if the name is in use (stale sockets are the
    /// caller's business). On Unix the socket file is mode 0600 (see the module docs)
    /// and removed again when the server is dropped (i.e. when [`run`](Self::run)
    /// returns).
    pub async fn bind(name: &SocketName, manager: Arc<Manager>) -> io::Result<Self> {
        let sock_name = match name {
            SocketName::Path(p) => p.as_path().to_fs_name::<GenericFilePath>()?,
            SocketName::Pipe(n) => n.as_str().to_ns_name::<GenericNamespaced>()?,
        };
        let opts = restrict_pipe(socket_mode(ListenerOptions::new().name(sock_name)))?;
        let listener = opts.create_tokio()?;
        // On error the listener is dropped, which removes the socket file again.
        restrict_socket(name)?;
        Ok(Self {
            listener,
            manager,
            clients: Arc::new(AtomicUsize::new(0)),
            shutdown: CancellationToken::new(),
            in_flight: Arc::default(),
        })
    }

    /// Number of currently open connections (handshaken or not).
    pub fn client_count(&self) -> Arc<AtomicUsize> {
        self.clients.clone()
    }

    /// Manager-request tasks in flight across all connections.
    pub fn in_flight_stats(&self) -> Arc<InFlightStats> {
        self.in_flight.clone()
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
                Some(r) = tasks.join_next(), if !tasks.is_empty() => log_join(r),
                r = self.listener.accept() => match r {
                    Ok(stream) => {
                        let guard = ClientGuard::new(&self.clients);
                        let conn = serve(
                            stream,
                            self.manager.clone(),
                            self.shutdown.clone(),
                            self.in_flight.clone(),
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
        let all_done = async {
            while let Some(r) = tasks.join_next().await {
                log_join(r);
            }
        };
        if timeout(DRAIN_TIMEOUT * 2, all_done).await.is_err() {
            debug!("aborting connections that did not finish in time");
        }
        // Dropping `tasks` aborts any stragglers.
    }
}

/// Reports a panicked connection (or writer) task; cancellations are expected.
fn log_join(r: Result<(), tokio::task::JoinError>) {
    if let Err(e) = r
        && e.is_panic()
    {
        error!("connection task panicked: {e}");
    }
}

/// Linux: the socket file is created 0600 (an `fchmod` before `bind`), so it is never
/// reachable by others, even briefly. Other systems do not support this (interprocess
/// returns `Unsupported`) and rely on [`restrict_socket`] after `bind`.
#[cfg(target_os = "linux")]
fn socket_mode(opts: ListenerOptions<'_>) -> ListenerOptions<'_> {
    use interprocess::os::unix::local_socket::ListenerOptionsExt;
    opts.mode(0o600)
}

#[cfg(not(target_os = "linux"))]
fn socket_mode(opts: ListenerOptions<'_>) -> ListenerOptions<'_> {
    opts
}

/// Whether a peer with effective user id `peer` (if known) may talk to a daemon
/// running as `me`. Unknown credentials are never trusted.
#[cfg_attr(not(unix), allow(dead_code))]
fn same_user(peer: Option<u32>, me: u32) -> bool {
    peer == Some(me)
}

/// Whether the process at the other end runs as this process's effective user.
#[cfg(unix)]
fn peer_allowed(stream: &Stream) -> io::Result<bool> {
    let peer = stream.peer_creds()?.euid();
    // SAFETY: geteuid has no preconditions and cannot fail.
    let me = unsafe { libc::geteuid() };
    Ok(same_user(peer, me))
}

/// Windows: the pipe's DACL already admits the current user only.
#[cfg(not(unix))]
fn peer_allowed(_stream: &Stream) -> io::Result<bool> {
    Ok(true)
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
    Ok(())
}

/// Windows: a protected DACL granting access to the current user only (the default
/// one lets everyone open the pipe for reading). It applies to every pipe instance.
#[cfg(windows)]
fn restrict_pipe(opts: ListenerOptions<'_>) -> io::Result<ListenerOptions<'_>> {
    use interprocess::os::windows::local_socket::ListenerOptionsExt;
    use interprocess::os::windows::security_descriptor::SecurityDescriptor;
    let sddl = format!("D:P(A;;GA;;;{})", current_user_sid()?);
    let sddl = widestring::U16CString::from_str(&sddl).map_err(io::Error::other)?;
    Ok(opts.security_descriptor(SecurityDescriptor::deserialize(&sddl)?))
}

#[cfg(not(windows))]
fn restrict_pipe(opts: ListenerOptions<'_>) -> io::Result<ListenerOptions<'_>> {
    Ok(opts)
}

/// The string SID (`S-1-5-21-...`) of the user this process runs as.
#[cfg(windows)]
fn current_user_sid() -> io::Result<String> {
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, LocalFree};
    use windows_sys::Win32::Security::Authorization::ConvertSidToStringSidW;
    use windows_sys::Win32::Security::{GetTokenInformation, TOKEN_QUERY, TOKEN_USER, TokenUser};
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
    // SAFETY: plain Win32 calls; the token handle is closed on every path, the
    // TOKEN_USER buffer is u64-aligned and sized by the first call, and the string
    // from ConvertSidToStringSidW is NUL-terminated and freed with LocalFree.
    unsafe {
        let mut token: HANDLE = std::ptr::null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            return Err(io::Error::last_os_error());
        }
        let mut len = 0u32;
        GetTokenInformation(token, TokenUser, std::ptr::null_mut(), 0, &mut len);
        let mut buf = vec![0u64; (len as usize).div_ceil(8).max(1)];
        let ok = GetTokenInformation(token, TokenUser, buf.as_mut_ptr().cast(), len, &mut len);
        let token_err = io::Error::last_os_error();
        CloseHandle(token);
        if ok == 0 {
            return Err(token_err);
        }
        let user = &*buf.as_ptr().cast::<TOKEN_USER>();
        let mut wide: *mut u16 = std::ptr::null_mut();
        if ConvertSidToStringSidW(user.User.Sid, &mut wide) == 0 {
            return Err(io::Error::last_os_error());
        }
        let n = (0..).take_while(|&i| *wide.add(i) != 0).count();
        let sid = String::from_utf16_lossy(std::slice::from_raw_parts(wide, n));
        LocalFree(wide.cast());
        Ok(sid)
    }
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

/// What happens to a connection after the reply to its first message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum After {
    /// Handshake done: serve requests.
    Serve,
    /// Close.
    Close,
    /// The client speaks another protocol version: accept one `shutdown`, then close.
    Mismatch,
}

/// The reply to the first message of a connection and what happens next. `None`:
/// close silently (EOF or I/O error).
fn handshake(first: Result<Option<serde_json::Value>, CodecError>) -> Option<(ServerMsg, After)> {
    let v = match first {
        Ok(Some(v)) => v,
        Ok(None) | Err(CodecError::Io(_)) => return None,
        Err(e) => {
            return Some((
                response(0, err(ErrorCode::BadRequest, e.to_string())),
                After::Close,
            ));
        }
    };
    Some(match parse_request(v) {
        Err((req_id, msg)) => (
            response(req_id.unwrap_or(0), err(ErrorCode::BadRequest, msg)),
            After::Close,
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
                After::Serve,
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
                After::Mismatch,
            ),
            _ => (
                response(
                    env.req_id,
                    err(ErrorCode::BadRequest, "the first message must be hello"),
                ),
                After::Close,
            ),
        },
    })
}

/// Serves one connection until the client goes away or `cancel` fires.
async fn serve(
    stream: Stream,
    manager: Arc<Manager>,
    shutdown: CancellationToken,
    in_flight: Arc<InFlightStats>,
    cancel: CancellationToken,
) {
    match peer_allowed(&stream) {
        Ok(true) => {}
        Ok(false) => {
            warn!("refusing a connection from a process running as another user");
            return;
        }
        Err(e) => {
            warn!("refusing a connection whose user cannot be checked: {e}");
            return;
        }
    }
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
    let Some((reply, after)) = handshake(first) else {
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
    match after {
        After::Serve => {}
        After::Close => return,
        After::Mismatch => {
            return mismatch_shutdown(&mut reader, &mut wr, &shutdown, &cancel).await;
        }
    }

    let (tx, rx) = mpsc::channel(QUEUE_CAP);
    let snaps = Arc::new(SnapshotSlot(watch::channel(None).0));
    let mut writer = tokio::spawn(writer_task(wr, rx, snaps.clone(), cancel.clone()));
    let mut conn = Conn {
        manager,
        shutdown,
        cancel: cancel.clone(),
        in_flight,
        permits: Arc::new(Semaphore::new(QUEUE_CAP)),
        tx,
        snaps,
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
    let r = match timeout(RESPONSE_TIMEOUT, &mut writer).await {
        Ok(r) => r,
        Err(_) => {
            cancel.cancel();
            writer.await
        }
    };
    log_join(r);
}

/// After a protocol mismatch: waits up to [`MISMATCH_SHUTDOWN_WINDOW`] for one message
/// and, if it is a `shutdown` (only its `method` is looked at, so any protocol version's
/// envelope works), replies `done` and requests the daemon's shutdown. Anything else
/// closes the connection.
async fn mismatch_shutdown<R, W>(
    reader: &mut MsgReader<R>,
    wr: &mut W,
    shutdown: &CancellationToken,
    cancel: &CancellationToken,
) where
    R: tokio::io::AsyncBufRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let next = tokio::select! {
        _ = cancel.cancelled() => return,
        r = timeout(MISMATCH_SHUTDOWN_WINDOW, reader.next::<serde_json::Value>()) => r,
    };
    let Ok(Ok(Some(v))) = next else { return };
    if v.get("method").and_then(serde_json::Value::as_str) != Some("shutdown") {
        debug!("ignoring a request from a client speaking another protocol");
        return;
    }
    let req_id = v
        .get("req_id")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    warn!("a client speaking another protocol version asked the daemon to shut down");
    let _ = timeout(
        RESPONSE_TIMEOUT,
        write_msg(wr, &response(req_id, ok(ReplyData::Done))),
    )
    .await;
    shutdown.cancel();
}

/// Per-connection state owned by the reader loop.
struct Conn {
    manager: Arc<Manager>,
    shutdown: CancellationToken,
    cancel: CancellationToken,
    in_flight: Arc<InFlightStats>,
    /// Bounds this connection's in-flight manager requests. When they are used up the
    /// reader stops reading, so a client pipelining requests without reading its
    /// replies is pushed back through the socket instead of costing memory.
    permits: Arc<Semaphore>,
    tx: mpsc::Sender<ServerMsg>,
    snaps: Arc<SnapshotSlot>,
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
                let permit = tokio::select! {
                    _ = self.cancel.cancelled() => return,
                    p = self.permits.clone().acquire_owned() => p.expect("semaphore never closed"),
                };
                let manager = self.manager.clone();
                let tx = self.tx.clone();
                let cancel = self.cancel.clone();
                let counted = self.in_flight.enter();
                // Detached on purpose: aborting a manager command half-way is never
                // wanted; if the client is gone the reply is simply discarded.
                tokio::spawn(async move {
                    let _held = (permit, counted);
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
            log_join(sub.await);
        }
        self.snaps.clear();
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
        Request::ListFiles { id } => ReplyData::Files {
            files: m.list_files(id).await?,
        },
        Request::SelectFiles { id, selection } => {
            m.select_files(id, selection).await.map(|()| Done)?
        }
        Request::SetSeed { id, policy } => m.set_seed(id, policy).await.map(|()| Done)?,
        Request::GetUpdate => ReplyData::Update {
            status: UpdateStatus::NotChecked,
        },
        Request::Update { .. } => {
            return Err(ManagerError::Unsupported(
                "updates are not enabled in this daemon".into(),
            ));
        }
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
    snaps: Arc<SnapshotSlot>,
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
                // A stalled client still has the last one: building another is wasted
                // work (and it would be just as stale by the time it is written).
                if !snaps.is_pending() {
                    let (downloads, totals) = manager.snapshot();
                    snaps.put(ServerMsg::Event {
                        event: Event::Snapshot { downloads, totals },
                    });
                }
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

/// Latest-value slot for a connection's snapshots. The writer takes the value out, so
/// "pending" means written by the subscription but not yet taken by the writer.
struct SnapshotSlot(watch::Sender<Option<ServerMsg>>);

impl SnapshotSlot {
    /// Replaces any pending snapshot and wakes the writer.
    fn put(&self, msg: ServerMsg) {
        self.0.send_replace(Some(msg));
    }

    /// Drops a pending snapshot.
    fn clear(&self) {
        self.0.send_if_modified(|v| {
            *v = None;
            false
        });
    }

    fn is_pending(&self) -> bool {
        self.0.borrow().is_some()
    }

    fn take(&self) -> Option<ServerMsg> {
        let mut out = None;
        self.0.send_if_modified(|v| {
            out = v.take();
            false
        });
        out
    }
}

/// Writes queued messages first, then the newest snapshot if it changed. Ends when
/// every queue sender is gone, on a write error (cancelling the connection), or on
/// `cancel` (then flushing what is queued for up to [`DRAIN_TIMEOUT`]).
async fn writer_task<W: AsyncWrite + Unpin>(
    mut wr: W,
    mut rx: mpsc::Receiver<ServerMsg>,
    slot: Arc<SnapshotSlot>,
    cancel: CancellationToken,
) {
    let mut snaps = slot.0.subscribe();
    loop {
        let msg = tokio::select! {
            biased;
            _ = cancel.cancelled() => break,
            m = rx.recv() => match m {
                Some(m) => m,
                None => return,
            },
            // Never fails: `slot` keeps the sender alive.
            _ = snaps.changed() => {
                match slot.take() {
                    Some(m) => m,
                    None => continue,
                }
            }
        };
        let write = write_one(&mut wr, &msg);
        tokio::pin!(write);
        let r = tokio::select! {
            r = &mut write => r,
            // Cancelled mid-write (e.g. the server is stopping right after queueing a
            // `Shutdown` reply): finish this message like the queued ones below.
            _ = cancel.cancelled() => match timeout(DRAIN_TIMEOUT, &mut write).await {
                Ok(r) => r,
                // Still stuck: the frame may be torn, so write nothing more.
                Err(_) => return,
            },
        };
        if r.is_err() {
            cancel.cancel();
            return;
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

#[cfg(test)]
mod tests {
    use super::*;

    fn snap() -> ServerMsg {
        ServerMsg::Event {
            event: Event::Snapshot {
                downloads: Vec::new(),
                totals: Default::default(),
            },
        }
    }

    #[test]
    fn only_the_same_user_is_served() {
        assert!(same_user(Some(1000), 1000));
        assert!(!same_user(Some(0), 1000));
        assert!(!same_user(Some(1001), 1000));
        // Unknown credentials are never trusted.
        assert!(!same_user(None, 1000));
    }

    #[tokio::test]
    async fn snapshot_slot_pending_until_taken() {
        let slot = SnapshotSlot(watch::channel(None).0);
        let mut rx = slot.0.subscribe();
        assert!(!slot.is_pending());
        slot.put(snap());
        assert!(slot.is_pending());
        rx.changed().await.unwrap();
        assert_eq!(slot.take(), Some(snap()));
        assert!(
            !slot.is_pending(),
            "taken snapshots no longer count as pending"
        );
        assert!(
            !rx.has_changed().unwrap(),
            "taking does not wake the writer"
        );
        slot.put(snap());
        slot.clear();
        assert!(!slot.is_pending());
        assert_eq!(slot.take(), None);
    }
}
