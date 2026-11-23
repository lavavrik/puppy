//! The daemon's download manager: owns the download list and queue, schedules downloads
//! onto engines (at most `max_active` at a time), applies limits, follows engine events,
//! persists everything and restores it after a restart.
//!
//! Concurrency rules:
//! - All bookkeeping lives in one `std::sync::Mutex<State>` that is never held across an
//!   `.await`.
//! - Long engine calls (`add` probes/validates, `resume` validates) run in spawned *jobs*.
//!   While a job is in flight the download shows `Starting` and its entry carries a
//!   [`JobCtl`]; commands racing it only leave requests there (pause) or orphan it
//!   (remove), and the job settles them when the engine call returns.
//! - Transient failures (`Failed { transient: true }`: network trouble, timeouts, HTTP
//!   5xx/429/408) are retried automatically: requeued after `Config::retry_base`,
//!   doubling per failure up to [`MAX_RETRY_DELAY`] (never less than
//!   [`MIN_RETRY_DELAY`]), for as long as the daemon runs. Pausing cancels a pending
//!   retry; resuming (or restarting) resets the delay, and so does a retried download
//!   that is active and transferring again.
//! - Engine events only mean "look again": the manager re-reads `engine.status()` instead
//!   of trusting event payloads, and ignores events for downloads with a job in flight
//!   (the job reads the status itself once the engine call returns). Events may thus
//!   arrive before `add` returns, late, or be lost to lag without corrupting state.
//! - Torrents: `ChoosingFiles` (waiting for `select_files`) and `Seeding` (data complete,
//!   still uploading; the engine keeps holding it) never take a slot. `Completed` is
//!   emitted when the data completes (entering `Seeding`), `SeedingFinished` when
//!   seeding stops (the download is then `Completed` and dropped from its engine).

mod commands;
mod restore;
mod schedule;
mod snapshot;

use crate::budget;
use crate::config::{Config, MAX_RETRY_DELAY};
use crate::store::{
    PersistedState, RuntimeSettings, Store, StoreError, StoredDownload, StoredState,
};
use puppy_core::engine::{Engine, EngineError, EngineEvent};
use puppy_core::types::{DownloadId, DownloadState, RequestContext, Source};
use puppy_proto::{ErrorCode, Event};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::time::Duration;
use tokio::sync::broadcast;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

const EVENT_CAPACITY: usize = 256;
const TICK: Duration = Duration::from_secs(1);
/// The shortest delay before an automatic retry, whatever `Config::retry_base` says.
const MIN_RETRY_DELAY: Duration = Duration::from_secs(1);

#[derive(thiserror::Error, Debug)]
pub enum ManagerError {
    #[error("download {0} not found")]
    NotFound(DownloadId),
    #[error("invalid state: {0}")]
    InvalidState(String),
    #[error("bad request: {0}")]
    BadRequest(String),
    #[error(transparent)]
    Engine(#[from] EngineError),
}

impl ManagerError {
    pub fn code(&self) -> ErrorCode {
        match self {
            ManagerError::NotFound(_) => ErrorCode::NotFound,
            ManagerError::InvalidState(_) => ErrorCode::InvalidState,
            ManagerError::BadRequest(_) => ErrorCode::BadRequest,
            ManagerError::Engine(EngineError::Unsupported(_)) => ErrorCode::Unsupported,
            ManagerError::Engine(_) => ErrorCode::Engine,
        }
    }
}

/// The download manager. Cheap to share as `Arc<Manager>`.
pub struct Manager {
    inner: Arc<Inner>,
}

impl Drop for Manager {
    fn drop(&mut self) {
        // Stops the event and tick tasks; running downloads are left to `shutdown`.
        self.inner.stop.cancel();
    }
}

struct Inner {
    engines: Vec<Arc<dyn Engine>>,
    store: Store,
    cfg: Config,
    st: Mutex<State>,
    events: broadcast::Sender<Event>,
    /// Cancels the event and tick tasks.
    stop: CancellationToken,
    /// In-flight jobs (engine add/resume/reset), aborted on shutdown. `None` once
    /// shutdown has begun: no job may start after that.
    jobs: Mutex<Option<JoinSet<()>>>,
    /// Completed by the first `shutdown`; later calls wait for it.
    shutdown_done: tokio::sync::OnceCell<()>,
}

struct State {
    next_id: DownloadId,
    /// Every download, in queue order (finished ones keep their place; snapshots list
    /// them last).
    order: Vec<DownloadId>,
    entries: HashMap<DownloadId, Entry>,
    settings: RuntimeSettings,
    /// Downloads removed while a job was in flight, with their engine and `delete_files`:
    /// the job removes them from the engine once its engine call returns.
    orphans: HashMap<DownloadId, (usize, bool)>,
    shutting_down: bool,
}

struct Entry {
    /// The persisted record. `rec.state` is only written when persisting; the live state
    /// is `state`.
    rec: StoredDownload,
    state: DownloadState,
    /// Index into `Inner::engines`; `None` if no engine accepts the source.
    engine: Option<usize>,
    /// The engine holds this download (an `add` succeeded and it was not removed since).
    held: bool,
    /// A job is in flight (`state` is `Starting`).
    job: Option<JobCtl>,
    /// A pending automatic retry (only while `Failed { transient: true }`).
    retry: Option<Retry>,
    /// The delay before the next automatic retry; `None`: `Config::retry_base`.
    backoff: Option<Duration>,
    /// While `backoff` is set: the bytes done when the download was last seen without
    /// progress; once it is active with more than that, `backoff` is reset.
    progress_mark: Option<u64>,
    /// `Event::SeedingFinished` was emitted since it last entered `Seeding` (the engine
    /// event and the manager's own look may both report the end).
    seeding_finished_sent: bool,
}

/// When a transiently failed download is requeued.
#[derive(Debug, Clone, Copy)]
struct Retry {
    at: tokio::time::Instant,
    unix: u64,
}

impl Entry {
    fn new(rec: StoredDownload, state: DownloadState, engine: Option<usize>) -> Self {
        Entry {
            rec,
            state,
            engine,
            held: false,
            job: None,
            retry: None,
            backoff: None,
            progress_mark: None,
            seeding_finished_sent: false,
        }
    }

    /// Whether `name` (reported by the engine with `total_bytes`) is only a placeholder
    /// that must not replace a name already known: a magnet's before its metadata is
    /// known (`dn` or `magnet <hash>`, its size unknown), e.g. while it is restored
    /// from its link.
    fn is_placeholder_name(&self, total_bytes: Option<u64>) -> bool {
        matches!(self.rec.request.source, Source::Magnet { .. }) && total_bytes.is_none()
    }

    /// Records the name the engine reports, unless it is a placeholder and a better one
    /// is known.
    fn record_name(&mut self, name: &str, total_bytes: Option<u64>) {
        if self.rec.name.is_none() || !self.is_placeholder_name(total_bytes) {
            self.rec.name = Some(name.to_owned());
        }
    }

    /// Forgets a pending retry and resets the backoff (a user resumed or restarted).
    fn reset_retry(&mut self) {
        self.retry = None;
        self.backoff = None;
        self.progress_mark = None;
    }
}

fn is_transient_failure(s: &DownloadState) -> bool {
    matches!(
        s,
        DownloadState::Failed {
            transient: true,
            ..
        }
    )
}

/// Requests left for an in-flight job by commands racing it.
#[derive(Debug, Default)]
struct JobCtl {
    /// Leave the download paused once the engine call returns.
    pause: bool,
    /// A pause was requested at some point (it may have interrupted the engine call,
    /// even if a later resume cleared `pause`).
    interrupted: bool,
}

/// Engine calls to make once the state lock is released.
#[derive(Default)]
struct Effects {
    /// Completed downloads the engine should drop (`drop_completed`).
    drop_completed: Vec<(usize, DownloadId)>,
}

/// The delay before the next automatic retry (the current backoff, or `base` for the
/// first one) and the backoff after it (doubled); both between [`MIN_RETRY_DELAY`] and
/// [`MAX_RETRY_DELAY`].
fn retry_delay(backoff: Option<Duration>, base: Duration) -> (Duration, Duration) {
    let delay = backoff
        .unwrap_or(base)
        .clamp(MIN_RETRY_DELAY, MAX_RETRY_DELAY);
    (delay, (delay * 2).min(MAX_RETRY_DELAY))
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// A limit of 0 means "unlimited" (puppy-core's token bucket treats it that way); make
/// that explicit so it is stored and reported as `None`.
fn normalize_limit(bps: Option<u64>) -> Option<u64> {
    bps.filter(|&b| b > 0)
}

/// Occupies a slot (`ChoosingFiles` and `Seeding` never do).
fn is_active(s: &DownloadState) -> bool {
    matches!(s, DownloadState::Starting | DownloadState::Active)
}

fn is_seeding(s: &DownloadState) -> bool {
    matches!(s, DownloadState::Seeding { .. })
}

/// The data is complete (completed or seeding): finished for queue order.
fn is_finished(s: &DownloadState) -> bool {
    matches!(s, DownloadState::Completed | DownloadState::Seeding { .. })
}

/// `Seeding` before the engine has reported its counters (restored, not held yet).
fn seeding_placeholder() -> DownloadState {
    DownloadState::Seeding {
        uploaded: 0,
        ratio: 0.0,
        seeded_secs: 0,
        ratio_target: None,
        time_target_secs: None,
    }
}

fn stored_state(s: &DownloadState) -> StoredState {
    match s {
        DownloadState::Queued => StoredState::Queued,
        // A download being started is persisted as active: after a restart it is
        // restored (if it got a path) and queued at the front.
        DownloadState::Starting | DownloadState::Active => StoredState::Active,
        DownloadState::Paused => StoredState::Paused,
        DownloadState::ChoosingFiles => StoredState::ChoosingFiles,
        DownloadState::Seeding { .. } => StoredState::Seeding,
        DownloadState::Completed => StoredState::Completed,
        DownloadState::Failed { reason, transient } => StoredState::Failed {
            reason: reason.clone(),
            transient: *transient,
        },
        DownloadState::NeedsAttention { reason } => StoredState::NeedsAttention {
            reason: reason.clone(),
        },
    }
}

impl State {
    fn active_count(&self) -> usize {
        self.entries
            .values()
            .filter(|e| is_active(&e.state))
            .count()
    }

    fn persisted(&self) -> PersistedState {
        let downloads = self
            .order
            .iter()
            .filter_map(|id| self.entries.get(id))
            .map(|e| {
                let mut rec = e.rec.clone();
                rec.state = stored_state(&e.state);
                rec
            })
            .collect();
        PersistedState {
            next_id: self.next_id,
            order: self.order.clone(),
            downloads,
            settings: Some(self.settings.clone()),
            ..PersistedState::default()
        }
    }
}

impl Manager {
    /// Loads the store, rebuilds the download list, starts restoring the engine-held
    /// downloads in the background (see `Inner::restore`: it returns before any engine
    /// call), then starts the event loop and the 1 s schedule/rebalance ticker. Fails if
    /// the state file exists but cannot be read, so it is never overwritten.
    pub async fn start(
        engines: Vec<Arc<dyn Engine>>,
        store: Store,
        cfg: Config,
    ) -> Result<Arc<Self>, StoreError> {
        let persisted = store.load().await?;
        // Subscribed before restoring so no event is missed.
        let receivers: Vec<_> = engines.iter().map(|e| e.subscribe()).collect();
        let settings = persisted.settings.clone().unwrap_or(RuntimeSettings {
            max_active: cfg.max_active,
            global_down_limit: cfg.global_down_limit,
            global_up_limit: cfg.global_up_limit,
        });
        let settings = RuntimeSettings {
            max_active: settings.max_active.max(1),
            global_down_limit: normalize_limit(settings.global_down_limit),
            global_up_limit: normalize_limit(settings.global_up_limit),
        };
        let inner = Arc::new(Inner {
            engines,
            store,
            cfg,
            st: Mutex::new(State {
                next_id: persisted.next_id,
                order: Vec::new(),
                entries: HashMap::new(),
                settings,
                orphans: HashMap::new(),
                shutting_down: false,
            }),
            events: broadcast::channel(EVENT_CAPACITY).0,
            stop: CancellationToken::new(),
            jobs: Mutex::new(Some(JoinSet::new())),
            shutdown_done: tokio::sync::OnceCell::new(),
        });
        inner.apply_upload_budget();
        let restores = inner.restore(persisted);
        inner.persist(&inner.lock());
        for (idx, rx) in receivers.into_iter().enumerate() {
            tokio::spawn(event_task(
                Arc::downgrade(&inner),
                idx,
                rx,
                inner.stop.clone(),
            ));
        }
        tokio::spawn(tick_task(Arc::downgrade(&inner), inner.stop.clone()));
        for job in restores {
            inner.spawn(job);
        }
        inner.schedule();
        Ok(Arc::new(Manager { inner }))
    }

    /// Discrete events (added, completed, failed, removed, needs attention).
    pub fn events(&self) -> broadcast::Receiver<Event> {
        self.inner.events.subscribe()
    }

    /// No download is active, starting, queued, seeding or waiting for an automatic
    /// retry (a torrent choosing files waits for the user, not for the daemon).
    pub fn is_idle(&self) -> bool {
        self.inner.lock().entries.values().all(|e| {
            e.retry.is_none()
                && !matches!(
                    e.state,
                    DownloadState::Queued
                        | DownloadState::Starting
                        | DownloadState::Active
                        | DownloadState::Seeding { .. }
                )
        })
    }

    /// Persists the current state and writes it out now, waiting at most `limit`
    /// (`false`: it timed out; a write error is only logged). For a forced exit, when
    /// `shutdown` is cut short.
    pub async fn save_state(&self, limit: Duration) -> bool {
        let inner = &self.inner;
        inner.persist(&inner.lock());
        tokio::time::timeout(limit, inner.store.flush())
            .await
            .is_ok()
    }

    /// Pauses every engine-held download (so sidecars are flushed; at most
    /// `Config::shutdown_pause_timeout`, then pauses still running are abandoned),
    /// persists the state (downloads that were active stay recorded as active, to resume
    /// after a restart) and flushes the store. Later commands fail with `InvalidState`.
    /// Concurrent and later calls return once the first one has finished.
    pub async fn shutdown(&self) {
        let inner = &self.inner;
        inner.shutdown_done.get_or_init(|| inner.shutdown()).await;
    }
}

impl Inner {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.st.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn persist(&self, st: &State) {
        self.store.save_soon(st.persisted());
    }

    fn emit(&self, ev: Event) {
        // No subscribers is fine.
        let _ = self.events.send(ev);
    }

    /// Moves `e` to `next`, emitting the matching event. Returns whether it changed
    /// (new seeding counters alone are taken without counting as a change: they are not
    /// persisted).
    ///
    /// Data completing (entering `Seeding`, or `Completed` from any other state) emits
    /// `Completed`; `Seeding` ⇒ `Completed` emits `SeedingFinished` instead. `Completed`
    /// drops the download from its engine; a seeding one stays held.
    fn transition(
        &self,
        id: DownloadId,
        e: &mut Entry,
        next: DownloadState,
        fx: &mut Effects,
    ) -> bool {
        if e.state == next {
            return false;
        }
        if is_seeding(&e.state) && is_seeding(&next) {
            e.state = next;
            return false;
        }
        match &next {
            DownloadState::Completed => {
                if is_seeding(&e.state) {
                    self.seeding_finished(id, e);
                } else {
                    self.data_completed(id, e);
                }
                if e.held
                    && let Some(engine) = e.engine
                {
                    fx.drop_completed.push((engine, id));
                }
                e.held = false;
            }
            DownloadState::Seeding { .. } => {
                e.seeding_finished_sent = false;
                self.data_completed(id, e);
            }
            // Its data is downloaded again: completing then is news again.
            DownloadState::NeedsAttention { reason } if reason.is_resumable() => {
                e.rec.finished_unix = None;
                self.emit(Event::NeedsAttention {
                    id,
                    reason: reason.into(),
                });
            }
            DownloadState::ChoosingFiles if !e.rec.choose_files_sent => {
                e.rec.choose_files_sent = true;
                self.emit(Event::ChoosingFiles {
                    id,
                    name: e.rec.name.clone().unwrap_or_default(),
                });
            }
            DownloadState::Failed { reason, transient } => self.emit(Event::Failed {
                id,
                reason: reason.clone(),
                transient: *transient,
            }),
            DownloadState::NeedsAttention { reason } => self.emit(Event::NeedsAttention {
                id,
                reason: reason.into(),
            }),
            _ => {}
        }
        e.state = next;
        if is_transient_failure(&e.state) {
            self.arm_retry(id, e);
        } else {
            e.retry = None;
        }
        true
    }

    /// The data of `e` is complete: emits `Completed`, unless it was already (a restore
    /// retried, or the daemon stopped before recording the seeding state).
    fn data_completed(&self, id: DownloadId, e: &mut Entry) {
        // Nothing left to request: the cookies and credentials go from the stored record.
        e.rec.request.context = RequestContext::default();
        if e.rec.finished_unix.is_none() {
            e.rec.finished_unix = Some(now_unix());
            self.emit(Event::Completed {
                id,
                name: e.rec.name.clone().unwrap_or_default(),
            });
        }
    }

    /// Emits `SeedingFinished` for `e`, once per seeding.
    fn seeding_finished(&self, id: DownloadId, e: &mut Entry) {
        if !e.seeding_finished_sent {
            e.seeding_finished_sent = true;
            self.emit(Event::SeedingFinished { id });
        }
    }

    /// The engine says seeding stopped (after the manager has looked at its status): a
    /// download that went from its data completing straight to `Completed` (seeding
    /// started and ended between two looks) gets its `SeedingFinished` too.
    fn on_seeding_finished(&self, id: DownloadId) {
        let mut st = self.lock();
        if let Some(e) = st.entries.get_mut(&id)
            && e.state == DownloadState::Completed
        {
            self.seeding_finished(id, e);
        }
    }

    /// Schedules the automatic retry of a transiently failed download: after the
    /// current backoff, which then doubles (up to [`MAX_RETRY_DELAY`]).
    fn arm_retry(&self, id: DownloadId, e: &mut Entry) {
        let (delay, next) = retry_delay(e.backoff, self.cfg.retry_base);
        e.backoff = Some(next);
        e.progress_mark = None;
        let unix = now_unix() + delay.as_secs_f64().ceil() as u64;
        e.retry = Some(Retry {
            at: tokio::time::Instant::now() + delay,
            unix,
        });
        tracing::info!(id, ?delay, "retrying the failed download later");
    }

    /// Requeues transiently failed downloads whose retry is due. The caller schedules
    /// afterwards.
    fn retry_due(&self) {
        let now = tokio::time::Instant::now();
        let mut st = self.lock();
        if st.shutting_down {
            return;
        }
        let mut changed = false;
        for (id, e) in st.entries.iter_mut() {
            if e.job.is_none()
                && is_transient_failure(&e.state)
                && e.retry.is_some_and(|r| r.at <= now)
            {
                tracing::info!(id, "retrying the failed download");
                e.retry = None;
                e.state = DownloadState::Queued;
                changed = true;
            }
        }
        if changed {
            self.persist(&st);
        }
    }

    /// Copies what the engine knows about name, path and size into the record, and
    /// resets the retry backoff once a retried download is active and making progress.
    fn record_status(e: &mut Entry, status: &puppy_core::types::DownloadStatus) {
        e.record_name(&status.name, status.total_bytes);
        e.rec.path = Some(status.path.clone());
        if status.total_bytes.is_some() {
            e.rec.total_bytes = status.total_bytes;
        }
        if e.backoff.is_some() {
            let transferring =
                e.state == DownloadState::Active && status.state == DownloadState::Active;
            match e.progress_mark {
                Some(mark) if transferring && status.done_bytes > mark => {
                    tracing::debug!(id = e.rec.id, "transferring again; retry backoff reset");
                    e.backoff = None;
                    e.progress_mark = None;
                }
                Some(_) => {}
                None => e.progress_mark = Some(status.done_bytes),
            }
        }
    }

    /// Applies the engine's view to an engine-held download with no job in flight.
    /// Only transitions the engine makes on its own are taken over (`Completed`,
    /// `Seeding` and its counters, `Failed`, `NeedsAttention`, a magnet's
    /// `ChoosingFiles` once its metadata is known); `Paused`/`Active` are only ever
    /// caused by the manager, which records them itself (a `Paused` seen while the
    /// manager says `Active` is the transient moment between `resume` and the transfer
    /// starting). A seeding torrent whose data failed its check, once resumed (queued or
    /// paused again by the user), is not sent back to needing attention while the engine
    /// still says so.
    fn reconcile(&self, id: DownloadId, e: &mut Entry, fx: &mut Effects) -> bool {
        let (Some(engine), true, None) = (e.engine, e.held, &e.job) else {
            return false;
        };
        let Some(status) = self.engines[engine].status(id) else {
            // Nothing seeds a seeding download its engine lost.
            if is_seeding(&e.state) {
                e.held = false;
                return self.transition(id, e, DownloadState::Completed, fx);
            }
            return false;
        };
        Self::record_status(e, &status);
        let next = match (&e.state, status.state) {
            (_, s @ (DownloadState::Completed | DownloadState::Seeding { .. })) => s,
            (
                DownloadState::Active | DownloadState::Seeding { .. },
                s @ DownloadState::Failed { .. },
            ) => s,
            (
                DownloadState::Paused | DownloadState::Queued,
                DownloadState::NeedsAttention { reason },
            ) if reason.is_resumable() => return false,
            (
                DownloadState::Active
                | DownloadState::Paused
                | DownloadState::Queued
                | DownloadState::Seeding { .. },
                s @ DownloadState::NeedsAttention { .. },
            ) => s,
            (DownloadState::Active, s @ DownloadState::ChoosingFiles) => s,
            _ => return false,
        };
        self.transition(id, e, next, fx)
    }

    /// Re-reads the engine status of `ids` (all of `engine`'s downloads if `None`).
    /// The caller schedules afterwards.
    fn on_engine_change(&self, engine: usize, ids: Option<&[DownloadId]>) {
        let mut fx = Effects::default();
        {
            let mut st = self.lock();
            if st.shutting_down {
                return;
            }
            let targets: Vec<DownloadId> = match ids {
                Some(ids) => ids.to_vec(),
                None => st
                    .entries
                    .iter()
                    .filter(|(_, e)| e.engine == Some(engine))
                    .map(|(id, _)| *id)
                    .collect(),
            };
            let mut changed = false;
            for id in targets {
                if let Some(e) = st.entries.get_mut(&id)
                    && e.engine == Some(engine)
                {
                    changed |= self.reconcile(id, e, &mut fx);
                }
            }
            if changed {
                self.persist(&st);
            }
        }
        self.apply(fx);
    }

    /// Runs the engine calls collected while the lock was held.
    fn apply(&self, fx: Effects) {
        for (engine, id) in fx.drop_completed {
            let engine = self.engines[engine].clone();
            tokio::spawn(async move {
                if let Err(e) = engine.drop_completed(id).await
                    && !matches!(e, EngineError::NotFound(_))
                {
                    tracing::warn!(id, "dropping a completed download from its engine: {e}");
                }
            });
        }
    }

    /// Divides the global download limit across engines by their active counts.
    fn rebalance(&self) {
        let (global, counts) = {
            let st = self.lock();
            let mut counts = vec![0usize; self.engines.len()];
            for e in st.entries.values() {
                if is_active(&e.state)
                    && let Some(engine) = e.engine
                {
                    counts[engine] += 1;
                }
            }
            (st.settings.global_down_limit, counts)
        };
        for (engine, share) in self.engines.iter().zip(budget::divide(global, &counts)) {
            engine.set_global_download_budget(share);
        }
    }

    /// Hands the global upload limit to every engine as is: unlike downloads it is not
    /// divided (only the torrent engine uploads).
    fn apply_upload_budget(&self) {
        let up = self.lock().settings.global_up_limit;
        for engine in &self.engines {
            engine.set_global_upload_budget(up);
        }
    }

    fn spawn_job<F>(&self, fut: F)
    where
        F: std::future::Future<Output = ()> + Send + 'static,
    {
        let mut jobs = self.jobs.lock().unwrap_or_else(|e| e.into_inner());
        let Some(jobs) = jobs.as_mut() else {
            // Shutting down: the download stays `Starting` and is persisted as active,
            // so it is resumed after a restart.
            tracing::debug!("not starting a job: shutting down");
            return;
        };
        while jobs.try_join_next().is_some() {}
        jobs.spawn(fut);
    }

    /// Runs once (see `Manager::shutdown`); if a caller is cancelled midway, the next
    /// one runs it again from the start, which is harmless.
    async fn shutdown(&self) {
        self.lock().shutting_down = true;
        self.stop.cancel();
        // Dropping an in-flight `add` cancels it; its download (if it got that far) is
        // paused below like every other one.
        let jobs = self.jobs.lock().unwrap_or_else(|e| e.into_inner()).take();
        if let Some(mut jobs) = jobs {
            jobs.abort_all();
            while jobs.join_next().await.is_some() {}
        }
        // Engine-held (or possibly held: the add may have returned) downloads are
        // paused; ones removed while starting are removed as their job would have.
        let (to_pause, to_remove) = {
            let mut st = self.lock();
            // Seeding torrents are left seeding: pausing would stop it for good (the
            // engine records their counters when it shuts down, and they go on seeding
            // after a restart).
            let to_pause: Vec<(usize, DownloadId)> = st
                .entries
                .iter()
                .filter(|(_, e)| (e.held || e.job.is_some()) && !is_seeding(&e.state))
                .filter_map(|(id, e)| Some((e.engine?, *id)))
                .collect();
            let to_remove: Vec<_> = st.orphans.drain().collect();
            (to_pause, to_remove)
        };
        let mut calls = JoinSet::new();
        for (engine, id) in to_pause {
            let engine = self.engines[engine].clone();
            calls.spawn(async move { (id, engine.pause(id).await) });
        }
        for (id, (engine, delete_files)) in to_remove {
            let engine = self.engines[engine].clone();
            calls.spawn(async move { (id, engine.remove(id, delete_files).await) });
        }
        let settle = async {
            while let Some(res) = calls.join_next().await {
                if let Ok((id, Err(e))) = res
                    && !matches!(e, EngineError::NotFound(_))
                {
                    tracing::warn!(id, "stopping on shutdown: {e}");
                }
            }
        };
        if tokio::time::timeout(self.cfg.shutdown_pause_timeout, settle)
            .await
            .is_err()
        {
            // Their sidecars may lag behind: they resume from an earlier point.
            tracing::warn!(
                "{} download(s) did not pause within {:?}; saving the state anyway",
                calls.len(),
                self.cfg.shutdown_pause_timeout
            );
            calls.abort_all();
        }
        {
            // What the engines learned since the last look (e.g. a magnet's name from
            // its metadata) is kept too.
            let mut st = self.lock();
            for (id, e) in st.entries.iter_mut() {
                if let (Some(engine), true) = (e.engine, e.held)
                    && let Some(status) = self.engines[engine].status(*id)
                {
                    Self::record_status(e, &status);
                }
            }
            self.persist(&st);
        }
        self.store.close().await;
    }
}

async fn event_task(
    inner: Weak<Inner>,
    engine: usize,
    mut rx: broadcast::Receiver<EngineEvent>,
    stop: CancellationToken,
) {
    loop {
        let ev = tokio::select! {
            _ = stop.cancelled() => return,
            ev = rx.recv() => ev,
        };
        let Some(inner) = inner.upgrade() else { return };
        match ev {
            Ok(EngineEvent::StateChanged { id, .. } | EngineEvent::Completed { id }) => {
                inner.on_engine_change(engine, Some(&[id]));
            }
            Ok(EngineEvent::SeedingFinished { id }) => {
                inner.on_engine_change(engine, Some(&[id]));
                inner.on_seeding_finished(id);
            }
            Err(broadcast::error::RecvError::Lagged(n)) => {
                tracing::debug!(engine, "missed {n} engine events; resyncing");
                inner.on_engine_change(engine, None);
            }
            Err(broadcast::error::RecvError::Closed) => return,
        }
        inner.schedule();
    }
}

/// Every second: resync with the engines, requeue due retries, start queued downloads,
/// rebalance limits.
async fn tick_task(inner: Weak<Inner>, stop: CancellationToken) {
    let mut tick = tokio::time::interval(TICK);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            _ = stop.cancelled() => return,
            _ = tick.tick() => {}
        }
        let Some(inner) = inner.upgrade() else { return };
        for engine in 0..inner.engines.len() {
            inner.on_engine_change(engine, None);
        }
        inner.retry_due();
        inner.schedule();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    #[tokio::test]
    async fn no_jobs_spawn_after_shutdown() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path().join("state.json"));
        let m = Manager::start(Vec::new(), store, Config::default())
            .await
            .unwrap();
        m.shutdown().await;
        let ran = Arc::new(AtomicBool::new(false));
        m.inner.spawn_job({
            let ran = ran.clone();
            async move { ran.store(true, Ordering::SeqCst) }
        });
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!ran.load(Ordering::SeqCst), "a job ran after shutdown");
    }

    #[test]
    fn unsupported_engine_error_has_its_own_code() {
        let e = ManagerError::Engine(EngineError::Unsupported("per-torrent limits"));
        assert_eq!(e.code(), ErrorCode::Unsupported);
        let e = ManagerError::Engine(EngineError::Invalid("x".into()));
        assert_eq!(e.code(), ErrorCode::Engine);
    }

    #[test]
    fn retry_delays_are_floored_and_capped() {
        let base = Duration::from_millis(200);
        let (d, next) = retry_delay(None, base);
        assert_eq!(d, MIN_RETRY_DELAY);
        assert_eq!(next, MIN_RETRY_DELAY * 2);
        assert_eq!(retry_delay(None, Duration::ZERO).0, MIN_RETRY_DELAY);
        // A zero backoff never turns into a tight loop.
        assert_eq!(
            retry_delay(Some(Duration::ZERO), Duration::ZERO),
            (MIN_RETRY_DELAY, MIN_RETRY_DELAY * 2)
        );
        let (d, next) = retry_delay(None, Duration::from_secs(30));
        assert_eq!(
            (d, next),
            (Duration::from_secs(30), Duration::from_secs(60))
        );
        let (d, next) = retry_delay(Some(MAX_RETRY_DELAY * 2), Duration::from_secs(30));
        assert_eq!((d, next), (MAX_RETRY_DELAY, MAX_RETRY_DELAY));
    }

    fn entry(state: DownloadState) -> Entry {
        let rec = StoredDownload {
            id: 1,
            request: puppy_core::types::AddRequest {
                source: puppy_core::types::Source::Http {
                    url: "http://example.invalid/a".parse().unwrap(),
                },
                output_dir: None,
                filename: None,
                context: Default::default(),
                files: Default::default(),
                seed: None,
            },
            limit: None,
            state: crate::store::StoredState::Queued,
            name: None,
            path: None,
            total_bytes: None,
            created_unix: 1,
            finished_unix: None,
            choose_files_sent: false,
        };
        Entry::new(rec, state, Some(0))
    }

    fn status(state: DownloadState, done: u64) -> puppy_core::types::DownloadStatus {
        puppy_core::types::DownloadStatus {
            state,
            name: "a".into(),
            path: "/tmp/a".into(),
            total_bytes: Some(1000),
            done_bytes: done,
            down_bps: 0,
            up_bps: 0,
            eta_secs: None,
            peers: None,
            note: None,
        }
    }

    fn seeding(uploaded: u64) -> DownloadState {
        DownloadState::Seeding {
            uploaded,
            ratio: 0.0,
            seeded_secs: 0,
            ratio_target: None,
            time_target_secs: None,
        }
    }

    fn drain(rx: &mut broadcast::Receiver<Event>) -> Vec<Event> {
        std::iter::from_fn(|| rx.try_recv().ok()).collect()
    }

    /// Data completing emits `Completed` (once), seeding stopping `SeedingFinished`
    /// (once, however it is learned); new counters alone are no change.
    #[tokio::test]
    async fn seeding_transitions_emit_completed_then_seeding_finished() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path().join("state.json"));
        let m = Manager::start(Vec::new(), store, Config::default())
            .await
            .unwrap();
        let mut rx = m.events();
        let inner = &m.inner;
        let mut fx = Effects::default();
        let mut e = entry(DownloadState::Active);
        e.held = true;
        assert!(inner.transition(1, &mut e, seeding(0), &mut fx));
        assert!(e.held, "a seeding torrent stays with its engine");
        assert!(!inner.transition(1, &mut e, seeding(10), &mut fx));
        assert_eq!(e.state, seeding(10));
        assert!(inner.transition(1, &mut e, DownloadState::Completed, &mut fx));
        assert!(!e.held);
        assert_eq!(fx.drop_completed, vec![(0, 1)]);
        assert_eq!(
            drain(&mut rx),
            vec![
                Event::Completed {
                    id: 1,
                    name: String::new()
                },
                Event::SeedingFinished { id: 1 }
            ]
        );
        // The engine's event afterwards adds nothing.
        inner.lock().entries.insert(1, e);
        inner.on_seeding_finished(1);
        assert_eq!(drain(&mut rx), Vec::<Event>::new());
        // Seeding started and stopped between two looks: the engine's event tells.
        let mut e = entry(DownloadState::Active);
        assert!(inner.transition(2, &mut e, DownloadState::Completed, &mut fx));
        inner.lock().entries.insert(2, e);
        inner.on_seeding_finished(2);
        inner.on_seeding_finished(2);
        assert_eq!(
            drain(&mut rx),
            vec![
                Event::Completed {
                    id: 2,
                    name: String::new()
                },
                Event::SeedingFinished { id: 2 }
            ]
        );
        // Completed already (a restore retried): no second `Completed`.
        let mut e = entry(DownloadState::Paused);
        e.rec.finished_unix = Some(1);
        assert!(inner.transition(3, &mut e, DownloadState::Completed, &mut fx));
        let mut e = entry(DownloadState::Active);
        e.rec.finished_unix = Some(1);
        assert!(inner.transition(4, &mut e, seeding(0), &mut fx));
        assert_eq!(drain(&mut rx), Vec::<Event>::new());
        // A failed re-check clears it: completing again is news again.
        assert!(inner.transition(
            4,
            &mut e,
            DownloadState::NeedsAttention {
                reason: puppy_core::types::AttentionReason::ValidationFailed {
                    detail: puppy_core::types::RECHECK_FAILED.into()
                }
            },
            &mut fx
        ));
        assert_eq!(e.rec.finished_unix, None);
        assert!(matches!(
            drain(&mut rx)[..],
            [Event::NeedsAttention { id: 4, .. }]
        ));
    }

    /// A torrent engine that holds nothing (it lost every download).
    struct Empty;

    #[async_trait::async_trait]
    impl Engine for Empty {
        fn kind(&self) -> puppy_core::engine::EngineKind {
            puppy_core::engine::EngineKind::Torrent
        }
        fn accepts(&self, _: &Source) -> bool {
            true
        }
        async fn add(
            &self,
            id: DownloadId,
            _: puppy_core::types::AddRequest,
            _: puppy_core::engine::StartMode,
            _: puppy_core::engine::AddOptions,
        ) -> Result<puppy_core::engine::Added, EngineError> {
            Err(EngineError::NotFound(id))
        }
        async fn pause(&self, id: DownloadId) -> Result<(), EngineError> {
            Err(EngineError::NotFound(id))
        }
        async fn resume(&self, id: DownloadId) -> Result<(), EngineError> {
            Err(EngineError::NotFound(id))
        }
        async fn remove(&self, id: DownloadId, _: bool) -> Result<(), EngineError> {
            Err(EngineError::NotFound(id))
        }
        fn status(&self, _: DownloadId) -> Option<puppy_core::types::DownloadStatus> {
            None
        }
        fn detail(&self, _: DownloadId) -> Option<puppy_core::types::EngineDetail> {
            None
        }
        fn set_download_limit(&self, _: DownloadId, _: Option<u64>) -> Result<(), EngineError> {
            Ok(())
        }
        fn set_global_download_budget(&self, _: Option<u64>) {}
        fn subscribe(&self) -> broadcast::Receiver<EngineEvent> {
            broadcast::channel(1).1
        }
    }

    #[tokio::test]
    async fn choosing_files_emits_once() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path().join("state.json"));
        let m = Manager::start(Vec::new(), store, Config::default())
            .await
            .unwrap();
        let mut rx = m.events();
        let inner = &m.inner;
        let mut fx = Effects::default();
        let mut e = entry(DownloadState::Active);
        e.rec.name = Some("Big.Show.S01".into());
        assert!(inner.transition(3, &mut e, DownloadState::ChoosingFiles, &mut fx));
        assert_eq!(
            drain(&mut rx),
            vec![Event::ChoosingFiles {
                id: 3,
                name: "Big.Show.S01".into()
            }]
        );
        assert!(inner.transition(3, &mut e, DownloadState::Starting, &mut fx));
        assert!(inner.transition(3, &mut e, DownloadState::ChoosingFiles, &mut fx));
        assert_eq!(drain(&mut rx), Vec::<Event>::new());
    }

    /// A daemon restarting with a torrent still choosing files must not ask again.
    #[tokio::test]
    async fn restored_choosing_files_does_not_notify() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let mut rec = entry(DownloadState::Queued).rec;
        rec.id = 5;
        rec.request.source = puppy_core::types::Source::Magnet {
            uri: "magnet:?xt=urn:btih:0123456789abcdef0123456789abcdef01234567".into(),
        };
        rec.state = crate::store::StoredState::ChoosingFiles;
        let st = crate::store::PersistedState {
            next_id: 6,
            order: vec![5],
            downloads: vec![rec],
            ..Default::default()
        };
        Store::new(path.clone()).save_now(&st).await.unwrap();
        let m = Manager::start(Vec::new(), Store::new(path), Config::default())
            .await
            .unwrap();
        let mut rx = m.events();
        let inner = &m.inner;
        let mut e = inner.lock().entries.remove(&5).expect("restored");
        assert!(e.rec.choose_files_sent);
        let mut fx = Effects::default();
        e.state = DownloadState::Active;
        assert!(inner.transition(5, &mut e, DownloadState::ChoosingFiles, &mut fx));
        assert_eq!(drain(&mut rx), Vec::<Event>::new());
    }

    /// A choosing torrent without a stored path is re-queued to refetch its metadata; if
    /// the daemon stops again before it re-enters choosing, the next one still must not
    /// ask again (the flag is persisted, not only derived from the stored state).
    #[tokio::test]
    async fn interrupted_refetch_does_not_ask_again() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let mut rec = entry(DownloadState::Queued).rec;
        rec.id = 5;
        rec.request.source = puppy_core::types::Source::Magnet {
            uri: "magnet:?xt=urn:btih:0123456789abcdef0123456789abcdef01234567".into(),
        };
        rec.state = crate::store::StoredState::ChoosingFiles;
        let st = crate::store::PersistedState {
            next_id: 6,
            order: vec![5],
            downloads: vec![rec],
            ..Default::default()
        };
        Store::new(path.clone()).save_now(&st).await.unwrap();
        // Run 1: restored (re-queued, no path), stopped before choosing again.
        let m = Manager::start(
            vec![std::sync::Arc::new(Empty)],
            Store::new(path.clone()),
            Config::default(),
        )
        .await
        .unwrap();
        m.shutdown().await;
        let saved = Store::new(path.clone()).load().await.unwrap();
        assert_ne!(
            saved.downloads[0].state,
            crate::store::StoredState::ChoosingFiles
        );
        assert!(saved.downloads[0].choose_files_sent);
        // Run 2: still does not ask when the metadata arrives.
        let m = Manager::start(
            vec![std::sync::Arc::new(Empty)],
            Store::new(path),
            Config::default(),
        )
        .await
        .unwrap();
        let mut rx = m.events();
        let mut e = m.inner.lock().entries.remove(&5).expect("restored");
        assert!(e.rec.choose_files_sent);
        let mut fx = Effects::default();
        e.state = DownloadState::Active;
        assert!(
            m.inner
                .transition(5, &mut e, DownloadState::ChoosingFiles, &mut fx)
        );
        assert_eq!(drain(&mut rx), Vec::<Event>::new());
    }

    /// State written before the flag existed reads as not asked yet.
    #[test]
    fn stored_download_without_flag_defaults_to_false() {
        let rec = entry(DownloadState::Queued).rec;
        let mut v = serde_json::to_value(&rec).unwrap();
        v.as_object_mut().unwrap().remove("choose_files_sent");
        let back: StoredDownload = serde_json::from_value(v).unwrap();
        assert!(!back.choose_files_sent);
    }

    /// A seeding download its engine does not hold is completed, never stuck seeding
    /// (which would also keep the daemon from idling out).
    #[tokio::test]
    async fn seeding_without_its_engine_falls_back_to_completed() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path().join("state.json"));
        let m = Manager::start(vec![Arc::new(Empty)], store, Config::default())
            .await
            .unwrap();
        let inner = &m.inner;
        // Held, but the engine lost it: the next look completes it.
        let mut e = entry(seeding(0));
        e.held = true;
        e.rec.finished_unix = Some(1);
        inner.lock().entries.insert(1, e);
        assert!(!m.is_idle());
        inner.on_engine_change(0, None);
        assert_eq!(inner.lock().entries[&1].state, DownloadState::Completed);
        assert!(!inner.lock().entries[&1].held);
        // Not held at all: pausing it completes it.
        let mut e = entry(seeding(0));
        e.rec.finished_unix = Some(1);
        inner.lock().entries.insert(2, e);
        m.pause(2).await.unwrap();
        assert_eq!(inner.lock().entries[&2].state, DownloadState::Completed);
        assert!(m.is_idle());
    }

    #[test]
    fn new_states_are_stored_as_themselves() {
        assert_eq!(stored_state(&seeding(5)), StoredState::Seeding);
        assert_eq!(
            stored_state(&DownloadState::ChoosingFiles),
            StoredState::ChoosingFiles
        );
        assert!(!is_active(&seeding(0)) && !is_active(&DownloadState::ChoosingFiles));
    }

    /// The backoff survives a retry that gets nowhere, but is reset once the retried
    /// download is active and actually transfers.
    #[test]
    fn progress_after_a_retry_resets_the_backoff() {
        let mut e = entry(DownloadState::Failed {
            reason: "x".into(),
            transient: true,
        });
        e.backoff = Some(Duration::from_secs(8));
        // Seen while failed, then active without progress: kept.
        let seen = status(e.state.clone(), 100);
        Inner::record_status(&mut e, &seen);
        e.state = DownloadState::Active;
        Inner::record_status(&mut e, &status(DownloadState::Active, 100));
        assert_eq!(e.backoff, Some(Duration::from_secs(8)));
        // Progress while not active (the engine's view) does not count.
        e.state = DownloadState::Starting;
        Inner::record_status(&mut e, &status(DownloadState::Active, 150));
        assert_eq!(e.backoff, Some(Duration::from_secs(8)));
        // Active and transferring: reset.
        e.state = DownloadState::Active;
        Inner::record_status(&mut e, &status(DownloadState::Active, 200));
        assert_eq!(e.backoff, None);
    }

    /// A download first seen active (e.g. after a restore) gets its mark then.
    #[test]
    fn backoff_reset_needs_progress_beyond_the_first_sighting() {
        let mut e = entry(DownloadState::Active);
        e.backoff = Some(Duration::from_secs(2));
        Inner::record_status(&mut e, &status(DownloadState::Active, 500));
        assert_eq!(e.backoff, Some(Duration::from_secs(2)));
        Inner::record_status(&mut e, &status(DownloadState::Active, 501));
        assert_eq!(e.backoff, None);
    }

    /// A magnet restored from its link reports its `dn` until the metadata is back: that
    /// placeholder does not replace the real name already recorded.
    #[test]
    fn magnet_placeholder_names_do_not_replace_known_ones() {
        let mut e = entry(DownloadState::Paused);
        e.rec.request.source = puppy_core::types::Source::Magnet {
            uri: "magnet:?xt=urn:btih:0123456789abcdef0123456789abcdef01234567&dn=dn".into(),
        };
        let mut st = status(DownloadState::Paused, 0);
        st.name = "dn".into();
        st.total_bytes = None;
        // Nothing known yet: the placeholder is better than nothing.
        Inner::record_status(&mut e, &st);
        assert_eq!(e.rec.name.as_deref(), Some("dn"));
        e.rec.name = Some("Real Name".into());
        Inner::record_status(&mut e, &st);
        assert_eq!(e.rec.name.as_deref(), Some("Real Name"));
        // Once the metadata is known, the engine's name wins.
        st.name = "Real Name 2".into();
        st.total_bytes = Some(1);
        Inner::record_status(&mut e, &st);
        assert_eq!(e.rec.name.as_deref(), Some("Real Name 2"));
        // Not a magnet: always taken.
        let mut e = entry(DownloadState::Paused);
        e.rec.name = Some("old".into());
        st.total_bytes = None;
        Inner::record_status(&mut e, &st);
        assert_eq!(e.rec.name.as_deref(), Some("Real Name 2"));
    }
}
