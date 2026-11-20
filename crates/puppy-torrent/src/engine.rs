//! `TorrentEngine`: one librqbit [`Session`] configured for puppy behind the `Engine` trait.

use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddr};
use std::num::NonZeroU32;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use librqbit::api::TorrentIdOrHash;
use librqbit::dht::{DhtPersistenceConfig, Id20};
use librqbit::limits::LimitsConfig;
use librqbit::{
    AddTorrent, AddTorrentOptions, AddTorrentResponse, DhtSessionConfig, ListenerOptions,
    ManagedTorrent, ManagedTorrentState, Session, SessionOptions,
};
use puppy_core::engine::{
    AddOptions, Added, Engine, EngineError, EngineEvent, EngineKind, StartMode,
};
use puppy_core::types::{
    AddRequest, AttentionReason, DownloadId, DownloadState, DownloadStatus, EngineDetail,
    FileChoice, Selection, Source, TorrentFile,
};
use tokio::sync::{OnceCell, broadcast};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::map;
use crate::poll;
use crate::selection::{self, Chosen, Decision, Sidecar};
use crate::source::{self, Layout, TorrentInfo};

pub(crate) type ManagedTorrentHandle = Arc<ManagedTorrent>;

/// How a [`TorrentEngine`] is set up (the daemon fills it from its `[torrent]` config).
#[derive(Debug, Clone)]
pub struct TorrentEngineOptions {
    /// Use the DHT to find peers (ignored in `test_mode`: always off).
    pub dht: bool,
    /// Ask the router to forward the listen port via UPnP (ignored in `test_mode`).
    pub upnp: bool,
    /// Find peers on the local network (local service discovery; ignored in
    /// `test_mode`: always off).
    pub lsd: bool,
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
/// - The session (listener, DHT, its state dirs) starts with the first torrent that
///   needs it, not with the engine: a daemon that only downloads over HTTP opens no
///   torrent sockets. If it cannot start, the add fails transiently ("torrent engine
///   unavailable (see daemon log)") and the next attempt tries again.
/// - `add` never waits for the network: a `.torrent` is handed to librqbit at once; a
///   magnet's metadata is fetched in the background (the download is `Starting` meanwhile
///   and can be paused or removed).
/// - Stop at completion (seeding policy v1): a finished torrent is paused in librqbit and
///   reported `Completed`.
/// - Unlike the HTTP engine, dropping it stops its torrents (the session goes with it).
/// - File selection (once, when the contents are known: at add for a `.torrent`, after
///   the metadata for a magnet): `FileChoice::Ask` on a multi-file torrent, or a magnet
///   whose pre-selection does not fit, waits in `ChoosingFiles` with nothing in librqbit
///   and nothing in the download dir; `select_files` then adds it with `only_files`. The
///   choice is kept in `<data_dir>/torrents/<id>.selection` (restore, delete). Sizes
///   and progress count the selected files only.
/// - librqbit 9.1 still opens (creates) every file of a torrent, selected or not:
///   unselected files appear as empty placeholders, holding only the part of a piece
///   they share with a selected file. Deleting removes the selected files and the
///   placeholders this download created, never an unselected file that was there before.
pub struct TorrentEngine {
    shared: Arc<Shared>,
}

/// Shown when a torrent (same info hash) is added twice.
const DUPLICATE: &str = "already downloading this torrent";
/// Restore found neither the stored `.torrent` copy nor the original file.
const TORRENT_MISSING: &str = "torrent file missing";
/// The session could not be started (the details are logged).
const UNAVAILABLE: &str = "torrent engine unavailable (see daemon log)";
/// A fresh add found a file of the torrent already there (librqbit does not overwrite).
const EXISTS: &str = "refusing to overwrite: a file of this torrent already exists";

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
    /// librqbit may write over existing files (restored downloads only: their files are
    /// ours). Fresh adds never do.
    overwrite: bool,
    /// Per-download limit applied when the torrent is (re)added to librqbit.
    limit: Option<u64>,
    /// `limit` as a magnet's metadata task reads it just before the real add (0: none).
    limit_cell: Arc<AtomicU64>,
    /// The piece map, recomputed at most every [`DETAIL_TTL`].
    detail: Option<DetailCache>,
    /// Most bytes uploaded seen so far (librqbit forgets them once the torrent is paused).
    uploaded: u64,
    /// The torrent's contents, once known (`.torrent`: at add; magnet: once its metadata
    /// has been fetched).
    info: Option<Arc<TorrentInfo>>,
    /// What to do about file selection when a magnet's metadata arrives.
    choice: FileChoice,
    /// The chosen files; `None`: every file (or not chosen yet).
    selected: Option<Arc<Chosen>>,
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
    /// The contents are known and the user must choose files (`Entry::info`): nothing in
    /// librqbit, nothing written but the stored `.torrent` copy and the `.selection`
    /// sidecar. `select_files` adds the torrent for real.
    Choosing {
        bytes: bytes::Bytes,
        /// A magnet's `tr`s (its fetched metadata has none).
        trackers: Vec<String>,
        /// Why a choice given up front could not be used.
        reason: Option<String>,
    },
}

type Torrents = HashMap<DownloadId, Entry>;

/// A torrent's status inputs, taken under the torrents lock (see [`Shared::view`]).
pub(crate) enum View {
    Pending(DownloadStatus),
    Managed {
        handle: ManagedTorrentHandle,
        name: String,
        path: PathBuf,
        selected: Option<Arc<Chosen>>,
    },
}

impl View {
    /// The status (reads librqbit's stats: call without the torrents lock).
    pub(crate) fn status(self) -> DownloadStatus {
        match self {
            View::Pending(st) => st,
            View::Managed {
                handle,
                name,
                path,
                selected,
            } => {
                let mut stats = map::Stats::from(&handle.stats());
                if let Some(c) = &selected {
                    stats.restrict(&c.lib_sizes);
                }
                let name = handle.name().unwrap_or(name);
                map::status(&stats, name, path)
            }
        }
    }
}

/// State shared by the engine, its poll task and the magnet tasks.
pub(crate) struct Shared {
    /// Started by the first torrent that needs it (see [`Shared::session`]).
    session: OnceCell<Arc<Session>>,
    /// The global `(download, upload)` limits, applied to the session (also when it
    /// starts).
    limits: Mutex<(Option<u64>, Option<u64>)>,
    /// The engine's token: the session's tokens are its children, so dropping the
    /// engine stops the session and the poll task.
    cancel: CancellationToken,
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

    /// The session, started now if it is not yet (creating the engine's dirs, then
    /// listening). A failure is logged and reported as [`UNAVAILABLE`] (transient); it is
    /// not remembered: the next call tries again.
    async fn session(&self) -> Result<&Arc<Session>, EngineError> {
        let session = self
            .session
            .get_or_try_init(|| self.start_session())
            .await
            .map_err(|e| {
                tracing::error!("cannot start the torrent session: {e:#}");
                EngineError::Unavailable(UNAVAILABLE.into())
            })?;
        // Also covers limits set while the session was starting.
        self.apply_limits(session);
        Ok(session)
    }

    async fn start_session(&self) -> anyhow::Result<Arc<Session>> {
        create_private_dir(&self.opts.data_dir.join("torrent"))?;
        create_private_dir(&self.opts.data_dir.join("torrents"))?;
        let session = SessionPlan::from_options(&self.opts)
            .start_with_fallback(&self.cancel)
            .await
            .map_err(|e| e.context("starting the torrent session"))?;
        tracing::info!(listen = ?session.listen_addr(), "torrent session started");
        Ok(session)
    }

    /// The session if it has been started.
    fn started(&self) -> Option<&Arc<Session>> {
        self.session.get()
    }

    /// The session, for a torrent already in it (so it has been started).
    fn live(&self) -> Result<&Arc<Session>, EngineError> {
        self.started()
            .ok_or_else(|| EngineError::Invalid("the torrent session is not running".into()))
    }

    fn apply_limits(&self, session: &Session) {
        let (down, up) = *self.limits.lock().unwrap_or_else(|e| e.into_inner());
        session.ratelimits.set_download_bps(bps_limit(down));
        session.ratelimits.set_upload_bps(bps_limit(up));
    }

    /// Records a global limit and applies it if the session runs.
    fn set_limit(&self, f: impl FnOnce(&mut (Option<u64>, Option<u64>))) {
        f(&mut self.limits.lock().unwrap_or_else(|e| e.into_inner()));
        if let Some(s) = self.started() {
            self.apply_limits(s);
        }
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
            Slot::Choosing { .. } => {
                let mut st = map::pending_status(
                    DownloadState::ChoosingFiles,
                    e.name.clone(),
                    e.layout.path.clone(),
                );
                st.total_bytes = e.info.as_ref().map(|i| i.total_bytes);
                Some(View::Pending(st))
            }
            Slot::Managed(h) => Some(View::Managed {
                handle: h.clone(),
                name: e.name.clone(),
                path: e.layout.path.clone(),
                selected: e.selected.clone(),
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
        self.live()?.pause(h).await.map_err(lib_error)
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
        self.live()?.unpause(h).await.map_err(lib_error)
    }

    /// librqbit add options for a torrent going to `layout`. `overwrite`: only for
    /// restored torrents (see [`Entry::overwrite`]); `trackers`: a magnet's `tr`s (its
    /// fetched metadata has none).
    fn add_options(
        &self,
        layout: &Layout,
        paused: bool,
        limit: Option<u64>,
        overwrite: bool,
        trackers: Vec<String>,
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
            overwrite,
            output_folder: Some(output_folder),
            trackers: (!trackers.is_empty()).then_some(trackers),
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
        choice: FileChoice,
    ) -> Slot {
        let r#gen = self.next_gen.fetch_add(1, Ordering::Relaxed);
        let weak = Arc::downgrade(self);
        let files = SideFiles {
            stored: self.stored_torrent(id),
            selection: self.selection_file(id),
        };
        let trackers = source::parse_magnet(&uri)
            .map(|m| m.trackers)
            .unwrap_or_default();
        let task = tokio::spawn(async move {
            let session = match weak.upgrade() {
                Some(shared) => shared.session().await.cloned(),
                None => return None,
            };
            let outcome = match session {
                Ok(session) => resolve_and_add(&session, uri, opts, &limit, &files, &choice).await,
                Err(e) => Err(ResolveFailure {
                    reason: e.to_string(),
                    transient: e.is_transient(),
                }),
            };
            let added = match &outcome {
                Ok(Resolved::Added { handle, .. }) => Some(handle.clone()),
                _ => None,
            };
            let Some(shared) = weak.upgrade() else {
                return added;
            };
            let to_pause = {
                let mut map = shared.lock();
                let Some(e) = map.get_mut(&id) else {
                    return added; // removed: the remover deletes it
                };
                match (settle(&e.slot, r#gen), outcome) {
                    (
                        Settle::Current | Settle::Paused,
                        Ok(Resolved::Choosing {
                            info,
                            bytes,
                            reason,
                        }),
                    ) => {
                        if let Some(r) = &reason {
                            tracing::info!(id, "choose files: {r}");
                        }
                        e.name = info.name.clone();
                        e.info = Some(info);
                        e.slot = Slot::Choosing {
                            bytes,
                            trackers,
                            reason,
                        };
                        None
                    }
                    (
                        Settle::Current,
                        Ok(Resolved::Added {
                            handle,
                            info,
                            chosen,
                        }),
                    ) => {
                        e.info = Some(info);
                        e.selected = chosen;
                        e.slot = Slot::Managed(handle);
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
                    (
                        Settle::Paused,
                        Ok(Resolved::Added {
                            handle,
                            info,
                            chosen,
                        }),
                    ) => {
                        e.info = Some(info);
                        e.selected = chosen;
                        e.slot = Slot::Managed(handle.clone());
                        Some(handle)
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
    ///
    /// With a file selection (`chosen`, `info`), librqbit would delete every file of the
    /// torrent: puppy deletes the selected files and the unselected ones librqbit created
    /// itself ([`Chosen::deletable`]), with the rules of [`source::delete_listed`].
    async fn delete_managed(
        &self,
        h: &ManagedTorrentHandle,
        layout: &Layout,
        delete_files: bool,
        chosen: Option<(&Chosen, &TorrentInfo)>,
    ) -> Result<(), EngineError> {
        let id = TorrentIdOrHash::Id(h.id());
        let session = self.live()?;
        if let (true, Some((c, info))) = (delete_files, chosen) {
            session.delete(id, false).await.map_err(lib_error)?;
            let output = layout.output_folder.clone();
            let keep = output
                .parent()
                .map(Path::to_owned)
                .unwrap_or_else(|| output.clone());
            let files = c.deletable(info);
            tokio::task::spawn_blocking(move || source::delete_listed(&output, &files, &keep))
                .await
                .map_err(std::io::Error::other)??;
            Ok(())
        } else if delete_files && layout.single_file {
            session.delete(id, false).await.map_err(lib_error)?;
            match std::fs::remove_file(&layout.path) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.into()),
                _ => Ok(()),
            }
        } else {
            session.delete(id, delete_files).await.map_err(lib_error)
        }
    }

    /// Re-adds download `id` (failed in librqbit: `h`) from its stored `.torrent` copy,
    /// allowed to write over existing files, at the same layout and limit, and installs
    /// the new handle. See [`readd_on_resume`]. Call with the ops lock held.
    async fn readd_overwriting(
        &self,
        id: DownloadId,
        h: &ManagedTorrentHandle,
    ) -> Result<(), EngineError> {
        let (layout, limit, magnet, only_files) = {
            let map = self.lock();
            let e = map.get(&id).ok_or(EngineError::NotFound(id))?;
            (e.layout.clone(), e.limit, e.magnet.clone(), only_files(e))
        };
        let bytes = source::read_torrent_file(&self.stored_torrent(id)).await?;
        let trackers = match &magnet {
            Some(uri) => source::parse_magnet(uri)?.trackers,
            None => Vec::new(),
        };
        let mut opts = self.add_options(&layout, false, limit, true, trackers)?;
        opts.only_files = only_files;
        let session = self.live()?;
        tracing::info!(id, "restarting a failed torrent over its own files");
        // Already gone (an earlier attempt) is fine: the add below retries.
        if let Err(e) = session.delete(TorrentIdOrHash::Id(h.id()), false).await {
            tracing::debug!(id, "dropping the failed torrent: {e:#}");
        }
        let nh = match session
            .add_torrent(AddTorrent::from_bytes(bytes), Some(opts))
            .await
            .map_err(lib_error)?
        {
            AddTorrentResponse::Added(_, nh) => nh,
            AddTorrentResponse::AlreadyManaged(..) => {
                return Err(EngineError::Invalid(DUPLICATE.into()));
            }
            AddTorrentResponse::ListOnly(_) => {
                return Err(EngineError::Invalid(
                    "unexpected list-only answer from librqbit".into(),
                ));
            }
        };
        let installed = match self.lock().get_mut(&id) {
            Some(e) => {
                e.slot = Slot::Managed(nh.clone());
                e.overwrite = true;
                e.detail = None;
                true
            }
            None => false,
        };
        if !installed {
            // Removed meanwhile (cannot happen under the ops lock): keep nothing running.
            let _ = session.delete(TorrentIdOrHash::Id(nh.id()), false).await;
        }
        Ok(())
    }

    /// After a metadata task was aborted (pause or remove): a torrent it may have added
    /// to librqbit just before the abort (and not handed back) is settled like a
    /// finished task would be ([`settle`]): adopted, paused, if download `id` is still
    /// waiting for it (`Held`); otherwise dropped from librqbit, keeping its files,
    /// unless another download owns that torrent.
    async fn settle_slipped(&self, id: DownloadId, info_hash: &str) {
        let Some(session) = self.started() else {
            return;
        };
        let Ok(hash) = Id20::from_str(info_hash) else {
            return;
        };
        let Some(h) = session.get(TorrentIdOrHash::Hash(hash)) else {
            return;
        };
        let adopt = {
            let mut map = self.lock();
            let owned = map.iter().any(|(other, e)| {
                (*other != id && e.info_hash == info_hash)
                    || matches!(&e.slot, Slot::Managed(x) if Arc::ptr_eq(x, &h))
            });
            if owned {
                return;
            }
            match map.get_mut(&id) {
                // Paused while fetching (see `settle`): keep the torrent, paused.
                Some(e) if e.info_hash == info_hash && matches!(e.slot, Slot::Held) => {
                    e.slot = Slot::Managed(h.clone());
                    true
                }
                _ => false,
            }
        };
        if adopt {
            tracing::debug!(id, "adopting a torrent added while its fetch was aborted");
            let bytes = h.with_metadata(|m| m.torrent_bytes.clone());
            if let Ok(bytes) = bytes {
                self.write_stored(id, &bytes);
                // The fetch may have chosen files (and recorded them) before it added.
                if let Ok(info) = source::parse_torrent(&bytes) {
                    let info = Arc::new(info);
                    let chosen = match Sidecar::read(&self.selection_file(id)) {
                        Some(Sidecar::Selected { indices, created }) => {
                            Some(Arc::new(Chosen::new(&info, indices, created)))
                        }
                        _ => None,
                    };
                    if let Some(e) = self.lock().get_mut(&id) {
                        e.info = Some(info);
                        e.selected = chosen;
                    }
                }
            }
            if let Err(e) = self.pause_managed(&h).await {
                tracing::warn!(id, "pausing an adopted torrent: {e}");
            }
        } else {
            tracing::debug!(id, "dropping a torrent added while its fetch was aborted");
            if let Err(e) = session.delete(TorrentIdOrHash::Id(h.id()), false).await {
                tracing::warn!(id, "dropping a torrent left by an aborted fetch: {e:#}");
            }
        }
    }

    /// The file selection recorded for download `id`, with `h`'s contents (`None`: every
    /// file).
    fn recorded_choice(
        &self,
        id: DownloadId,
        h: &ManagedTorrentHandle,
    ) -> Option<(Chosen, TorrentInfo)> {
        let Some(Sidecar::Selected { indices, created }) = Sidecar::read(&self.selection_file(id))
        else {
            return None;
        };
        let bytes = h.with_metadata(|m| m.torrent_bytes.clone()).ok()?;
        let info = source::parse_torrent(&bytes).ok()?;
        Some((Chosen::new(&info, indices, created), info))
    }

    /// Writes the stored `.torrent` copy of download `id` (a failure is only logged:
    /// restoring then falls back to the original source).
    fn write_stored(&self, id: DownloadId, bytes: &[u8]) {
        if let Err(e) = write_stored_copy(&self.stored_torrent(id), bytes) {
            tracing::warn!(id, "keeping the torrent's metadata: {e}");
        }
    }

    /// `<data_dir>/torrents/<id>.torrent`: the copy of an added `.torrent` (for restore).
    fn stored_torrent(&self, id: DownloadId) -> PathBuf {
        self.opts
            .data_dir
            .join("torrents")
            .join(format!("{id}.torrent"))
    }

    /// `<data_dir>/torrents/<id>.placement`: how download `id`'s files were placed
    /// ([`source::Placement`]), for [`Engine::delete_completed`]. Absent: its own folder.
    fn placement_marker(&self, id: DownloadId) -> PathBuf {
        self.opts
            .data_dir
            .join("torrents")
            .join(format!("{id}.placement"))
    }

    /// `<data_dir>/torrents/<id>.selection`: download `id`'s file choice ([`Sidecar`]).
    fn selection_file(&self, id: DownloadId) -> PathBuf {
        self.opts
            .data_dir
            .join("torrents")
            .join(format!("{id}.selection"))
    }

    /// The recorded placement of download `id`'s files (none recorded: its own folder,
    /// as for every magnet).
    async fn placement(&self, id: DownloadId) -> source::Placement {
        match tokio::fs::read_to_string(self.placement_marker(id)).await {
            Ok(s) => source::Placement::parse(&s),
            Err(_) => source::Placement::Folder,
        }
    }

    /// Deletes the stored `.torrent` copy of download `id`, its placement marker and its
    /// selection sidecar, if any.
    fn forget(&self, id: DownloadId) {
        for path in [
            self.stored_torrent(id),
            self.placement_marker(id),
            self.selection_file(id),
        ] {
            match std::fs::remove_file(&path) {
                Err(err) if err.kind() != std::io::ErrorKind::NotFound => {
                    tracing::warn!(id, "removing {}: {err}", path.display());
                }
                _ => {}
            }
        }
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

/// Where a magnet's metadata task keeps what it learns.
struct SideFiles {
    /// The stored `.torrent` copy.
    stored: PathBuf,
    /// The `.selection` sidecar.
    selection: PathBuf,
}

/// What a magnet's metadata task ends with.
enum Resolved {
    /// Added to librqbit.
    Added {
        handle: ManagedTorrentHandle,
        info: Arc<TorrentInfo>,
        chosen: Option<Arc<Chosen>>,
    },
    /// The user must choose files first: nothing added (the stored copy is written).
    Choosing {
        info: Arc<TorrentInfo>,
        bytes: bytes::Bytes,
        reason: Option<String>,
    },
}

/// Fetches a magnet's metadata without adding it (`list_only`: nothing is written),
/// checks its file names ([`source::validate_layout`]), applies the file `choice`, then
/// adds it for real from the fetched torrent bytes (no second metadata fetch; the peers
/// already seen are reused). If the user must choose files ([`Decision::Choose`]) nothing
/// is added: the metadata is kept and the task ends. The metadata is kept at
/// `files.stored` once added or choosing (restore needs no network then, and knows the
/// torrent's name and files).
async fn resolve_and_add(
    session: &Arc<Session>,
    uri: String,
    mut opts: AddTorrentOptions,
    limit: &AtomicU64,
    files: &SideFiles,
    choice: &FileChoice,
) -> Result<Resolved, ResolveFailure> {
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
    let invalid = |e: EngineError| {
        ResolveFailure::permanent(match e {
            EngineError::Invalid(m) => m,
            other => other.to_string(),
        })
    };
    source::validate_layout(&listed.info).map_err(invalid)?;
    let bytes = listed.torrent_bytes;
    let info = Arc::new(source::parse_torrent(&bytes).map_err(invalid)?);
    let decision = selection::decide(&info, choice, true).map_err(ResolveFailure::permanent)?;
    let chosen = match decision {
        Decision::Choose(reason) => {
            if let Err(e) = write_stored_copy(&files.stored, &bytes) {
                tracing::warn!("keeping the torrent's metadata: {e}");
            }
            let mark = Sidecar::Choosing {
                reason: reason.clone(),
            };
            if let Err(e) = mark.write(&files.selection) {
                tracing::warn!("recording that files must be chosen: {e}");
            }
            return Ok(Resolved::Choosing {
                info,
                bytes,
                reason,
            });
        }
        Decision::All => {
            opts.only_files = listed.only_files;
            None
        }
        Decision::Select { indices, created } => {
            let output = PathBuf::from(opts.output_folder.clone().unwrap_or_default());
            let created = selection::created_stubs(&output, &info, &indices, &created);
            let c = Chosen::new(&info, indices, created);
            opts.only_files = Some(info.lib_indices(&c.indices));
            if let Err(e) = c.sidecar().write(&files.selection) {
                tracing::warn!("recording the file selection: {e}");
            }
            Some(Arc::new(c))
        }
    };
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
        .add_torrent(AddTorrent::from_bytes(bytes.clone()), Some(opts))
        .await
    {
        Ok(AddTorrentResponse::Added(_, handle)) => {
            // Not awaited: an abort cannot leave a half-written copy behind its remover.
            if let Err(e) = write_stored_copy(&files.stored, &bytes) {
                tracing::warn!("keeping the torrent's metadata: {e}");
            }
            Ok(Resolved::Added {
                handle,
                info,
                chosen,
            })
        }
        Ok(AddTorrentResponse::AlreadyManaged(..)) => Err(ResolveFailure::permanent(DUPLICATE)),
        Ok(AddTorrentResponse::ListOnly(_)) => Err(ResolveFailure::permanent(
            "unexpected list-only answer from librqbit",
        )),
        Err(e) if is_already_exists(&e) => {
            tracing::warn!("{e:#}");
            Err(ResolveFailure::permanent(EXISTS))
        }
        Err(e) => Err(ResolveFailure::new(format!("{e:#}"))),
    }
}

/// librqbit's `only_files` for `e` (`None`: every file).
fn only_files(e: &Entry) -> Option<Vec<usize>> {
    let (c, info) = (e.selected.as_ref()?, e.info.as_ref()?);
    Some(info.lib_indices(&c.indices))
}

/// Whether `resume` must re-add a torrent instead of unpausing it: one in librqbit's
/// `Error` state that was added with `overwrite: false` (a fresh add). librqbit
/// restarts a failed torrent by opening its files again with the add-time options, and
/// `create_new` then refuses the files it created itself. Every file there was created
/// by librqbit for this download (a fresh add never starts over existing files), so
/// they are puppy's to write over.
fn readd_on_resume(phase: map::Phase, overwrite: bool) -> bool {
    phase == map::Phase::Error && !overwrite
}

/// Writes a stored `.torrent` copy (synchronously: it is small).
fn write_stored_copy(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    std::fs::write(path, bytes)
}

/// librqbit refused to create a file that already exists (`overwrite: false`).
fn is_already_exists(e: &anyhow::Error) -> bool {
    e.chain().any(|c| {
        c.downcast_ref::<std::io::Error>()
            .is_some_and(|io| io.kind() == std::io::ErrorKind::AlreadyExists)
    })
}

/// A librqbit error as an engine error: I/O if an `io::Error` is behind it, else invalid.
fn lib_error(e: anyhow::Error) -> EngineError {
    let msg = format!("{e:#}");
    match e.chain().find_map(|c| c.downcast_ref::<std::io::Error>()) {
        Some(io) => EngineError::Io(std::io::Error::new(io.kind(), msg)),
        None => EngineError::Invalid(msg),
    }
}

/// Releases a reserved id (and the stored `.torrent` copy and placement marker, if
/// `stored`) if `add` fails or its future is dropped mid-await.
struct Reservation<'a> {
    shared: &'a Shared,
    id: DownloadId,
    stored: bool,
    armed: bool,
}

impl Drop for Reservation<'_> {
    fn drop(&mut self) {
        if self.armed {
            self.shared.lock().remove(&self.id);
            if self.stored {
                self.shared.forget(self.id);
            }
        }
    }
}

/// Where an added torrent's files go.
#[derive(Clone, Copy)]
enum Target<'a> {
    /// A new download: into this output dir, never where something already is (see
    /// [`Layout::fresh_torrent`]).
    Fresh(&'a Path),
    /// A restored download: back where its files were (its `Added::path`). `folder`:
    /// `path` is the torrent's folder whatever its layout (a magnet's).
    Restore { path: &'a Path, folder: bool },
}

impl Target<'_> {
    /// The layout, and whether librqbit may write over existing files (restore only).
    fn layout(self, info: &source::TorrentInfo, map: &Torrents) -> (Layout, bool) {
        match self {
            Target::Fresh(base) => (Layout::fresh_torrent(base, info, claimed(map)), false),
            Target::Restore { path, folder } => (
                Layout::restored(path, !folder && info.single_file.is_some()),
                true,
            ),
        }
    }
}

/// Paths that downloads in the engine already use (a fresh add must avoid them even
/// before their files exist).
fn claimed(map: &Torrents) -> impl Fn(&Path) -> bool + '_ {
    |p| {
        map.values()
            .any(|e| e.layout.path == p || e.layout.output_folder == p)
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
    /// Creates the engine and its poll task. Nothing is opened yet: the librqbit session
    /// (listener, DHT, the engine's dirs) starts with the first torrent that needs it.
    /// Never fails today; the `Result` is kept for engines that check more up front.
    pub async fn new(opts: TorrentEngineOptions) -> Result<Self, EngineError> {
        let (events, _) = broadcast::channel(256);
        let cancel = CancellationToken::new();
        let shared = Arc::new(Shared {
            session: OnceCell::new(),
            limits: Mutex::new((None, None)),
            cancel: cancel.clone(),
            torrents: Mutex::new(HashMap::new()),
            events,
            opts,
            next_gen: AtomicU64::new(0),
            ops: tokio::sync::Mutex::new(()),
        });
        tokio::spawn(poll::run(Arc::downgrade(&shared), cancel));
        Ok(Self { shared })
    }

    /// The session's current global `(download, upload)` limits in bytes/s, or the ones
    /// it will start with (tests).
    #[doc(hidden)]
    pub fn global_limits(&self) -> (Option<u32>, Option<u32>) {
        match self.shared.started() {
            Some(s) => (
                s.ratelimits.get_download_bps().map(NonZeroU32::get),
                s.ratelimits.get_upload_bps().map(NonZeroU32::get),
            ),
            None => {
                let (d, u) = *self.shared.limits.lock().unwrap_or_else(|e| e.into_inner());
                (
                    bps_limit(d).map(NonZeroU32::get),
                    bps_limit(u).map(NonZeroU32::get),
                )
            }
        }
    }

    /// Whether the librqbit session has been started (tests).
    #[doc(hidden)]
    pub fn session_started(&self) -> bool {
        self.shared.started().is_some()
    }

    /// Starts the session now if it is not running yet (tests).
    #[doc(hidden)]
    pub async fn start_session(&self) -> Result<(), EngineError> {
        self.shared.session().await.map(|_| ())
    }

    /// How many torrents librqbit holds (tests: choosing files adds none).
    #[doc(hidden)]
    pub fn librqbit_torrents(&self) -> usize {
        self.shared
            .started()
            .map_or(0, |s| s.with_torrents(|t| t.count()))
    }

    /// Why a download is choosing files although a choice was given (tests).
    #[doc(hidden)]
    pub fn choosing_reason(&self, id: DownloadId) -> Option<String> {
        match &self.shared.lock().get(&id)?.slot {
            Slot::Choosing { reason, .. } => reason.clone(),
            _ => None,
        }
    }

    /// Where the session accepts incoming peer connections (`None` until it is started).
    pub fn listen_addr(&self) -> Option<SocketAddr> {
        self.shared.started()?.listen_addr()
    }

    /// Pauses every torrent and stops the session's tasks (listener, DHT, peers, poll).
    /// Takes about 1 s (librqbit gives its tasks a moment to wind down); nothing to do if
    /// the session never started.
    pub async fn shutdown(&self) {
        self.shared.abort_resolving();
        if let Some(session) = self.shared.started() {
            session.stop().await;
        }
    }

    /// Adds a `.torrent` (already read) to librqbit. `trackers`: a magnet's `tr`s (a
    /// magnet restored from its stored metadata). `files`: the file choice of a new
    /// download (an invalid selection is refused up front); `None` (restore): the
    /// recorded one ([`selection::decide_restored`]). If the user must choose files,
    /// nothing is added to librqbit (see [`Self::enter_choosing`]).
    #[allow(clippy::too_many_arguments)]
    async fn add_torrent_file(
        &self,
        id: DownloadId,
        bytes: Vec<u8>,
        target: Target<'_>,
        copy: StoredCopy,
        opts: &AddOptions,
        trackers: Vec<String>,
        files: Option<&FileChoice>,
    ) -> Result<Added, EngineError> {
        let info = Arc::new(source::parse_torrent(&bytes)?);
        let decision = match files {
            Some(choice) => {
                selection::decide(&info, choice, false).map_err(EngineError::Invalid)?
            }
            None => {
                selection::decide_restored(Sidecar::read(&self.shared.selection_file(id)), &info)
            }
        };
        let (indices, created) = match decision {
            Decision::Choose(reason) => {
                return self
                    .enter_choosing(id, bytes.into(), info, target, copy, opts, trackers, reason)
                    .await;
            }
            Decision::All => (None, Vec::new()),
            Decision::Select { indices, created } => (Some(indices), created),
        };
        let session = self.shared.session().await?;
        let (layout, overwrite, chosen) = {
            let mut map = self.shared.lock();
            check_free(&map, id, &info.info_hash)?;
            let (layout, overwrite) = target.layout(&info, &map);
            let chosen = indices.map(|indices| {
                let created =
                    selection::created_stubs(&layout.output_folder, &info, &indices, &created);
                Arc::new(Chosen::new(&info, indices, created))
            });
            map.insert(
                id,
                Entry {
                    info_hash: info.info_hash.clone(),
                    name: info.name.clone(),
                    layout: layout.clone(),
                    magnet: None,
                    overwrite,
                    limit: opts.limit,
                    limit_cell: Arc::new(AtomicU64::new(opts.limit.unwrap_or(0))),
                    detail: None,
                    uploaded: 0,
                    info: Some(info.clone()),
                    choice: FileChoice::All,
                    selected: chosen.clone(),
                    slot: Slot::Adding,
                    last_state: None,
                    completed_sent: false,
                    run_wanted: !opts.start_paused,
                },
            );
            (layout, overwrite, chosen)
        };
        let stored = self.shared.stored_torrent(id);
        let mut guard = Reservation {
            shared: &self.shared,
            id,
            stored: copy == StoredCopy::Write,
            armed: true,
        };
        let mut add_opts =
            self.shared
                .add_options(&layout, opts.start_paused, opts.limit, overwrite, trackers)?;
        add_opts.only_files = chosen.as_ref().map(|c| info.lib_indices(&c.indices));
        if copy != StoredCopy::Keep {
            tokio::fs::write(&stored, &bytes).await?;
        }
        // Before the add: the unselected files librqbit creates are recorded as ours.
        if let Some(c) = &chosen {
            let path = self.shared.selection_file(id);
            let sidecar = c.sidecar();
            tokio::task::spawn_blocking(move || sidecar.write(&path))
                .await
                .map_err(std::io::Error::other)??;
        }
        // Recorded now: the disk cannot tell it once the download is completed.
        let placement = source::Placement::of(&layout).as_str();
        if let Err(e) = tokio::fs::write(self.shared.placement_marker(id), placement).await {
            tracing::warn!(id, "recording where the torrent's files go: {e}");
        }
        let res = session
            .add_torrent(AddTorrent::from_bytes(bytes), Some(add_opts))
            .await
            .map_err(|e| {
                if is_already_exists(&e) {
                    tracing::warn!(id, "{e:#}");
                    EngineError::Invalid(EXISTS.into())
                } else {
                    lib_error(e)
                }
            })?;
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
            name: info.name.clone(),
            path: layout.path,
            total_bytes: Some(chosen.map_or(info.total_bytes, |c| c.total())),
        })
    }

    /// Registers download `id` as choosing files: nothing goes to librqbit (the session
    /// is not even started for it) and nothing is written to the download dir; the
    /// stored `.torrent` copy (as `copy` says), the placement marker and the
    /// `.selection` sidecar are written now, so a restart restores it as choosing.
    #[allow(clippy::too_many_arguments)]
    async fn enter_choosing(
        &self,
        id: DownloadId,
        bytes: bytes::Bytes,
        info: Arc<TorrentInfo>,
        target: Target<'_>,
        copy: StoredCopy,
        opts: &AddOptions,
        trackers: Vec<String>,
        reason: Option<String>,
    ) -> Result<Added, EngineError> {
        let layout = {
            let mut map = self.shared.lock();
            check_free(&map, id, &info.info_hash)?;
            let (layout, overwrite) = target.layout(&info, &map);
            map.insert(
                id,
                Entry {
                    info_hash: info.info_hash.clone(),
                    name: info.name.clone(),
                    layout: layout.clone(),
                    magnet: None,
                    overwrite,
                    limit: opts.limit,
                    limit_cell: Arc::new(AtomicU64::new(opts.limit.unwrap_or(0))),
                    detail: None,
                    uploaded: 0,
                    info: Some(info.clone()),
                    choice: FileChoice::All,
                    selected: None,
                    slot: Slot::Choosing {
                        bytes: bytes.clone(),
                        trackers,
                        reason: reason.clone(),
                    },
                    last_state: None,
                    completed_sent: false,
                    run_wanted: false,
                },
            );
            layout
        };
        let mut guard = Reservation {
            shared: &self.shared,
            id,
            stored: copy == StoredCopy::Write,
            armed: true,
        };
        let shared = self.shared.clone();
        let placement = source::Placement::of(&layout).as_str();
        tokio::task::spawn_blocking(move || -> std::io::Result<()> {
            create_private_dir(&shared.opts.data_dir.join("torrents"))?;
            if copy != StoredCopy::Keep {
                write_stored_copy(&shared.stored_torrent(id), &bytes)?;
            }
            if let Err(e) = std::fs::write(shared.placement_marker(id), placement) {
                tracing::warn!(id, "recording where the torrent's files go: {e}");
            }
            Sidecar::Choosing { reason }.write(&shared.selection_file(id))
        })
        .await
        .map_err(std::io::Error::other)??;
        guard.armed = false;
        Ok(Added {
            name: info.name.clone(),
            path: layout.path,
            total_bytes: Some(info.total_bytes),
        })
    }

    /// Registers a magnet; its metadata is fetched in the background (unless paused).
    ///
    /// A magnet restored from its link never had its metadata added (its stored copy is
    /// written once it is), so nothing at its path is ours: librqbit does not overwrite
    /// there either.
    fn add_magnet(
        &self,
        id: DownloadId,
        uri: String,
        target: Target<'_>,
        opts: &AddOptions,
        choice: FileChoice,
    ) -> Result<Added, EngineError> {
        let info = source::parse_magnet(&uri)?;
        let name = info.display_name();
        let mut map = self.shared.lock();
        check_free(&map, id, &info.info_hash)?;
        let layout = match target {
            Target::Fresh(base) => Layout::fresh_magnet(base, &info, claimed(&map)),
            Target::Restore { path, .. } => Layout::restored(path, false),
        };
        let add_opts =
            self.shared
                .add_options(&layout, false, opts.limit, false, info.trackers.clone())?;
        let limit_cell = Arc::new(AtomicU64::new(opts.limit.unwrap_or(0)));
        let slot = if opts.start_paused {
            Slot::Held
        } else {
            self.shared.spawn_resolve(
                id,
                uri.clone(),
                add_opts,
                limit_cell.clone(),
                choice.clone(),
            )
        };
        map.insert(
            id,
            Entry {
                info_hash: info.info_hash,
                name: name.clone(),
                layout: layout.clone(),
                magnet: Some(uri),
                overwrite: false,
                limit: opts.limit,
                limit_cell,
                detail: None,
                uploaded: 0,
                info: None,
                choice,
                selected: None,
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

    /// Removes download `id`: from librqbit (deleting only its own files if asked: see
    /// [`Shared::delete_managed`]), and its stored `.torrent` copy unless `keep_copy`. A
    /// magnet still fetching its metadata is cancelled.
    async fn remove_inner(
        &self,
        id: DownloadId,
        delete_files: bool,
        keep_copy: bool,
    ) -> Result<(), EngineError> {
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
                let h = task.await.ok().flatten();
                if h.is_none() {
                    self.shared.settle_slipped(id, &e.info_hash).await;
                }
                h
            }
            Slot::Attention(_) if delete_files => {
                tracing::warn!(
                    id,
                    path = %e.layout.path.display(),
                    "removed without its .torrent: its files are left on disk"
                );
                None
            }
            Slot::Held
            | Slot::PendingFailed { .. }
            | Slot::Attention(_)
            | Slot::Adding
            | Slot::Choosing { .. } => None,
        };
        // A fetch aborted after it added may have chosen files: they are recorded.
        let recorded = match (&handle, &e.selected) {
            (Some(h), None) if delete_files => self.shared.recorded_choice(id, h),
            _ => None,
        };
        let chosen = match (&e.selected, &e.info, &recorded) {
            (Some(c), Some(i), _) => Some((c.as_ref(), i.as_ref())),
            (_, _, Some((c, i))) => Some((c, i)),
            _ => None,
        };
        let deleted = match handle {
            Some(h) => {
                self.shared
                    .delete_managed(&h, &e.layout, delete_files, chosen)
                    .await
            }
            None => Ok(()),
        };
        if !keep_copy {
            // The download is gone either way: drop its stored copy before reporting.
            self.shared.forget(id);
        }
        deleted
    }
}

impl TorrentEngine {
    /// Restores a download added before a restart, its files at `path`: a `.torrent`
    /// from its stored copy (else the original file, if still there), a magnet from its
    /// stored metadata (else its link: the metadata is fetched again). librqbit checks
    /// the existing data before the torrent goes on. With neither copy nor original, a
    /// `.torrent` download is registered as needing attention.
    ///
    /// The file choice recorded in the `.selection` sidecar is kept (choosing stays
    /// choosing, nothing added to librqbit). A magnet restored from its link applies its
    /// recorded choice (else the request's) once its metadata arrives.
    async fn restore(
        &self,
        id: DownloadId,
        req: AddRequest,
        path: &Path,
        opts: &AddOptions,
    ) -> Result<Added, EngineError> {
        let stored = self.shared.stored_torrent(id);
        let have_copy = tokio::fs::try_exists(&stored).await.unwrap_or(false);
        let original = match req.source {
            Source::Magnet { uri } => {
                let target = Target::Restore { path, folder: true };
                if have_copy {
                    let trackers = source::parse_magnet(&uri)?.trackers;
                    match source::read_torrent_file(&stored).await {
                        Ok(bytes) => {
                            return self
                                .add_torrent_file(
                                    id,
                                    bytes,
                                    target,
                                    StoredCopy::Keep,
                                    opts,
                                    trackers,
                                    None,
                                )
                                .await;
                        }
                        Err(e) => tracing::warn!(id, "reading the stored metadata: {e}"),
                    }
                }
                let choice = match Sidecar::read(&self.shared.selection_file(id)) {
                    Some(Sidecar::Choosing { .. }) => FileChoice::Ask,
                    Some(Sidecar::Selected { indices, .. }) => FileChoice::Select {
                        selection: Selection {
                            indices,
                            globs: Vec::new(),
                        },
                    },
                    None => req.files,
                };
                return self.add_magnet(id, uri, target, opts, choice);
            }
            Source::TorrentFile { path } => path,
            Source::Http { .. } => return Err(EngineError::Unsupported("not a torrent source")),
        };
        let target = Target::Restore {
            path,
            folder: false,
        };
        if have_copy {
            let bytes = source::read_torrent_file(&stored).await?;
            return self
                .add_torrent_file(id, bytes, target, StoredCopy::Keep, opts, Vec::new(), None)
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
                .add_torrent_file(
                    id,
                    bytes,
                    target,
                    StoredCopy::Rewrite,
                    opts,
                    Vec::new(),
                    None,
                )
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
                overwrite: true,
                limit: opts.limit,
                limit_cell: Arc::new(AtomicU64::new(opts.limit.unwrap_or(0))),
                detail: None,
                uploaded: 0,
                info: None,
                choice: FileChoice::All,
                selected: None,
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
        self.shared.cancel.cancel();
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
    /// DHT UDP port; `None`: the one stored in `dht_state` (else OS-assigned), `Some(0)`:
    /// OS-assigned.
    dht_port: Option<u16>,
    /// DHT bootstrap nodes; `None`: librqbit's defaults (tests: none).
    dht_bootstrap: Option<Vec<String>>,
    /// How often the DHT state is saved; `None`: librqbit's default (tests: often).
    dht_dump_interval: Option<Duration>,
    trackers: bool,
    lsd: bool,
    upnp: bool,
    /// Check the listen port with a plain bind first (see [`Self::start_recording_attempts`]).
    probe_port: bool,
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
            dht_port: None,
            dht_bootstrap: None,
            dht_dump_interval: None,
            trackers: !test,
            lsd: opts.lsd && !test,
            upnp: opts.upnp && !test,
            probe_port: true,
        }
    }

    /// Starts the session (its tasks stop when `parent` is cancelled); see
    /// [`Self::start_recording_attempts`].
    async fn start_with_fallback(
        &self,
        parent: &CancellationToken,
    ) -> anyhow::Result<Arc<Session>> {
        let (session, _) = self
            .start_recording_attempts(parent, &mut Vec::new())
            .await?;
        Ok(session)
    }

    /// Starts the session on the planned ports, falling back to OS-assigned ones:
    /// - The listen port is first checked with a plain `TcpListener` bind (released at
    ///   once): if it is in use or privileged, an OS-assigned port is used from the
    ///   start (on Windows librqbit may otherwise bind a port another program holds).
    /// - If librqbit still cannot bind the listen port, once more on an OS-assigned one.
    /// - If the DHT's UDP port (the one stored in its state file) is busy, once more on
    ///   an OS-assigned DHT port.
    ///
    /// Any other error is returned as is. Each attempt's cancellation token (a child of
    /// `parent`) is pushed to `attempts` (tests check failed attempts are cancelled).
    async fn start_recording_attempts(
        &self,
        parent: &CancellationToken,
        attempts: &mut Vec<CancellationToken>,
    ) -> anyhow::Result<(Arc<Session>, CancellationToken)> {
        let mut plan = self.clone();
        if plan.probe_port && plan.listen_addr.port() != 0 && !port_free(plan.listen_addr) {
            tracing::warn!(
                addr = %plan.listen_addr,
                "torrent listen port unavailable; using an OS-assigned port"
            );
            plan.listen_addr.set_port(0);
        }
        loop {
            let cancel = parent.child_token();
            attempts.push(cancel.clone());
            let e = match plan.start(cancel).await {
                Ok(r) => return Ok(r),
                Err(e) if is_port_unavailable(&e) => e,
                Err(e) => return Err(e),
            };
            if is_dht_error(&e) && plan.dht_state.is_some() && plan.dht_port != Some(0) {
                tracing::warn!("DHT port unavailable ({e:#}); using an OS-assigned port");
                plan.dht_port = Some(0);
            } else if !is_dht_error(&e) && plan.listen_addr.port() != 0 {
                tracing::warn!(
                    addr = %plan.listen_addr,
                    "torrent listen port unavailable ({e:#}); using an OS-assigned port"
                );
                plan.listen_addr.set_port(0);
            } else {
                return Err(e);
            }
        }
    }

    /// One attempt, with its own cancellation token: cancelled if the attempt fails, so
    /// nothing it started (listener, DHT) outlives it.
    async fn start(
        &self,
        cancel: CancellationToken,
    ) -> anyhow::Result<(Arc<Session>, CancellationToken)> {
        let dht = self.dht_state.as_ref().map(|state| DhtSessionConfig {
            bootstrap_addrs: self.dht_bootstrap.clone(),
            port: self.dht_port,
            // The DHT routing table (not torrents) is kept between runs for a quick bootstrap.
            persistence: Some(DhtPersistenceConfig {
                dump_interval: self.dht_dump_interval,
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
                listen_addr: self.listen_addr,
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

/// Whether a plain TCP bind of `addr` works (the socket is closed again at once). Only
/// "in use" and "permission denied" count as not free.
fn port_free(addr: SocketAddr) -> bool {
    use std::io::ErrorKind::{AddrInUse, PermissionDenied};
    !matches!(
        std::net::TcpListener::bind(addr),
        Err(e) if matches!(e.kind(), AddrInUse | PermissionDenied)
    )
}

/// Whether a session start failed while setting up the DHT (librqbit's context).
fn is_dht_error(e: &anyhow::Error) -> bool {
    e.chain().any(|c| {
        let m = c.to_string();
        m.starts_with("error initializing") && m.contains("DHT")
    })
}

/// Whether a session start failed because a port can't be bound. librqbit wraps
/// the socket error in `librqbit_dualstack_sockets::Error::Bind`, whose io::Error is not
/// exposed as a `source()` (and the DHT boxes that error in its own `Error::Bind`), so
/// every form is looked for along the chain.
fn is_port_unavailable(e: &anyhow::Error) -> bool {
    use std::io::ErrorKind::{AddrInUse, PermissionDenied};
    fn bind_io(e: &librqbit_dualstack_sockets::Error) -> Option<&std::io::Error> {
        match e {
            librqbit_dualstack_sockets::Error::Bind(io) => Some(io),
            _ => None,
        }
    }
    e.chain().any(|cause| {
        let io = cause
            .downcast_ref::<std::io::Error>()
            .or_else(|| {
                cause
                    .downcast_ref::<librqbit_dualstack_sockets::Error>()
                    .and_then(bind_io)
            })
            .or_else(|| match cause.downcast_ref::<librqbit::dht::Error>() {
                Some(librqbit::dht::Error::Bind(b)) => bind_io(b),
                _ => None,
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
            return self.restore(id, req, &path, &opts).await;
        }
        let base = req
            .output_dir
            .clone()
            .unwrap_or_else(|| self.shared.opts.default_dir.clone());
        let target = Target::Fresh(&base);
        match req.source {
            Source::TorrentFile { path } => {
                let bytes = source::read_torrent_file(&path).await?;
                self.add_torrent_file(
                    id,
                    bytes,
                    target,
                    StoredCopy::Write,
                    &opts,
                    Vec::new(),
                    Some(&req.files),
                )
                .await
            }
            Source::Magnet { uri } => self.add_magnet(id, uri, target, &opts, req.files),
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
                // Choosing files: nothing runs until the choice is made.
                other @ (Slot::Held
                | Slot::PendingFailed { .. }
                | Slot::Attention(_)
                | Slot::Adding
                | Slot::Choosing { .. }) => {
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
        match task.await {
            Ok(Some(h)) => {
                let _ = self
                    .shared
                    .live()?
                    .delete(TorrentIdOrHash::Id(h.id()), false)
                    .await;
            }
            // Aborted: it may have added the torrent to librqbit just before.
            _ => {
                let hash = self.shared.lock().get(&id).map(|e| e.info_hash.clone());
                if let Some(hash) = hash {
                    self.shared.settle_slipped(id, &hash).await;
                }
            }
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
        let (h, overwrite) = {
            let mut map = self.shared.lock();
            let e = map
                .get_mut(&id)
                .filter(|e| !matches!(e.slot, Slot::Adding))
                .ok_or(EngineError::NotFound(id))?;
            if !matches!(e.slot, Slot::Attention(_) | Slot::Choosing { .. }) {
                e.run_wanted = true;
            }
            match &e.slot {
                Slot::Managed(h) => (h.clone(), e.overwrite),
                Slot::Resolving { .. } => return Ok(()),
                Slot::Held | Slot::PendingFailed { .. } => {
                    let uri = e.magnet.clone().expect("only magnets wait for metadata");
                    let trackers = source::parse_magnet(&uri)?.trackers;
                    let opts = self.shared.add_options(
                        &e.layout,
                        false,
                        e.limit,
                        e.overwrite,
                        trackers,
                    )?;
                    e.slot = self.shared.spawn_resolve(
                        id,
                        uri,
                        opts,
                        e.limit_cell.clone(),
                        e.choice.clone(),
                    );
                    return Ok(());
                }
                Slot::Choosing { .. } => {
                    return Err(EngineError::Invalid("choose files first".into()));
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
        if readd_on_resume(st.phase, overwrite) {
            return self.shared.readd_overwriting(id, &h).await;
        }
        self.shared.live()?.unpause(&h).await.map_err(lib_error)
    }

    /// Removes the torrent from librqbit (deleting only its own files if asked: a
    /// multi-file torrent's or magnet's folder, a single-file torrent's file) and its
    /// stored `.torrent` copy. A magnet still fetching its metadata is cancelled.
    async fn remove(&self, id: DownloadId, delete_files: bool) -> Result<(), EngineError> {
        self.remove_inner(id, delete_files, false).await
    }

    /// Like `remove(id, false)`, but the stored `.torrent` copy is kept: it lists the
    /// files [`Engine::delete_completed`] deletes.
    async fn drop_completed(&self, id: DownloadId) -> Result<(), EngineError> {
        self.remove_inner(id, false, true).await
    }

    /// Deletes exactly the files the stored `.torrent` copy lists, where they were put
    /// (`path` and the placement as recorded at add, see [`source::completed_folders`]),
    /// then the folders they were in once empty (a torrent's own folder included, the
    /// download dir never). Never through a symlink (see [`source::delete_listed`]).
    /// Without the copy, nothing is deleted.
    async fn delete_completed(&self, id: DownloadId, path: &Path) -> Result<(), EngineError> {
        let stored = self.shared.stored_torrent(id);
        let bytes = match source::read_torrent_file(&stored).await {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!(
                    id,
                    path = %path.display(),
                    "cannot tell the torrent's files ({e}): they are left on disk"
                );
                return Ok(());
            }
        };
        let mut info = source::parse_torrent(&bytes)?;
        match Sidecar::read(&self.shared.selection_file(id)) {
            // Never added to librqbit: nothing of it is on disk.
            Some(Sidecar::Choosing { .. }) => return Ok(()),
            Some(Sidecar::Selected { indices, created }) => {
                info.files = Chosen::new(&info, indices, created).deletable(&info);
            }
            None => {}
        }
        let placement = self.shared.placement(id).await;
        let Some((output, keep)) = source::completed_folders(path, &info, placement) else {
            tracing::warn!(
                id,
                path = %path.display(),
                "the recorded path does not match the torrent: its files are left on disk"
            );
            return Ok(());
        };
        tokio::task::spawn_blocking(move || source::delete_listed(&output, &info.files, &keep))
            .await
            .map_err(std::io::Error::other)??;
        Ok(())
    }

    /// Deletes the stored `.torrent` copy.
    async fn forget(&self, id: DownloadId) {
        self.shared.forget(id);
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
            | Slot::Attention(_)
            | Slot::Choosing { .. } => {
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

    /// Kept for the session if it has not started yet.
    fn set_global_download_budget(&self, bps: Option<u64>) {
        self.shared.set_limit(|l| l.0 = bps);
    }

    /// Kept for the session if it has not started yet.
    fn set_global_upload_budget(&self, bps: Option<u64>) {
        self.shared.set_limit(|l| l.1 = bps);
    }

    fn subscribe(&self) -> broadcast::Receiver<EngineEvent> {
        self.shared.events.subscribe()
    }

    /// The torrent's files, once its contents are known (a `.torrent` at once, a magnet
    /// once its metadata has been fetched).
    async fn list_files(&self, id: DownloadId) -> Result<Vec<TorrentFile>, EngineError> {
        let map = self.shared.lock();
        let e = map
            .get(&id)
            .filter(|e| !matches!(e.slot, Slot::Adding))
            .ok_or(EngineError::NotFound(id))?;
        match &e.info {
            Some(info) => Ok(info.torrent_files()),
            None => Err(EngineError::Invalid(
                "the torrent's files are not known yet".into(),
            )),
        }
    }

    /// Makes the one-time file choice of a torrent that is choosing files (else
    /// `Invalid("not choosing files")`; an empty or invalid selection is `Invalid` too)
    /// and adds it to librqbit with only those files, **paused**: `resume` starts it
    /// (the caller queues it like any other download). The choice is recorded in the
    /// `.selection` sidecar (restore, delete). Selecting every file is the same as no
    /// selection.
    async fn select_files(&self, id: DownloadId, sel: Selection) -> Result<(), EngineError> {
        let _ops = self.shared.ops.lock().await;
        let (info, bytes, trackers, layout, limit, overwrite) = {
            let map = self.shared.lock();
            let e = map
                .get(&id)
                .filter(|e| !matches!(e.slot, Slot::Adding))
                .ok_or(EngineError::NotFound(id))?;
            let Slot::Choosing {
                bytes, trackers, ..
            } = &e.slot
            else {
                return Err(EngineError::Invalid("not choosing files".into()));
            };
            let info = e
                .info
                .clone()
                .ok_or_else(|| EngineError::Invalid("not choosing files".into()))?;
            (
                info,
                bytes.clone(),
                trackers.clone(),
                e.layout.clone(),
                e.limit,
                e.overwrite,
            )
        };
        let indices =
            selection::resolve(&info.torrent_files(), &sel).map_err(EngineError::Invalid)?;
        let chosen = (indices.len() < info.files.len()).then(|| {
            let created = selection::created_stubs(&layout.output_folder, &info, &indices, &[]);
            Arc::new(Chosen::new(&info, indices, created))
        });
        let mut opts = self
            .shared
            .add_options(&layout, true, limit, overwrite, trackers)?;
        opts.only_files = chosen.as_ref().map(|c| info.lib_indices(&c.indices));
        let session = self.shared.session().await?;
        let handle = match session
            .add_torrent(AddTorrent::from_bytes(bytes), Some(opts))
            .await
            .map_err(|e| {
                if is_already_exists(&e) {
                    tracing::warn!(id, "{e:#}");
                    EngineError::Invalid(EXISTS.into())
                } else {
                    lib_error(e)
                }
            })? {
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
        let path = self.shared.selection_file(id);
        let recorded = match &chosen {
            Some(c) => c.sidecar().write(&path),
            None => match std::fs::remove_file(&path) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
                _ => Ok(()),
            },
        };
        if let Err(e) = recorded {
            tracing::warn!(id, "recording the file selection: {e}");
        }
        let installed = match self.shared.lock().get_mut(&id) {
            Some(e) => {
                e.slot = Slot::Managed(handle.clone());
                e.selected = chosen;
                e.run_wanted = false;
                e.detail = None;
                true
            }
            None => false,
        };
        if !installed {
            // Removed meanwhile (cannot happen under the ops lock): keep nothing running.
            let _ = session
                .delete(TorrentIdOrHash::Id(handle.id()), false)
                .await;
        }
        Ok(())
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
            dht_port: None,
            dht_bootstrap: Some(Vec::new()),
            dht_dump_interval: None,
            trackers: false,
            lsd: false,
            upnp: false,
            probe_port: true,
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn busy_listen_port_is_seen_by_the_probe() {
        let dir = tempfile::tempdir().unwrap();
        let busy = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let busy_addr = busy.local_addr().unwrap();

        let mut attempts = Vec::new();
        let (session, _) = local_plan(dir.path(), busy_addr)
            .start_recording_attempts(&CancellationToken::new(), &mut attempts)
            .await
            .expect("an OS-assigned port is used");
        let got = session.listen_addr().unwrap();
        assert!(got.ip().is_loopback());
        assert_ne!(got.port(), busy_addr.port());
        assert_eq!(
            attempts.len(),
            1,
            "librqbit was not asked to bind the busy port"
        );
        session.stop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn busy_listen_port_falls_back_without_leaking() {
        let dir = tempfile::tempdir().unwrap();
        let busy = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let busy_addr = busy.local_addr().unwrap();

        // Without the probe, librqbit's own bind error makes it fall back.
        let mut attempts = Vec::new();
        let plan = SessionPlan {
            probe_port: false,
            ..local_plan(dir.path(), busy_addr)
        };
        let (session, cancel) = plan
            .start_recording_attempts(&CancellationToken::new(), &mut attempts)
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
            .start_recording_attempts(&CancellationToken::new(), &mut attempts)
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
        let dht = anyhow::Error::from(librqbit::dht::Error::Bind(Box::new(
            librqbit_dualstack_sockets::Error::Bind(Error::from(ErrorKind::AddrInUse)),
        )))
        .context("error initializing persistent DHT");
        assert!(is_port_unavailable(&dht));
        assert!(is_dht_error(&dht));
        assert!(!is_dht_error(&wrapped(ErrorKind::AddrInUse)));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn busy_stored_dht_port_falls_back_to_a_fresh_one() {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("torrent/dht.json");
        let plan = SessionPlan {
            dht_state: Some(state.clone()),
            dht_dump_interval: Some(Duration::from_millis(100)),
            ..local_plan(dir.path(), (Ipv4Addr::LOCALHOST, 0).into())
        };
        let (first, _) = plan
            .start_recording_attempts(&CancellationToken::new(), &mut Vec::new())
            .await
            .expect("starts");
        let port = first.get_dht().expect("DHT on").listen_addr().port();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !state.exists() {
            assert!(Instant::now() < deadline, "DHT state never saved");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        first.stop().await;
        // Someone else takes the stored DHT port meanwhile.
        let deadline = Instant::now() + Duration::from_secs(5);
        let _busy = loop {
            match std::net::UdpSocket::bind((Ipv4Addr::UNSPECIFIED, port)) {
                Ok(s) => break s,
                Err(e) => {
                    assert!(Instant::now() < deadline, "port {port} never released: {e}");
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            }
        };

        let mut attempts = Vec::new();
        let (second, _) = plan
            .start_recording_attempts(&CancellationToken::new(), &mut attempts)
            .await
            .expect("falls back to a fresh DHT port");
        assert_ne!(second.get_dht().unwrap().listen_addr().port(), port);
        assert_eq!(attempts.len(), 2);
        assert!(attempts[0].is_cancelled(), "failed attempt left running");
        second.stop().await;
    }

    fn test_options(dir: &Path) -> TorrentEngineOptions {
        TorrentEngineOptions {
            dht: false,
            upnp: false,
            lsd: false,
            listen_port: 0,
            seed_ratio: 0.0,
            data_dir: dir.join("data"),
            default_dir: dir.join("downloads"),
            test_mode: true,
            test_initial_peers: Vec::new(),
        }
    }

    /// A single-file torrent `x.bin` of 1000 bytes (piece hashes all zero).
    fn crafted_torrent() -> Vec<u8> {
        let mut out =
            b"d4:infod6:lengthi1000e4:name5:x.bin12:piece lengthi16384e6:pieces20:".to_vec();
        out.extend_from_slice(&[0u8; 20]);
        out.extend_from_slice(b"ee");
        out
    }

    fn held_entry(info_hash: &str, dir: &Path) -> Entry {
        Entry {
            info_hash: info_hash.into(),
            name: "x".into(),
            layout: Layout::restored(dir, false),
            magnet: Some(format!("magnet:?xt=urn:btih:{info_hash}")),
            overwrite: false,
            limit: None,
            limit_cell: Arc::new(AtomicU64::new(0)),
            detail: None,
            uploaded: 0,
            info: None,
            choice: FileChoice::All,
            selected: None,
            slot: Slot::Held,
            last_state: None,
            completed_sent: false,
            run_wanted: false,
        }
    }

    /// The seam behind pause/remove of a magnet whose fetch was aborted right after it
    /// added the torrent to librqbit (an end-to-end race test is not deterministic).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn torrents_slipped_in_after_an_aborted_fetch_are_settled() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("out");
        let engine = TorrentEngine::new(test_options(dir.path())).await.unwrap();
        let shared = &engine.shared;
        let session = shared.session().await.unwrap().clone();
        let bytes = crafted_torrent();
        let hash = source::parse_torrent(&bytes).unwrap().info_hash;
        let lookup = || session.get(TorrentIdOrHash::Hash(Id20::from_str(&hash).unwrap()));
        let h = session
            .add_torrent(
                AddTorrent::from_bytes(bytes.clone()),
                Some(AddTorrentOptions {
                    paused: true,
                    output_folder: Some(out.to_str().unwrap().to_owned()),
                    ..Default::default()
                }),
            )
            .await
            .unwrap()
            .into_handle()
            .unwrap();

        // Paused while fetching (`Held`): adopted, with its metadata kept.
        shared.lock().insert(1, held_entry(&hash, &out));
        shared.settle_slipped(1, &hash).await;
        assert!(matches!(&shared.lock()[&1].slot, Slot::Managed(x) if Arc::ptr_eq(x, &h)));
        assert_eq!(std::fs::read(shared.stored_torrent(1)).unwrap(), bytes);

        // Another download owns it: left alone.
        shared.settle_slipped(2, &hash).await;
        assert!(lookup().is_some(), "another download's torrent was dropped");

        // Removed meanwhile: dropped from librqbit, its files kept.
        shared.lock().remove(&1);
        shared.settle_slipped(1, &hash).await;
        assert!(lookup().is_none(), "left in librqbit");
        assert!(out.join("x.bin").exists(), "its files were deleted");
        engine.shutdown().await;
    }

    #[test]
    fn only_failed_fresh_adds_are_readded_on_resume() {
        use map::Phase;
        assert!(readd_on_resume(Phase::Error, false));
        assert!(!readd_on_resume(Phase::Error, true), "restored: unpause");
        for p in [
            Phase::Paused,
            Phase::Live,
            Phase::Initializing { paused: false },
        ] {
            assert!(!readd_on_resume(p, false), "{p:?}");
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn session_start_failure_is_transient_and_retried() {
        let dir = tempfile::tempdir().unwrap();
        let engine = TorrentEngine::new(test_options(dir.path())).await.unwrap();
        assert!(!engine.session_started(), "started before any torrent");
        assert!(engine.listen_addr().is_none());
        // The engine's dir cannot be created: a file is in the way.
        std::fs::create_dir_all(dir.path().join("data")).unwrap();
        std::fs::write(dir.path().join("data/torrent"), b"x").unwrap();

        let err = engine.start_session().await.unwrap_err();
        assert!(
            matches!(&err, EngineError::Unavailable(m) if m == UNAVAILABLE),
            "{err:?}"
        );
        assert!(err.is_transient());
        let uri = format!("magnet:?xt=urn:btih:{}", "ab".repeat(20));
        let req = AddRequest {
            source: Source::Magnet { uri },
            output_dir: Some(dir.path().join("out")),
            filename: None,
            context: Default::default(),
            files: Default::default(),
            seed: None,
        };
        engine
            .add(1, req, StartMode::Fresh, AddOptions::default())
            .await
            .expect("a magnet is registered at once");
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let st = engine.status(1).unwrap().state;
            if st
                == (DownloadState::Failed {
                    reason: UNAVAILABLE.into(),
                    transient: true,
                })
            {
                break;
            }
            assert!(Instant::now() < deadline, "{st:?}");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        // Fixed: the next attempt starts the session.
        std::fs::remove_file(dir.path().join("data/torrent")).unwrap();
        engine.resume(1).await.unwrap();
        engine.start_session().await.expect("starts now");
        assert!(engine.session_started());
        assert!(engine.listen_addr().is_some());
        engine.shutdown().await;
    }
}
