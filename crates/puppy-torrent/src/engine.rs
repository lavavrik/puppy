//! `TorrentEngine`: one librqbit [`Session`] configured for puppy behind the `Engine` trait.

use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddr};
use std::num::NonZeroU32;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

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
    AddRequest, AttentionReason, DownloadId, DownloadState, DownloadStatus, EngineDetail, Source,
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
    /// Seeding policy. `0.0` (the default): stop at completion. Reserved: any value
    /// above 0 currently means "keep seeding until removed" (no ratio tracking yet).
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
/// Restore found neither the stored `.torrent` copy nor the original file.
const TORRENT_MISSING: &str = "torrent file missing";

/// What the engine knows about one torrent besides librqbit's handle.
pub(crate) struct Entry {
    /// Lowercase hex info hash (duplicates are refused); empty if unknown (a restored
    /// torrent whose `.torrent` is gone).
    info_hash: String,
    /// The name until librqbit knows the torrent's own (magnet: `dn` or `magnet <hash>`).
    name: String,
    layout: Layout,
    /// The magnet link, to fetch the metadata again (resume after pause or failure).
    magnet: Option<String>,
    /// Per-download limit applied when the torrent is (re)added to librqbit.
    limit: Option<u64>,
    /// `limit` as a magnet's metadata task reads it just before the real add (0: none).
    limit_cell: Arc<AtomicU64>,
    /// The piece map, recomputed at most every [`DETAIL_TTL`].
    detail: Option<DetailCache>,
    /// Most bytes uploaded seen so far (librqbit forgets them once the torrent is paused).
    uploaded: u64,
    pub(crate) slot: Slot,
    /// The state last broadcast by the poll task.
    pub(crate) last_state: Option<DownloadState>,
    /// `Completed` was broadcast (and seeding stopped).
    pub(crate) completed_sent: bool,
    /// The torrent should be running: set by a non-paused add and by `resume`, cleared
    /// by `pause`. librqbit's own paused flag can disagree (see [`Shared::unstick`]).
    run_wanted: bool,
}

/// How long a synthesised piece map is reused by `detail`.
const DETAIL_TTL: Duration = Duration::from_secs(1);

/// A torrent's piece map as last computed.
struct DetailCache {
    at: Instant,
    piece_count: u32,
    have: Vec<u8>,
}

/// A bytes/s limit as librqbit takes it: `None`/0 is unlimited, above `u32::MAX` clamps.
fn bps_limit(bps: Option<u64>) -> Option<NonZeroU32> {
    bps.and_then(|l| NonZeroU32::new(u32::try_from(l).unwrap_or(u32::MAX)))
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
    /// Restored without its `.torrent` (stored copy and original both gone): nothing in
    /// librqbit; it cannot be resumed, only removed.
    Attention(AttentionReason),
}

type Torrents = HashMap<DownloadId, Entry>;

/// A torrent's status inputs, taken under the torrents lock (see [`Shared::view`]).
pub(crate) enum View {
    Pending(DownloadStatus),
    Managed {
        handle: ManagedTorrentHandle,
        name: String,
        path: PathBuf,
    },
}

impl View {
    /// The status (reads librqbit's stats: call without the torrents lock).
    pub(crate) fn status(self) -> DownloadStatus {
        match self {
            View::Pending(st) => st,
            View::Managed { handle, name, path } => {
                let stats = map::Stats::from(&handle.stats());
                let name = handle.name().unwrap_or(name);
                map::status(&stats, name, path)
            }
        }
    }
}

/// State shared by the engine, its poll task and the magnet tasks.
pub(crate) struct Shared {
    session: Arc<Session>,
    torrents: Mutex<Torrents>,
    pub(crate) events: broadcast::Sender<EngineEvent>,
    pub(crate) opts: TorrentEngineOptions,
    next_gen: AtomicU64,
    /// Serializes `pause`, `resume`, `remove` and [`Shared::unstick`], so a pause and an
    /// unstick never interleave between their check and their action.
    ops: tokio::sync::Mutex<()>,
}

impl Shared {
    pub(crate) fn lock(&self) -> MutexGuard<'_, Torrents> {
        self.torrents.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// What is needed to compute `e`'s status, taken under the torrents lock; librqbit's
    /// stats are then read outside it (`None` while its `.torrent` is being added).
    pub(crate) fn view(e: &Entry) -> Option<View> {
        let pending = |state| {
            Some(View::Pending(map::pending_status(
                state,
                e.name.clone(),
                e.layout.path.clone(),
            )))
        };
        match &e.slot {
            Slot::Adding => None,
            Slot::Resolving { .. } => pending(DownloadState::Starting),
            Slot::Held => pending(DownloadState::Paused),
            Slot::PendingFailed { reason, transient } => pending(DownloadState::Failed {
                reason: reason.clone(),
                transient: *transient,
            }),
            Slot::Attention(reason) => pending(DownloadState::NeedsAttention {
                reason: reason.clone(),
            }),
            Slot::Managed(h) => Some(View::Managed {
                handle: h.clone(),
                name: e.name.clone(),
                path: e.layout.path.clone(),
            }),
        }
    }

    /// Pauses a torrent in librqbit; already paused (or failed: nothing runs) is fine.
    /// A stuck torrent (librqbit state `Paused` with its paused flag cleared, see
    /// [`Shared::unstick`]) cannot be paused again (librqbit refuses: "already paused");
    /// the caller's cleared `Entry::run_wanted` keeps it stopped instead.
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

    /// Restarts a torrent left stopped against our wishes: one resumed while librqbit
    /// was still checking its data after a paused add. librqbit then clears its paused
    /// flag but its check task still ends in `Paused`, where it would stay. Acts only
    /// if the torrent should run (`run_wanted`: no `pause` since the `resume`), checked
    /// and acted on under the ops lock.
    pub(crate) async fn unstick(
        &self,
        id: DownloadId,
        h: &ManagedTorrentHandle,
    ) -> Result<(), EngineError> {
        let stuck = || {
            let wanted = self.lock().get(&id).is_some_and(|e| {
                e.run_wanted && matches!(&e.slot, Slot::Managed(x) if Arc::ptr_eq(x, h))
            });
            wanted && h.with_state(|s| matches!(s, ManagedTorrentState::Paused(_)))
        };
        // Cheap check first: the ops lock is only taken for a stuck torrent.
        if !stuck() {
            return Ok(());
        }
        let _ops = self.ops.lock().await;
        if !stuck() || h.stats().finished {
            return Ok(());
        }
        tracing::debug!(
            id,
            "restarting a torrent resumed while it was being checked"
        );
        self.session.unpause(h).await.map_err(lib_error)
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
                download_bps: bps_limit(limit),
                upload_bps: None,
            },
            initial_peers: (!peers.is_empty()).then(|| peers.clone()),
            ..Default::default()
        })
    }

    /// Starts fetching a magnet's metadata (then downloading) in the background; returns
    /// the slot to store for it. Call with the torrents lock held, so the task cannot
    /// look for its slot before it is stored.
    ///
    /// When the task ends: if its slot is still current it installs the torrent (or the
    /// failure); if the download was paused meanwhile (`Held`) it installs the torrent
    /// and pauses it; if the download was removed (or superseded) it returns the handle
    /// to whoever took the slot (`remove`/`pause` abort and await the task).
    fn spawn_resolve(
        self: &Arc<Self>,
        id: DownloadId,
        uri: String,
        opts: AddTorrentOptions,
        limit: Arc<AtomicU64>,
    ) -> Slot {
        let r#gen = self.next_gen.fetch_add(1, Ordering::Relaxed);
        let session = self.session.clone();
        let weak = Arc::downgrade(self);
        let task = tokio::spawn(async move {
            let outcome = resolve_and_add(&session, uri, opts, &limit).await;
            let added = outcome.as_ref().ok().cloned();
            let Some(shared) = weak.upgrade() else {
                return added;
            };
            let to_pause = {
                let mut map = shared.lock();
                let Some(e) = map.get_mut(&id) else {
                    return added; // removed: the remover deletes it
                };
                match (settle(&e.slot, r#gen), outcome) {
                    (Settle::Current, Ok(h)) => {
                        e.slot = Slot::Managed(h);
                        None
                    }
                    (Settle::Current, Err(f)) => {
                        tracing::info!(id, "{}", f.reason);
                        e.slot = Slot::PendingFailed {
                            reason: f.reason,
                            transient: f.transient,
                        };
                        None
                    }
                    (Settle::Paused, Ok(h)) => {
                        e.slot = Slot::Managed(h.clone());
                        Some(h)
                    }
                    (Settle::Paused, Err(_)) => None, // stays paused; resume retries
                    (Settle::Stale, _) => return added,
                }
            };
            if let Some(h) = to_pause
                && let Err(e) = shared.pause_managed(&h).await
            {
                tracing::warn!(id, "pausing a torrent paused while resolving: {e}");
            }
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

/// How a finished metadata task relates to its download's current slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Settle {
    /// Still the download's fetch: install the result.
    Current,
    /// Paused while fetching: keep the torrent, paused.
    Paused,
    /// Superseded by another fetch (or another state): hand the torrent back.
    Stale,
}

fn settle(slot: &Slot, task_gen: u64) -> Settle {
    match slot {
        Slot::Resolving { r#gen, .. } if *r#gen == task_gen => Settle::Current,
        Slot::Held => Settle::Paused,
        _ => Settle::Stale,
    }
}

/// Why a magnet could not be added.
struct ResolveFailure {
    reason: String,
    transient: bool,
}

impl ResolveFailure {
    fn new(reason: String) -> Self {
        let transient = map::is_transient_error(&reason);
        Self { reason, transient }
    }

    fn permanent(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
            transient: false,
        }
    }
}

/// Fetches a magnet's metadata without adding it (`list_only`: nothing is written),
/// checks its file names ([`source::validate_layout`]), then adds it for real from the
/// fetched torrent bytes (no second metadata fetch; the peers already seen are reused).
async fn resolve_and_add(
    session: &Arc<Session>,
    uri: String,
    mut opts: AddTorrentOptions,
    limit: &AtomicU64,
) -> Result<ManagedTorrentHandle, ResolveFailure> {
    let list_opts = AddTorrentOptions {
        list_only: true,
        output_folder: opts.output_folder.clone(),
        initial_peers: opts.initial_peers.clone(),
        ..Default::default()
    };
    let listed = match session
        .add_torrent(AddTorrent::from_url(uri), Some(list_opts))
        .await
    {
        Ok(AddTorrentResponse::ListOnly(listed)) => listed,
        Ok(_) => {
            return Err(ResolveFailure::permanent(
                "unexpected answer from librqbit to a list-only add",
            ));
        }
        Err(e) => {
            return Err(ResolveFailure::new(format!(
                "fetching the torrent metadata: {e:#}"
            )));
        }
    };
    source::validate_layout(&listed.info).map_err(|e| {
        ResolveFailure::permanent(match e {
            EngineError::Invalid(m) => m,
            other => other.to_string(),
        })
    })?;
    opts.only_files = listed.only_files;
    // A limit set while the metadata was being fetched.
    opts.ratelimits.download_bps = bps_limit(Some(limit.load(Ordering::Relaxed)));
    let mut peers = opts.initial_peers.take().unwrap_or_default();
    for p in listed.seen_peers {
        if !peers.contains(&p) {
            peers.push(p);
        }
    }
    opts.initial_peers = (!peers.is_empty()).then_some(peers);
    match session
        .add_torrent(AddTorrent::from_bytes(listed.torrent_bytes), Some(opts))
        .await
    {
        Ok(AddTorrentResponse::Added(_, h)) => Ok(h),
        Ok(AddTorrentResponse::AlreadyManaged(..)) => Err(ResolveFailure::permanent(DUPLICATE)),
        Ok(AddTorrentResponse::ListOnly(_)) => Err(ResolveFailure::permanent(
            "unexpected list-only answer from librqbit",
        )),
        Err(e) => Err(ResolveFailure::new(format!("{e:#}"))),
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

/// Releases a reserved id (and the stored `.torrent` copy, if `stored`) if `add` fails
/// or its future is dropped mid-await.
struct Reservation<'a> {
    shared: &'a Shared,
    id: DownloadId,
    stored: Option<PathBuf>,
    armed: bool,
}

impl Drop for Reservation<'_> {
    fn drop(&mut self) {
        if self.armed {
            self.shared.lock().remove(&self.id);
            if let Some(stored) = &self.stored {
                let _ = std::fs::remove_file(stored);
            }
        }
    }
}

/// Where an added torrent's files go.
#[derive(Clone, Copy)]
enum Target<'a> {
    /// A new download: into this output dir (its own folder unless single-file).
    Fresh(&'a Path),
    /// A restored download: back where its files were (its `Added::path`).
    Restore(&'a Path),
}

impl Target<'_> {
    fn layout(self, info: &source::TorrentInfo) -> Layout {
        match self {
            Target::Fresh(base) => Layout::for_torrent(base, info),
            Target::Restore(path) => Layout::restored(path, info.single_file.is_some()),
        }
    }
}

/// What `add` does with the stored `.torrent` copy.
#[derive(Clone, Copy, PartialEq, Eq)]
enum StoredCopy {
    /// Write it; delete it again if the add fails (a fresh add).
    Write,
    /// Write it (restored from the original: the copy was gone); keep it on failure.
    Rewrite,
    /// Restored from the copy: leave it as it is.
    Keep,
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
            ops: tokio::sync::Mutex::new(()),
        });
        tokio::spawn(poll::run(Arc::downgrade(&shared), cancel.clone()));
        Ok(Self { shared, cancel })
    }

    /// The session's current global `(download, upload)` limits in bytes/s (tests).
    #[doc(hidden)]
    pub fn global_limits(&self) -> (Option<u32>, Option<u32>) {
        let l = &self.shared.session.ratelimits;
        (
            l.get_download_bps().map(NonZeroU32::get),
            l.get_upload_bps().map(NonZeroU32::get),
        )
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
        target: Target<'_>,
        copy: StoredCopy,
        opts: &AddOptions,
    ) -> Result<Added, EngineError> {
        let info = source::parse_torrent(&bytes)?;
        let layout = target.layout(&info);
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
                    limit_cell: Arc::new(AtomicU64::new(opts.limit.unwrap_or(0))),
                    detail: None,
                    uploaded: 0,
                    slot: Slot::Adding,
                    last_state: None,
                    completed_sent: false,
                    run_wanted: !opts.start_paused,
                },
            );
        }
        let stored = self.shared.stored_torrent(id);
        let mut guard = Reservation {
            shared: &self.shared,
            id,
            stored: (copy == StoredCopy::Write).then(|| stored.clone()),
            armed: true,
        };
        if copy != StoredCopy::Keep {
            tokio::fs::write(&stored, &bytes).await?;
        }
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
        target: Target<'_>,
        opts: &AddOptions,
    ) -> Result<Added, EngineError> {
        let info = source::parse_magnet(&uri)?;
        let layout = match target {
            Target::Fresh(base) => Layout::for_magnet(base, &info),
            Target::Restore(path) => Layout::restored(path, false),
        };
        let add_opts = self.shared.add_options(&layout, false, opts.limit)?;
        let name = info.display_name();
        let mut map = self.shared.lock();
        check_free(&map, id, &info.info_hash)?;
        let limit_cell = Arc::new(AtomicU64::new(opts.limit.unwrap_or(0)));
        let slot = if opts.start_paused {
            Slot::Held
        } else {
            self.shared
                .spawn_resolve(id, uri.clone(), add_opts, limit_cell.clone())
        };
        map.insert(
            id,
            Entry {
                info_hash: info.info_hash,
                name: name.clone(),
                layout: layout.clone(),
                magnet: Some(uri),
                limit: opts.limit,
                limit_cell,
                detail: None,
                uploaded: 0,
                slot,
                last_state: None,
                completed_sent: false,
                run_wanted: !opts.start_paused,
            },
        );
        Ok(Added {
            name,
            path: layout.path,
            total_bytes: None,
        })
    }
}

impl TorrentEngine {
    /// Restores a download added before a restart, its files at `path`: a `.torrent`
    /// from its stored copy (else the original file, if still there), a magnet from its
    /// link (the metadata is fetched again). librqbit checks the existing data before
    /// the torrent goes on. With neither copy nor original, the download is registered
    /// as needing attention.
    async fn restore(
        &self,
        id: DownloadId,
        source: Source,
        path: &Path,
        opts: &AddOptions,
    ) -> Result<Added, EngineError> {
        let target = Target::Restore(path);
        let original = match source {
            Source::Magnet { uri } => return self.add_magnet(id, uri, target, opts),
            Source::TorrentFile { path } => path,
            Source::Http { .. } => return Err(EngineError::Unsupported("not a torrent source")),
        };
        let stored = self.shared.stored_torrent(id);
        if tokio::fs::try_exists(&stored).await.unwrap_or(false) {
            let bytes = source::read_torrent_file(&stored).await?;
            return self
                .add_torrent_file(id, bytes, target, StoredCopy::Keep, opts)
                .await;
        }
        if tokio::fs::try_exists(&original).await.unwrap_or(false) {
            tracing::info!(
                id,
                "stored .torrent copy missing; restoring from {}",
                original.display()
            );
            let bytes = source::read_torrent_file(&original).await?;
            return self
                .add_torrent_file(id, bytes, target, StoredCopy::Rewrite, opts)
                .await;
        }
        tracing::warn!(id, "cannot restore: the .torrent file is gone");
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.display().to_string());
        let layout = Layout::restored(path, false);
        let mut map = self.shared.lock();
        check_free(&map, id, "")?;
        map.insert(
            id,
            Entry {
                info_hash: String::new(),
                name: name.clone(),
                layout,
                magnet: None,
                limit: opts.limit,
                limit_cell: Arc::new(AtomicU64::new(opts.limit.unwrap_or(0))),
                detail: None,
                uploaded: 0,
                slot: Slot::Attention(AttentionReason::ValidationFailed {
                    detail: TORRENT_MISSING.into(),
                }),
                last_state: None,
                completed_sent: false,
                run_wanted: false,
            },
        );
        Ok(Added {
            name,
            path: path.to_owned(),
            total_bytes: None,
        })
    }
}

/// Refuses a taken id or an info hash already in the engine (an empty, unknown hash
/// only the id).
fn check_free(map: &Torrents, id: DownloadId, info_hash: &str) -> Result<(), EngineError> {
    if map.contains_key(&id) {
        return Err(EngineError::Invalid(format!("duplicate download id {id}")));
    }
    if !info_hash.is_empty() && map.values().any(|e| e.info_hash == info_hash) {
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
    ///
    /// `Restore { path }` re-adds the torrent so its files stay at `path` (see
    /// [`TorrentEngine::restore`]); a torrent whose `.torrent` is gone is `Ok`, needing
    /// attention.
    async fn add(
        &self,
        id: DownloadId,
        req: AddRequest,
        mode: StartMode,
        opts: AddOptions,
    ) -> Result<Added, EngineError> {
        if let StartMode::Restore { path } = mode {
            return self.restore(id, req.source, &path, &opts).await;
        }
        let base = req
            .output_dir
            .clone()
            .unwrap_or_else(|| self.shared.opts.default_dir.clone());
        let target = Target::Fresh(&base);
        match req.source {
            Source::TorrentFile { path } => {
                let bytes = source::read_torrent_file(&path).await?;
                self.add_torrent_file(id, bytes, target, StoredCopy::Write, &opts)
                    .await
            }
            Source::Magnet { uri } => self.add_magnet(id, uri, target, &opts),
            Source::Http { .. } => Err(EngineError::Unsupported("not a torrent source")),
        }
    }

    async fn pause(&self, id: DownloadId) -> Result<(), EngineError> {
        let _ops = self.shared.ops.lock().await;
        let taken = {
            let mut map = self.shared.lock();
            let e = map
                .get_mut(&id)
                .filter(|e| !matches!(e.slot, Slot::Adding))
                .ok_or(EngineError::NotFound(id))?;
            // Before acting: a stuck torrent (see `Shared::unstick`) is paused for
            // librqbit already, and must not be restarted.
            e.run_wanted = false;
            match std::mem::replace(&mut e.slot, Slot::Held) {
                Slot::Managed(h) => {
                    e.slot = Slot::Managed(h.clone());
                    Ok(h)
                }
                Slot::Resolving { task, .. } => Err(task),
                // A failed magnet stays failed (with its reason); nothing runs.
                other @ (Slot::Held
                | Slot::PendingFailed { .. }
                | Slot::Attention(_)
                | Slot::Adding) => {
                    e.slot = other;
                    return Ok(());
                }
            }
        };
        let task = match taken {
            Ok(h) => return self.shared.pause_managed(&h).await,
            Err(task) => task,
        };
        task.abort();
        // A task that finished meanwhile installed its torrent (paused) itself; it only
        // hands one back if the download was removed or superseded meanwhile: drop that
        // torrent from librqbit, keeping any files.
        if let Ok(Some(h)) = task.await {
            let _ = self
                .shared
                .session
                .delete(TorrentIdOrHash::Id(h.id()), false)
                .await;
        }
        // The abort may have interrupted the task's own pause of an installed torrent.
        let installed = match self.shared.lock().get(&id).map(|e| &e.slot) {
            Some(Slot::Managed(h)) => Some(h.clone()),
            _ => None,
        };
        if let Some(h) = installed {
            self.shared.pause_managed(&h).await?;
        }
        Ok(())
    }

    async fn resume(&self, id: DownloadId) -> Result<(), EngineError> {
        let _ops = self.shared.ops.lock().await;
        let h = {
            let mut map = self.shared.lock();
            let e = map
                .get_mut(&id)
                .filter(|e| !matches!(e.slot, Slot::Adding))
                .ok_or(EngineError::NotFound(id))?;
            if !matches!(e.slot, Slot::Attention(_)) {
                e.run_wanted = true;
            }
            match &e.slot {
                Slot::Managed(h) => h.clone(),
                Slot::Resolving { .. } => return Ok(()),
                Slot::Held | Slot::PendingFailed { .. } => {
                    let uri = e.magnet.clone().expect("only magnets wait for metadata");
                    let opts = self.shared.add_options(&e.layout, false, e.limit)?;
                    e.slot = self
                        .shared
                        .spawn_resolve(id, uri, opts, e.limit_cell.clone());
                    return Ok(());
                }
                Slot::Attention(_) => {
                    return Err(EngineError::Invalid(
                        "download needs attention and cannot be resumed as is".into(),
                    ));
                }
                Slot::Adding => return Err(EngineError::NotFound(id)),
            }
        };
        let st = map::Stats::from(&h.stats());
        // Finished: stays stopped (no seeding). Running or starting: nothing to do.
        let stopped = h.is_paused() || matches!(st.phase, map::Phase::Paused | map::Phase::Error);
        if st.finished || !stopped {
            return Ok(());
        }
        self.shared.session.unpause(&h).await.map_err(lib_error)
    }

    /// Removes the torrent from librqbit (deleting only its own files if asked: a
    /// multi-file torrent's or magnet's folder, a single-file torrent's file) and its
    /// stored `.torrent` copy. A magnet still fetching its metadata is cancelled.
    async fn remove(&self, id: DownloadId, delete_files: bool) -> Result<(), EngineError> {
        let _ops = self.shared.ops.lock().await;
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
            Slot::Attention(_) if delete_files => {
                tracing::warn!(
                    id,
                    path = %e.layout.path.display(),
                    "removed without its .torrent: its files are left on disk"
                );
                None
            }
            Slot::Held | Slot::PendingFailed { .. } | Slot::Attention(_) | Slot::Adding => None,
        };
        let deleted = match handle {
            Some(h) => {
                self.shared
                    .delete_managed(&h, &e.layout, delete_files)
                    .await
            }
            None => Ok(()),
        };
        // The download is gone either way: drop its stored copy before reporting.
        match std::fs::remove_file(self.shared.stored_torrent(id)) {
            Err(err) if err.kind() != std::io::ErrorKind::NotFound => {
                tracing::warn!(id, "removing the stored .torrent copy: {err}");
            }
            _ => {}
        }
        deleted
    }

    fn status(&self, id: DownloadId) -> Option<DownloadStatus> {
        let view = Shared::view(self.shared.lock().get(&id)?)?;
        Some(view.status())
    }

    /// The piece map is synthesised from per-file progress (see [`map::have_bitfield`]) and
    /// reused for a second. Seeds are connected peers that have every piece, leechers the
    /// other connected peers; the ratio is bytes uploaded since the torrent last went live
    /// (the highest seen) over its size. `None` until librqbit has the torrent's metadata.
    fn detail(&self, id: DownloadId) -> Option<EngineDetail> {
        let (h, cached) = {
            let map = self.shared.lock();
            let e = map.get(&id)?;
            let Slot::Managed(h) = &e.slot else {
                return None;
            };
            let cached = e
                .detail
                .as_ref()
                .filter(|c| c.at.elapsed() < DETAIL_TTL)
                .map(|c| (c.piece_count, c.have.clone()));
            (h.clone(), cached)
        };
        let stats = h.stats();
        // A map cached while downloading is stale once the torrent has finished.
        let cached = cached.filter(|(n, have)| !stats.finished || map::has_every_piece(*n, have));
        let fresh = match cached {
            Some(_) => None,
            None => {
                let (piece_len, total, pieces, lengths) = h
                    .with_metadata(|m| {
                        let l = m.lengths();
                        (
                            u64::from(l.default_piece_length()),
                            l.total_length(),
                            l.total_pieces(),
                            m.file_infos.iter().map(|f| f.len).collect::<Vec<_>>(),
                        )
                    })
                    .ok()?;
                // A finished torrent has every piece, whatever the progress says.
                let progress = if stats.finished {
                    &lengths
                } else {
                    &stats.file_progress
                };
                Some((
                    pieces,
                    map::have_bitfield(piece_len, total, &lengths, progress),
                ))
            }
        };
        let (piece_count, have) = match (cached, fresh) {
            (Some(c), _) => c,
            (None, Some(f)) => f,
            (None, None) => return None,
        };
        let uploaded = {
            let mut map = self.shared.lock();
            // Removed meanwhile: nothing to report.
            let e = map.get_mut(&id)?;
            e.uploaded = e.uploaded.max(stats.uploaded_bytes);
            if e.detail.as_ref().is_none_or(|c| {
                c.at.elapsed() >= DETAIL_TTL
                    || (stats.finished && !map::has_every_piece(c.piece_count, &c.have))
            }) {
                e.detail = Some(DetailCache {
                    at: Instant::now(),
                    piece_count,
                    have: have.clone(),
                });
            }
            e.uploaded
        };
        let (peers, seeds, leechers) = map::peer_counts(&stats);
        Some(EngineDetail::Torrent {
            piece_count,
            have,
            peers,
            seeds,
            leechers,
            ratio: map::ratio(uploaded, stats.total_bytes),
        })
    }

    /// librqbit takes a torrent's limit only when it is added: set while the torrent is
    /// still waiting for its metadata (or paused before it), it is applied then.
    fn set_download_limit(&self, id: DownloadId, bps: Option<u64>) -> Result<(), EngineError> {
        let mut map = self.shared.lock();
        let e = map
            .get_mut(&id)
            .filter(|e| !matches!(e.slot, Slot::Adding))
            .ok_or(EngineError::NotFound(id))?;
        match e.slot {
            Slot::Resolving { .. }
            | Slot::Held
            | Slot::PendingFailed { .. }
            | Slot::Attention(_) => {
                let bps = bps.filter(|&b| b > 0);
                e.limit = bps;
                e.limit_cell.store(bps.unwrap_or(0), Ordering::Relaxed);
                Ok(())
            }
            _ => Err(EngineError::Unsupported(
                "per-torrent limits can only be set before the torrent starts",
            )),
        }
    }

    fn set_global_download_budget(&self, bps: Option<u64>) {
        self.shared
            .session
            .ratelimits
            .set_download_bps(bps_limit(bps));
    }

    fn set_global_upload_budget(&self, bps: Option<u64>) {
        self.shared
            .session
            .ratelimits
            .set_upload_bps(bps_limit(bps));
    }

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

    #[tokio::test]
    async fn finished_metadata_task_settles_by_slot() {
        let task = || tokio::spawn(async { None });
        let resolving = |g| Slot::Resolving {
            r#gen: g,
            task: task(),
        };
        assert_eq!(settle(&resolving(3), 3), Settle::Current);
        assert_eq!(settle(&resolving(4), 3), Settle::Stale, "superseded fetch");
        assert_eq!(
            settle(&Slot::Held, 3),
            Settle::Paused,
            "paused while fetching"
        );
        let failed = Slot::PendingFailed {
            reason: "x".into(),
            transient: true,
        };
        assert_eq!(settle(&failed, 3), Settle::Stale);
        assert_eq!(settle(&Slot::Adding, 3), Settle::Stale);
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
