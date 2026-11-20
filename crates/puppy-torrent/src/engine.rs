//! `TorrentEngine`: one librqbit [`Session`] configured for puppy behind the `Engine` trait.

use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddr};
use std::num::NonZeroU32;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use librqbit::api::TorrentIdOrHash;
use librqbit::dht::DhtPersistenceConfig;
use librqbit::limits::LimitsConfig;
use librqbit::{
    AddTorrent, AddTorrentOptions, AddTorrentResponse, DhtSessionConfig, ListenerOptions,
    ManagedTorrent, ManagedTorrentState, Session, SessionOptions,
};
use puppy_core::engine::{
    AddOptions, Added, Engine, EngineError, EngineEvent, EngineKind, StartMode,
};
use puppy_core::types::{
    AddRequest, DownloadId, DownloadState, DownloadStatus, EngineDetail, Source,
};
use tokio::sync::broadcast;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::map;
use crate::poll;
use crate::source::{self, Layout};

pub(crate) type ManagedTorrentHandle = Arc<ManagedTorrent>;

/// How a [`TorrentEngine`] is set up (the daemon fills it from its `[torrent]` config).
#[derive(Debug, Clone)]
pub struct TorrentEngineOptions {
    /// Use the DHT to find peers (ignored in `test_mode`: always off).
    pub dht: bool,
    /// Ask the router to forward the listen port via UPnP (ignored in `test_mode`).
    pub upnp: bool,
    /// Port for incoming peer connections on `0.0.0.0`; an OS-assigned port is used if it
    /// cannot be bound. 0: always OS-assigned.
    pub listen_port: u16,
    /// Reserved: seeding stops at completion (ratio 0) in this version.
    pub seed_ratio: f32,
    /// puppy's data dir; the engine keeps its own files under `<data_dir>/torrent/` and
    /// copies of added `.torrent` files in `<data_dir>/torrents/<id>.torrent`.
    pub data_dir: PathBuf,
    /// Where torrents go when the request names no output dir.
    pub default_dir: PathBuf,
    /// Tests: DHT, trackers, LSD and UPnP off; listen on `127.0.0.1:0`.
    #[doc(hidden)]
    pub test_mode: bool,
    /// Tests: peers given to every added torrent (the local seeder).
    #[doc(hidden)]
    pub test_initial_peers: Vec<SocketAddr>,
}

/// Torrent engine: owns one librqbit [`Session`] (its own persistence off: puppy's store is
/// the source of truth, torrents are re-added on restore).
///
/// - `add` never waits for the network: a `.torrent` is handed to librqbit at once; a
///   magnet's metadata is fetched in the background (the download is `Starting` meanwhile
///   and can be paused or removed).
/// - Stop at completion (seeding policy v1): a finished torrent is paused in librqbit and
///   reported `Completed`.
/// - Unlike the HTTP engine, dropping it stops its torrents (the session goes with it).
pub struct TorrentEngine {
    shared: Arc<Shared>,
    /// The session's cancellation token: cancelling it stops every session task.
    cancel: CancellationToken,
}

/// Shown when a torrent (same info hash) is added twice.
const DUPLICATE: &str = "already downloading this torrent";

/// What the engine knows about one torrent besides librqbit's handle.
pub(crate) struct Entry {
    /// Lowercase hex info hash (duplicates are refused).
    info_hash: String,
    /// The name until librqbit knows the torrent's own (magnet: `dn` or `magnet <hash>`).
    name: String,
    layout: Layout,
    /// The magnet link, to fetch the metadata again (resume after pause or failure).
    magnet: Option<String>,
    /// Per-download limit applied when the torrent is (re)added to librqbit.
    limit: Option<u64>,
    pub(crate) slot: Slot,
    /// The state last broadcast by the poll task.
    pub(crate) last_state: Option<DownloadState>,
    /// `Completed` was broadcast (and seeding stopped).
    pub(crate) completed_sent: bool,
}

/// Where a torrent is in its life with librqbit.
pub(crate) enum Slot {
    /// `add` of a `.torrent` is in progress (the id and info hash are reserved).
    Adding,
    /// A magnet's metadata is being fetched by `task` (librqbit's `add_torrent` only
    /// returns once it has the metadata). The task installs the handle itself if its
    /// slot is still current (`r#gen`); otherwise it returns the handle to whoever took
    /// the slot (pause/remove abort and await it).
    Resolving {
        r#gen: u64,
        task: JoinHandle<Option<ManagedTorrentHandle>>,
    },
    /// A magnet paused before its metadata arrived (nothing in librqbit).
    Held,
    /// Fetching a magnet's metadata failed (nothing in librqbit); `resume` retries.
    PendingFailed { reason: String, transient: bool },
    /// In librqbit.
    Managed(ManagedTorrentHandle),
}

type Torrents = HashMap<DownloadId, Entry>;

/// State shared by the engine, its poll task and the magnet tasks.
pub(crate) struct Shared {
    session: Arc<Session>,
    torrents: Mutex<Torrents>,
    pub(crate) events: broadcast::Sender<EngineEvent>,
    pub(crate) opts: TorrentEngineOptions,
    next_gen: AtomicU64,
}

impl Shared {
    pub(crate) fn lock(&self) -> MutexGuard<'_, Torrents> {
        self.torrents.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The status of `e` (`None` while its `.torrent` is being added).
    pub(crate) fn entry_status(&self, e: &Entry) -> Option<DownloadStatus> {
        let pending = |state| {
            Some(map::pending_status(
                state,
                e.name.clone(),
                e.layout.path.clone(),
            ))
        };
        match &e.slot {
            Slot::Adding => None,
            Slot::Resolving { .. } => pending(DownloadState::Starting),
            Slot::Held => pending(DownloadState::Paused),
            Slot::PendingFailed { reason, transient } => pending(DownloadState::Failed {
                reason: reason.clone(),
                transient: *transient,
            }),
            Slot::Managed(h) => {
                let stats = map::Stats::from(&h.stats());
                let name = h.name().unwrap_or_else(|| e.name.clone());
                Some(map::status(&stats, name, e.layout.path.clone()))
            }
        }
    }

    /// Pauses a torrent in librqbit; already paused (or failed: nothing runs) is fine.
    pub(crate) async fn pause_managed(&self, h: &ManagedTorrentHandle) -> Result<(), EngineError> {
        let idle = h.is_paused()
            || h.with_state(|s| {
                matches!(
                    s,
                    ManagedTorrentState::Paused(_) | ManagedTorrentState::Error(_)
                )
            });
        if idle {
            return Ok(());
        }
        self.session.pause(h).await.map_err(lib_error)
    }

    /// librqbit add options for a torrent going to `layout`.
    fn add_options(
        &self,
        layout: &Layout,
        paused: bool,
        limit: Option<u64>,
    ) -> Result<AddTorrentOptions, EngineError> {
        let output_folder = layout
            .output_folder
            .to_str()
            .ok_or_else(|| {
                EngineError::Invalid(format!(
                    "output folder {} is not valid UTF-8",
                    layout.output_folder.display()
                ))
            })?
            .to_owned();
        let peers = &self.opts.test_initial_peers;
        Ok(AddTorrentOptions {
            paused,
            overwrite: true,
            output_folder: Some(output_folder),
            ratelimits: LimitsConfig {
                download_bps: limit
                    .and_then(|l| NonZeroU32::new(u32::try_from(l).unwrap_or(u32::MAX))),
                upload_bps: None,
            },
            initial_peers: (!peers.is_empty()).then(|| peers.clone()),
            ..Default::default()
        })
    }

    /// Starts fetching a magnet's metadata (then downloading) in the background; returns
    /// the slot to store for it. Call with the torrents lock held, so the task cannot
    /// look for its slot before it is stored.
    fn spawn_resolve(
        self: &Arc<Self>,
        id: DownloadId,
        uri: String,
        opts: AddTorrentOptions,
    ) -> Slot {
        let r#gen = self.next_gen.fetch_add(1, Ordering::Relaxed);
        let session = self.session.clone();
        let weak = Arc::downgrade(self);
        let task = tokio::spawn(async move {
            let res = session
                .add_torrent(AddTorrent::from_url(uri), Some(opts))
                .await;
            let added = match &res {
                Ok(AddTorrentResponse::Added(_, h)) => Some(h.clone()),
                _ => None,
            };
            let Some(shared) = weak.upgrade() else {
                return added;
            };
            let mut map = shared.lock();
            let current = map
                .get_mut(&id)
                .filter(|e| matches!(e.slot, Slot::Resolving { r#gen: g, .. } if g == r#gen));
            let Some(e) = current else {
                // Paused or removed meanwhile: whoever took the slot handles the torrent.
                return added;
            };
            e.slot = match res {
                Ok(AddTorrentResponse::Added(_, h)) => Slot::Managed(h),
                Ok(AddTorrentResponse::AlreadyManaged(..)) => Slot::PendingFailed {
                    reason: DUPLICATE.into(),
                    transient: false,
                },
                Ok(AddTorrentResponse::ListOnly(_)) => Slot::PendingFailed {
                    reason: "unexpected list-only answer from librqbit".into(),
                    transient: false,
                },
                Err(err) => {
                    let reason = format!("fetching the torrent metadata: {err:#}");
                    tracing::info!(id, "{reason}");
                    Slot::PendingFailed {
                        transient: map::is_transient_error(&reason),
                        reason,
                    }
                }
            };
            None
        });
        Slot::Resolving { r#gen, task }
    }

    /// Removes a torrent from librqbit, deleting its files if asked. A single-file
    /// torrent's output folder is the shared download dir: only its file is deleted
    /// (librqbit would also remove that dir if it ended up empty).
    async fn delete_managed(
        &self,
        h: &ManagedTorrentHandle,
        layout: &Layout,
        delete_files: bool,
    ) -> Result<(), EngineError> {
        let id = TorrentIdOrHash::Id(h.id());
        if delete_files && layout.single_file {
            self.session.delete(id, false).await.map_err(lib_error)?;
            match std::fs::remove_file(&layout.path) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.into()),
                _ => Ok(()),
            }
        } else {
            self.session
                .delete(id, delete_files)
                .await
                .map_err(lib_error)
        }
    }

    /// `<data_dir>/torrents/<id>.torrent`: the copy of an added `.torrent` (for restore).
    fn stored_torrent(&self, id: DownloadId) -> PathBuf {
        self.opts
            .data_dir
            .join("torrents")
            .join(format!("{id}.torrent"))
    }

    /// Aborts every metadata fetch (engine shutdown).
    fn abort_resolving(&self) {
        for e in self.lock().values() {
            if let Slot::Resolving { task, .. } = &e.slot {
                task.abort();
            }
        }
    }
}

/// A librqbit error as an engine error: I/O if an `io::Error` is behind it, else invalid.
fn lib_error(e: anyhow::Error) -> EngineError {
    let msg = format!("{e:#}");
    match e.chain().find_map(|c| c.downcast_ref::<std::io::Error>()) {
        Some(io) => EngineError::Io(std::io::Error::new(io.kind(), msg)),
        None => EngineError::Invalid(msg),
    }
}

/// Releases a reserved id (and the stored `.torrent` copy) if `add` fails or its future
/// is dropped mid-await.
struct Reservation<'a> {
    shared: &'a Shared,
    id: DownloadId,
    stored: PathBuf,
    armed: bool,
}

impl Drop for Reservation<'_> {
    fn drop(&mut self) {
        if self.armed {
            self.shared.lock().remove(&self.id);
            let _ = std::fs::remove_file(&self.stored);
        }
    }
}

impl TorrentEngine {
    /// Starts the librqbit session (listener, DHT unless disabled) and the poll task.
    pub async fn new(opts: TorrentEngineOptions) -> Result<Self, EngineError> {
        create_private_dir(&opts.data_dir.join("torrent"))?;
        create_private_dir(&opts.data_dir.join("torrents"))?;
        let plan = SessionPlan::from_options(&opts);
        let (session, cancel) = plan
            .start_with_fallback()
            .await
            .map_err(|e| EngineError::Network(format!("starting the torrent session: {e:#}")))?;
        tracing::info!(listen = ?session.listen_addr(), "torrent session started");
        let (events, _) = broadcast::channel(256);
        let shared = Arc::new(Shared {
            session,
            torrents: Mutex::new(HashMap::new()),
            events,
            opts,
            next_gen: AtomicU64::new(0),
        });
        tokio::spawn(poll::run(Arc::downgrade(&shared), cancel.clone()));
        Ok(Self { shared, cancel })
    }

    /// Where the session accepts incoming peer connections.
    pub fn listen_addr(&self) -> Option<SocketAddr> {
        self.shared.session.listen_addr()
    }

    /// Pauses every torrent and stops the session's tasks (listener, DHT, peers, poll).
    /// Takes about 1 s (librqbit gives its tasks a moment to wind down).
    pub async fn shutdown(&self) {
        self.shared.abort_resolving();
        self.shared.session.stop().await;
    }

    /// Adds a `.torrent` (already read) to librqbit.
    async fn add_torrent_file(
        &self,
        id: DownloadId,
        bytes: Vec<u8>,
        base: &Path,
        opts: &AddOptions,
    ) -> Result<Added, EngineError> {
        let info = source::parse_torrent(&bytes)?;
        let layout = Layout::for_torrent(base, &info);
        let add_opts = self
            .shared
            .add_options(&layout, opts.start_paused, opts.limit)?;
        {
            let mut map = self.shared.lock();
            check_free(&map, id, &info.info_hash)?;
            map.insert(
                id,
                Entry {
                    info_hash: info.info_hash.clone(),
                    name: info.name.clone(),
                    layout: layout.clone(),
                    magnet: None,
                    limit: opts.limit,
                    slot: Slot::Adding,
                    last_state: None,
                    completed_sent: false,
                },
            );
        }
        let stored = self.shared.stored_torrent(id);
        let mut guard = Reservation {
            shared: &self.shared,
            id,
            stored: stored.clone(),
            armed: true,
        };
        tokio::fs::write(&stored, &bytes).await?;
        let res = self
            .shared
            .session
            .add_torrent(AddTorrent::from_bytes(bytes), Some(add_opts))
            .await
            .map_err(lib_error)?;
        let handle = match res {
            AddTorrentResponse::Added(_, h) => h,
            AddTorrentResponse::AlreadyManaged(..) => {
                return Err(EngineError::Invalid(DUPLICATE.into()));
            }
            AddTorrentResponse::ListOnly(_) => {
                return Err(EngineError::Invalid(
                    "unexpected list-only answer from librqbit".into(),
                ));
            }
        };
        if let Some(e) = self.shared.lock().get_mut(&id) {
            e.slot = Slot::Managed(handle);
        }
        guard.armed = false;
        Ok(Added {
            name: info.name,
            path: layout.path,
            total_bytes: Some(info.total_bytes),
        })
    }

    /// Registers a magnet; its metadata is fetched in the background (unless paused).
    fn add_magnet(
        &self,
        id: DownloadId,
        uri: String,
        base: &Path,
        opts: &AddOptions,
    ) -> Result<Added, EngineError> {
        let info = source::parse_magnet(&uri)?;
        let layout = Layout::for_magnet(base, &info);
        let add_opts = self.shared.add_options(&layout, false, opts.limit)?;
        let name = info.display_name();
        let mut map = self.shared.lock();
        check_free(&map, id, &info.info_hash)?;
        let slot = if opts.start_paused {
            Slot::Held
        } else {
            self.shared.spawn_resolve(id, uri.clone(), add_opts)
        };
        map.insert(
            id,
            Entry {
                info_hash: info.info_hash,
                name: name.clone(),
                layout: layout.clone(),
                magnet: Some(uri),
                limit: opts.limit,
                slot,
                last_state: None,
                completed_sent: false,
            },
        );
        Ok(Added {
            name,
            path: layout.path,
            total_bytes: None,
        })
    }
}

/// Refuses a taken id or an info hash already in the engine.
fn check_free(map: &Torrents, id: DownloadId, info_hash: &str) -> Result<(), EngineError> {
    if map.contains_key(&id) {
        return Err(EngineError::Invalid(format!("duplicate download id {id}")));
    }
    if map.values().any(|e| e.info_hash == info_hash) {
        return Err(EngineError::Invalid(DUPLICATE.into()));
    }
    Ok(())
}

impl Drop for TorrentEngine {
    fn drop(&mut self) {
        // Unlike the HTTP engine, a dropped torrent engine stops its session: nothing else
        // could reach it, and a new engine (restore) must be able to take the files over.
        self.shared.abort_resolving();
        self.cancel.cancel();
    }
}

/// An engine dir (`<data_dir>/torrent/`: DHT state; `<data_dir>/torrents/`: stored
/// `.torrent` copies): owner-only on Unix.
fn create_private_dir(p: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(p)?;
        // Tighten the leaf even if it pre-existed with looser permissions.
        std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o700))
    }
    #[cfg(not(unix))]
    {
        std::fs::create_dir_all(p)
    }
}

/// What the session is started with, derived from [`TorrentEngineOptions`].
#[derive(Debug, Clone)]
struct SessionPlan {
    default_dir: PathBuf,
    listen_addr: SocketAddr,
    /// DHT state file; `None`: DHT off.
    dht_state: Option<PathBuf>,
    trackers: bool,
    lsd: bool,
    upnp: bool,
}

impl SessionPlan {
    fn from_options(opts: &TorrentEngineOptions) -> Self {
        let test = opts.test_mode;
        Self {
            default_dir: opts.default_dir.clone(),
            listen_addr: if test {
                (Ipv4Addr::LOCALHOST, 0).into()
            } else {
                (Ipv4Addr::UNSPECIFIED, opts.listen_port).into()
            },
            dht_state: (opts.dht && !test).then(|| opts.data_dir.join("torrent").join("dht.json")),
            trackers: !test,
            lsd: !test,
            upnp: opts.upnp && !test,
        }
    }

    /// Starts the session on the planned port; if that port can't be bound (in use, or
    /// privileged), once more on an OS-assigned one. Any other error is returned as is.
    async fn start_with_fallback(&self) -> anyhow::Result<(Arc<Session>, CancellationToken)> {
        self.start_recording_attempts(&mut Vec::new()).await
    }

    /// [`Self::start_with_fallback`], pushing each attempt's cancellation token to
    /// `attempts` (tests check failed attempts are cancelled).
    async fn start_recording_attempts(
        &self,
        attempts: &mut Vec<CancellationToken>,
    ) -> anyhow::Result<(Arc<Session>, CancellationToken)> {
        let mut attempt = |addr| {
            let cancel = CancellationToken::new();
            attempts.push(cancel.clone());
            self.start(addr, cancel)
        };
        match attempt(self.listen_addr).await {
            Err(e) if self.listen_addr.port() != 0 && is_port_unavailable(&e) => {
                tracing::warn!(
                    addr = %self.listen_addr,
                    "torrent listen port unavailable ({e:#}); using an OS-assigned port"
                );
                let mut any_port = self.listen_addr;
                any_port.set_port(0);
                attempt(any_port).await
            }
            r => r,
        }
    }

    /// One attempt, with its own cancellation token: cancelled if the attempt fails, so
    /// nothing it started (listener, DHT) outlives it.
    async fn start(
        &self,
        listen_addr: SocketAddr,
        cancel: CancellationToken,
    ) -> anyhow::Result<(Arc<Session>, CancellationToken)> {
        let dht = self.dht_state.as_ref().map(|state| DhtSessionConfig {
            bootstrap_addrs: None,
            port: None,
            // The DHT routing table (not torrents) is kept between runs for a quick bootstrap.
            persistence: Some(DhtPersistenceConfig {
                dump_interval: None,
                config_filename: Some(state.clone()),
            }),
        });
        let session_opts = SessionOptions {
            dht,
            disable_trackers: !self.trackers,
            disable_local_service_discovery: !self.lsd,
            persistence: None,
            fastresume: false,
            // IPv4 only (listen on 0.0.0.0; DHT/LSD on IPv4): works on IPv6-less hosts.
            ipv4_only: true,
            listen: Some(ListenerOptions {
                listen_addr,
                enable_upnp_port_forwarding: self.upnp,
                ipv4_only: true,
                ..Default::default()
            }),
            cancellation_token: Some(cancel.clone()),
            ..Default::default()
        };
        match Session::new_with_opts(self.default_dir.clone(), session_opts).await {
            Ok(session) => Ok((session, cancel)),
            Err(e) => {
                cancel.cancel();
                Err(e)
            }
        }
    }
}

/// Whether a session start failed because the listen port can't be bound. librqbit wraps
/// the socket error in `librqbit_dualstack_sockets::Error::Bind`, whose io::Error is not
/// exposed as a `source()`, so both forms are looked for along the chain.
fn is_port_unavailable(e: &anyhow::Error) -> bool {
    use std::io::ErrorKind::{AddrInUse, PermissionDenied};
    e.chain().any(|cause| {
        let io = cause.downcast_ref::<std::io::Error>().or_else(|| {
            match cause.downcast_ref::<librqbit_dualstack_sockets::Error>() {
                Some(librqbit_dualstack_sockets::Error::Bind(io)) => Some(io),
                _ => None,
            }
        });
        io.is_some_and(|io| matches!(io.kind(), AddrInUse | PermissionDenied))
    })
}

const NOT_YET: &str = "not implemented yet";

#[async_trait::async_trait]
impl Engine for TorrentEngine {
    fn kind(&self) -> EngineKind {
        EngineKind::Torrent
    }

    fn accepts(&self, source: &Source) -> bool {
        matches!(source, Source::Magnet { .. } | Source::TorrentFile { .. })
    }

    /// Returns without waiting for the network (see [`TorrentEngine`]). A `.torrent` is
    /// copied to `<data_dir>/torrents/<id>.torrent` for restore.
    async fn add(
        &self,
        id: DownloadId,
        req: AddRequest,
        mode: StartMode,
        opts: AddOptions,
    ) -> Result<Added, EngineError> {
        if let StartMode::Restore { .. } = mode {
            return Err(EngineError::Unsupported(
                "restoring torrents is not implemented yet",
            ));
        }
        let base = req
            .output_dir
            .clone()
            .unwrap_or_else(|| self.shared.opts.default_dir.clone());
        match req.source {
            Source::TorrentFile { path } => {
                let bytes = source::read_torrent_file(&path).await?;
                self.add_torrent_file(id, bytes, &base, &opts).await
            }
            Source::Magnet { uri } => self.add_magnet(id, uri, &base, &opts),
            Source::Http { .. } => Err(EngineError::Unsupported("not a torrent source")),
        }
    }

    async fn pause(&self, id: DownloadId) -> Result<(), EngineError> {
        let taken = {
            let mut map = self.shared.lock();
            let e = map
                .get_mut(&id)
                .filter(|e| !matches!(e.slot, Slot::Adding))
                .ok_or(EngineError::NotFound(id))?;
            match std::mem::replace(&mut e.slot, Slot::Held) {
                Slot::Managed(h) => {
                    e.slot = Slot::Managed(h.clone());
                    Ok(h)
                }
                Slot::Resolving { task, .. } => Err(task),
                Slot::Held | Slot::PendingFailed { .. } | Slot::Adding => return Ok(()),
            }
        };
        let task = match taken {
            Ok(h) => return self.shared.pause_managed(&h).await,
            Err(task) => task,
        };
        task.abort();
        // The metadata may have arrived just before the abort: keep that torrent, paused.
        if let Ok(Some(h)) = task.await {
            let kept = {
                let mut map = self.shared.lock();
                match map.get_mut(&id) {
                    Some(e) if matches!(e.slot, Slot::Held) => {
                        e.slot = Slot::Managed(h.clone());
                        true
                    }
                    _ => false,
                }
            };
            if kept {
                self.shared.pause_managed(&h).await?;
            } else {
                // Removed (or re-resumed) meanwhile: nobody wants this one.
                let _ = self
                    .shared
                    .session
                    .delete(TorrentIdOrHash::Id(h.id()), true)
                    .await;
            }
        }
        Ok(())
    }

    async fn resume(&self, id: DownloadId) -> Result<(), EngineError> {
        let h = {
            let mut map = self.shared.lock();
            let e = map
                .get_mut(&id)
                .filter(|e| !matches!(e.slot, Slot::Adding))
                .ok_or(EngineError::NotFound(id))?;
            match &e.slot {
                Slot::Managed(h) => h.clone(),
                Slot::Resolving { .. } => return Ok(()),
                Slot::Held | Slot::PendingFailed { .. } => {
                    let uri = e.magnet.clone().expect("only magnets wait for metadata");
                    let opts = self.shared.add_options(&e.layout, false, e.limit)?;
                    e.slot = self.shared.spawn_resolve(id, uri, opts);
                    return Ok(());
                }
                Slot::Adding => return Err(EngineError::NotFound(id)),
            }
        };
        let st = map::Stats::from(&h.stats());
        // Finished: stays stopped (no seeding). Running or starting: nothing to do.
        let stopped = h.is_paused() || st.phase == map::Phase::Error;
        if st.finished || !stopped {
            return Ok(());
        }
        self.shared.session.unpause(&h).await.map_err(lib_error)
    }

    /// Removes the torrent from librqbit (deleting only its own files if asked: a
    /// multi-file torrent's or magnet's folder, a single-file torrent's file) and its
    /// stored `.torrent` copy. A magnet still fetching its metadata is cancelled.
    async fn remove(&self, id: DownloadId, delete_files: bool) -> Result<(), EngineError> {
        let e = {
            let mut map = self.shared.lock();
            match map.get(&id) {
                Some(e) if !matches!(e.slot, Slot::Adding) => map.remove(&id),
                _ => None,
            }
        }
        .ok_or(EngineError::NotFound(id))?;
        let handle = match e.slot {
            Slot::Managed(h) => Some(h),
            Slot::Resolving { task, .. } => {
                task.abort();
                task.await.ok().flatten()
            }
            Slot::Held | Slot::PendingFailed { .. } | Slot::Adding => None,
        };
        if let Some(h) = handle {
            self.shared
                .delete_managed(&h, &e.layout, delete_files)
                .await?;
        }
        match std::fs::remove_file(self.shared.stored_torrent(id)) {
            Err(err) if err.kind() != std::io::ErrorKind::NotFound => {
                tracing::warn!(id, "removing the stored .torrent copy: {err}");
            }
            _ => {}
        }
        Ok(())
    }

    fn status(&self, id: DownloadId) -> Option<DownloadStatus> {
        let map = self.shared.lock();
        self.shared.entry_status(map.get(&id)?)
    }

    fn detail(&self, _id: DownloadId) -> Option<EngineDetail> {
        None
    }

    fn set_download_limit(&self, _id: DownloadId, _bps: Option<u64>) -> Result<(), EngineError> {
        Err(EngineError::Unsupported(NOT_YET))
    }

    fn set_global_download_budget(&self, _bps: Option<u64>) {}

    fn subscribe(&self) -> broadcast::Receiver<EngineEvent> {
        self.shared.events.subscribe()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    fn local_plan(dir: &Path, listen_addr: SocketAddr) -> SessionPlan {
        SessionPlan {
            default_dir: dir.to_owned(),
            listen_addr,
            dht_state: None,
            trackers: false,
            lsd: false,
            upnp: false,
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn busy_listen_port_falls_back_without_leaking() {
        let dir = tempfile::tempdir().unwrap();
        let busy = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let busy_addr = busy.local_addr().unwrap();

        let mut attempts = Vec::new();
        let (session, cancel) = local_plan(dir.path(), busy_addr)
            .start_recording_attempts(&mut attempts)
            .await
            .expect("falls back to an OS-assigned port");
        let got = session.listen_addr().unwrap();
        assert!(got.ip().is_loopback());
        assert_ne!(got.port(), busy_addr.port());

        // The failed attempt was torn down; the winner's token is the one returned.
        assert_eq!(attempts.len(), 2);
        assert!(attempts[0].is_cancelled(), "failed attempt left running");
        assert!(!attempts[1].is_cancelled());
        assert!(!cancel.is_cancelled());

        // Nothing from the failed attempt holds the busy port once its owner lets go.
        drop(busy);
        assert!(
            TcpListener::bind(busy_addr).is_ok(),
            "{busy_addr} held by a leaked listener"
        );

        // Shutdown leaves no attempt running.
        session.stop().await;
        assert!(attempts.iter().all(|t| t.is_cancelled()));
    }

    /// A free loopback port (bound, then released).
    fn free_port() -> SocketAddr {
        TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .unwrap()
            .local_addr()
            .unwrap()
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn other_start_errors_are_not_retried_and_torn_down() {
        // The listener binds fine, then the DHT fails deterministically (its state file's
        // parent is a regular file) before it touches the network: not a port problem.
        let dir = tempfile::tempdir().unwrap();
        let not_a_dir = dir.path().join("file");
        std::fs::write(&not_a_dir, b"x").unwrap();
        let addr = free_port();
        let plan = SessionPlan {
            dht_state: Some(not_a_dir.join("dht.json")),
            ..local_plan(dir.path(), addr)
        };

        let mut attempts = Vec::new();
        let err = plan
            .start_recording_attempts(&mut attempts)
            .await
            .err()
            .expect("start fails");
        assert!(!is_port_unavailable(&err), "{err:#}");
        assert_eq!(attempts.len(), 1, "a non-port error is not retried");
        assert!(attempts[0].is_cancelled(), "failed attempt left running");
        // The attempt's listener is gone.
        let rebound = (0..20).any(|_| {
            let ok = TcpListener::bind(addr).is_ok();
            if !ok {
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            ok
        });
        assert!(rebound, "{addr} still held by the failed attempt");
    }

    #[test]
    fn port_unavailable_detection() {
        use std::io::{Error, ErrorKind};
        let wrapped = |kind| {
            anyhow::Error::from(librqbit_dualstack_sockets::Error::Bind(Error::from(kind)))
                .context("error starting TCP listener")
                .context("error starting listeners")
        };
        assert!(is_port_unavailable(&wrapped(ErrorKind::AddrInUse)));
        assert!(is_port_unavailable(&wrapped(ErrorKind::PermissionDenied)));
        assert!(!is_port_unavailable(&wrapped(ErrorKind::AddrNotAvailable)));
        assert!(is_port_unavailable(&anyhow::Error::from(Error::from(
            ErrorKind::AddrInUse
        ))));
        assert!(!is_port_unavailable(&anyhow::anyhow!(
            "error initializing DHT"
        )));
    }
}
