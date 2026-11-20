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
//!   doubling per failure up to [`MAX_RETRY_DELAY`], for as long as the daemon runs.
//!   Pausing cancels a pending retry; resuming (or restarting) resets the delay.
//! - Engine events only mean "look again": the manager re-reads `engine.status()` instead
//!   of trusting event payloads, and ignores events for downloads with a job in flight
//!   (the job reads the status itself once the engine call returns). Events may thus
//!   arrive before `add` returns, late, or be lost to lag without corrupting state.

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
use puppy_core::types::{DownloadId, DownloadState};
use puppy_proto::{ErrorCode, Event};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::time::Duration;
use tokio::sync::broadcast;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

const EVENT_CAPACITY: usize = 256;
const TICK: Duration = Duration::from_secs(1);

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
        }
    }

    /// Forgets a pending retry and resets the backoff (a user resumed or restarted).
    fn reset_retry(&mut self) {
        self.retry = None;
        self.backoff = None;
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
    /// Completed downloads the engine should drop (`remove(id, false)`).
    drop_completed: Vec<(usize, DownloadId)>,
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

fn is_active(s: &DownloadState) -> bool {
    matches!(s, DownloadState::Starting | DownloadState::Active)
}

fn stored_state(s: &DownloadState) -> StoredState {
    match s {
        DownloadState::Queued => StoredState::Queued,
        // A download being started is persisted as active: after a restart it is
        // restored (if it got a path) and queued at the front.
        DownloadState::Starting | DownloadState::Active => StoredState::Active,
        DownloadState::Paused => StoredState::Paused,
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

    /// No download is active, starting, queued or waiting for an automatic retry.
    pub fn is_idle(&self) -> bool {
        self.inner.lock().entries.values().all(|e| {
            e.retry.is_none()
                && !matches!(
                    e.state,
                    DownloadState::Queued | DownloadState::Starting | DownloadState::Active
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

    /// Moves `e` to `next`, emitting the matching event. Returns whether it changed.
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
        match &next {
            DownloadState::Completed => {
                e.rec.finished_unix = Some(now_unix());
                if e.held
                    && let Some(engine) = e.engine
                {
                    fx.drop_completed.push((engine, id));
                }
                e.held = false;
                self.emit(Event::Completed {
                    id,
                    name: e.rec.name.clone().unwrap_or_default(),
                });
            }
            DownloadState::Failed { reason, .. } => self.emit(Event::Failed {
                id,
                reason: reason.clone(),
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

    /// Schedules the automatic retry of a transiently failed download: after the
    /// current backoff, which then doubles (up to [`MAX_RETRY_DELAY`]).
    fn arm_retry(&self, id: DownloadId, e: &mut Entry) {
        let delay = e
            .backoff
            .unwrap_or(self.cfg.retry_base)
            .min(MAX_RETRY_DELAY);
        e.backoff = Some((delay * 2).min(MAX_RETRY_DELAY));
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

    /// Copies what the engine knows about name, path and size into the record.
    fn record_status(e: &mut Entry, status: &puppy_core::types::DownloadStatus) {
        e.rec.name = Some(status.name.clone());
        e.rec.path = Some(status.path.clone());
        if status.total_bytes.is_some() {
            e.rec.total_bytes = status.total_bytes;
        }
    }

    /// Applies the engine's view to an engine-held download with no job in flight.
    /// Only transitions the engine makes on its own are taken over (`Completed`,
    /// `Failed`, `NeedsAttention`); `Paused`/`Active` are only ever caused by the
    /// manager, which records them itself (a `Paused` seen while the manager says
    /// `Active` is the transient moment between `resume` and the transfer starting).
    fn reconcile(&self, id: DownloadId, e: &mut Entry, fx: &mut Effects) -> bool {
        let (Some(engine), true, None) = (e.engine, e.held, &e.job) else {
            return false;
        };
        let Some(status) = self.engines[engine].status(id) else {
            return false;
        };
        Self::record_status(e, &status);
        let next = match (&e.state, status.state) {
            (_, DownloadState::Completed) => DownloadState::Completed,
            (DownloadState::Active, s @ DownloadState::Failed { .. }) => s,
            (
                DownloadState::Active | DownloadState::Paused | DownloadState::Queued,
                s @ DownloadState::NeedsAttention { .. },
            ) => s,
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
                if let Err(e) = engine.remove(id, false).await
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
            let to_pause: Vec<(usize, DownloadId)> = st
                .entries
                .iter()
                .filter(|(_, e)| e.held || e.job.is_some())
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
        self.persist(&self.lock());
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
}
