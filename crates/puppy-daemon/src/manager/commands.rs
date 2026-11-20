//! User commands: add, pause, resume, remove, restart, reorder and limits.

use super::schedule::JobKind;
use super::{Effects, Entry, Inner, Manager, ManagerError, State, normalize_limit, now_unix};
use crate::store::{StoredDownload, StoredState};
use puppy_core::engine::EngineError;
use puppy_core::types::{AddRequest, DownloadId, DownloadState};
use puppy_proto::{Event, Settings};

type Result<T> = std::result::Result<T, ManagerError>;

fn entry_mut(st: &mut State, id: DownloadId) -> Result<&mut Entry> {
    if st.shutting_down {
        return Err(ManagerError::InvalidState(
            "the daemon is shutting down".into(),
        ));
    }
    st.entries.get_mut(&id).ok_or(ManagerError::NotFound(id))
}

/// Engine errors that only mean the engine no longer holds the download.
fn ignore_not_found(r: std::result::Result<(), EngineError>) -> Result<()> {
    match r {
        Ok(()) | Err(EngineError::NotFound(_)) => Ok(()),
        Err(e) => Err(e.into()),
    }
}

impl Manager {
    /// Adds a download as `Queued` (or `Paused` with `start_paused`) and returns its id at
    /// once; the engine probe runs later, in a job started by the scheduler.
    pub async fn add(
        &self,
        req: AddRequest,
        start_paused: bool,
        limit: Option<u64>,
    ) -> Result<DownloadId> {
        let inner = &self.inner;
        let engine = inner
            .engines
            .iter()
            .position(|e| e.accepts(&req.source))
            .ok_or_else(|| ManagerError::BadRequest("unsupported source".into()))?;
        let id = {
            let mut st = inner.lock();
            if st.shutting_down {
                return Err(ManagerError::InvalidState(
                    "the daemon is shutting down".into(),
                ));
            }
            let id = st.next_id;
            st.next_id += 1;
            let state = if start_paused {
                DownloadState::Paused
            } else {
                DownloadState::Queued
            };
            st.entries.insert(
                id,
                Entry {
                    rec: StoredDownload {
                        id,
                        request: req,
                        limit: normalize_limit(limit),
                        state: StoredState::Queued,
                        name: None,
                        path: None,
                        total_bytes: None,
                        created_unix: now_unix(),
                        finished_unix: None,
                    },
                    state,
                    engine: Some(engine),
                    held: false,
                    job: None,
                },
            );
            st.order.push(id);
            inner.persist(&st);
            inner.emit(Event::Added { id });
            id
        };
        inner.schedule();
        Ok(id)
    }

    /// Queued ⇒ paused (daemon-level); transferring ⇒ paused in the engine; starting ⇒
    /// paused as soon as the engine call returns. Completed ⇒ `InvalidState`.
    pub async fn pause(&self, id: DownloadId) -> Result<()> {
        let inner = &self.inner;
        let (engine, was_active) = {
            let mut st = inner.lock();
            let e = entry_mut(&mut st, id)?;
            match e.state {
                DownloadState::Completed => {
                    return Err(ManagerError::InvalidState("download is completed".into()));
                }
                DownloadState::Queued => {
                    e.state = DownloadState::Paused;
                    inner.persist(&st);
                    (None, false)
                }
                DownloadState::Starting => {
                    if let Some(job) = &mut e.job {
                        job.pause = true;
                        job.interrupted = true;
                    }
                    // An engine-held download is validating in `resume`: interrupt it.
                    (e.held.then_some(e.engine).flatten(), false)
                }
                DownloadState::Active => (e.engine, true),
                DownloadState::Paused
                | DownloadState::Failed { .. }
                | DownloadState::NeedsAttention { .. } => (None, false),
            }
        };
        if let Some(engine) = engine {
            ignore_not_found(inner.engines[engine].pause(id).await)?;
        }
        if was_active {
            inner.settle_paused(id);
        }
        inner.schedule();
        Ok(())
    }

    /// Paused/Failed ⇒ resumed in the engine now if it holds the download and a slot is
    /// free, else queued. NeedsAttention ⇒ `InvalidState` (restart it instead).
    pub async fn resume(&self, id: DownloadId) -> Result<()> {
        let inner = &self.inner;
        let job = {
            let mut st = inner.lock();
            let e = entry_mut(&mut st, id)?;
            match e.state {
                DownloadState::Completed => {
                    return Err(ManagerError::InvalidState("download is completed".into()));
                }
                DownloadState::NeedsAttention { .. } => {
                    return Err(ManagerError::InvalidState(
                        "download needs attention; use restart".into(),
                    ));
                }
                DownloadState::Starting => {
                    if let Some(job) = &mut e.job {
                        job.pause = false;
                    }
                    None
                }
                DownloadState::Queued | DownloadState::Active => None,
                DownloadState::Paused | DownloadState::Failed { .. } => {
                    let held = e.held;
                    let job = if held {
                        Inner::start_now(&mut st, id)
                    } else {
                        None
                    };
                    if job.is_none()
                        && let Some(e) = st.entries.get_mut(&id)
                    {
                        e.state = DownloadState::Queued;
                    }
                    inner.persist(&st);
                    job
                }
            }
        };
        if let Some(job) = job {
            inner.spawn(job);
        }
        inner.schedule();
        Ok(())
    }

    /// Removes the download from the engine (deleting its files with `delete_files`,
    /// including a completed download's file) and from the list.
    pub async fn remove(&self, id: DownloadId, delete_files: bool) -> Result<()> {
        let inner = &self.inner;
        let (engine, final_file) = {
            let mut st = inner.lock();
            entry_mut(&mut st, id)?;
            let e = st.entries.remove(&id).expect("checked above");
            st.order.retain(|x| *x != id);
            if e.job.is_some()
                && let Some(engine) = e.engine
            {
                // The job removes it from the engine once its engine call returns.
                st.orphans.insert(id, (engine, delete_files));
            }
            let engine = (e.held || e.job.is_some()).then_some(e.engine).flatten();
            let final_file = (delete_files && e.state == DownloadState::Completed)
                .then_some(e.rec.path)
                .flatten();
            inner.persist(&st);
            inner.emit(Event::Removed { id });
            (engine, final_file)
        };
        if let Some(engine) = engine
            && let Err(e) = ignore_not_found(inner.engines[engine].remove(id, delete_files).await)
        {
            tracing::warn!(id, "removing from the engine: {e}");
        }
        if let Some(path) = final_file {
            match tokio::fs::remove_file(&path).await {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => tracing::warn!(path = %path.display(), "cannot delete: {e}"),
            }
        }
        inner.schedule();
        Ok(())
    }

    /// NeedsAttention/Failed ⇒ drops the engine's files and starts over (same id, fresh
    /// probe), through the queue.
    pub async fn restart(&self, id: DownloadId) -> Result<()> {
        let inner = &self.inner;
        let job = {
            let mut st = inner.lock();
            let e = entry_mut(&mut st, id)?;
            if !matches!(
                e.state,
                DownloadState::Failed { .. } | DownloadState::NeedsAttention { .. }
            ) {
                return Err(ManagerError::InvalidState(
                    "only failed downloads or downloads needing attention can be restarted".into(),
                ));
            }
            let job = if e.held {
                Inner::begin_job(id, e, JobKind::Reset)
            } else {
                e.rec.name = None;
                e.rec.path = None;
                e.rec.total_bytes = None;
                e.state = DownloadState::Queued;
                None
            };
            inner.persist(&st);
            job
        };
        if let Some(job) = job {
            inner.spawn(job);
        }
        inner.schedule();
        Ok(())
    }

    /// Moves an unfinished download to `position` among the unfinished downloads
    /// (clamped). Completed ⇒ `InvalidState`.
    pub async fn move_to(&self, id: DownloadId, position: usize) -> Result<()> {
        let inner = &self.inner;
        {
            let mut st = inner.lock();
            let e = entry_mut(&mut st, id)?;
            if e.state == DownloadState::Completed {
                return Err(ManagerError::InvalidState("download is completed".into()));
            }
            let st = &mut *st;
            let finished = |x: &DownloadId| {
                st.entries
                    .get(x)
                    .map(|e| e.state == DownloadState::Completed)
            };
            let (mut unfinished, done): (Vec<_>, Vec<_>) = st
                .order
                .iter()
                .filter(|x| **x != id)
                .partition(|x| finished(x) == Some(false));
            unfinished.insert(position.min(unfinished.len()), id);
            unfinished.extend(done);
            st.order = unfinished;
            inner.persist(st);
        }
        inner.schedule();
        Ok(())
    }

    /// Per-download limit in bytes/s (`None` or 0: unlimited); applied at once if the
    /// engine holds the download.
    pub fn set_limit(&self, id: DownloadId, bps: Option<u64>) -> Result<()> {
        let inner = &self.inner;
        let bps = normalize_limit(bps);
        let mut st = inner.lock();
        let e = entry_mut(&mut st, id)?;
        e.rec.limit = bps;
        if e.held
            && let Some(engine) = e.engine
        {
            ignore_not_found(inner.engines[engine].set_download_limit(id, bps))?;
        }
        inner.persist(&st);
        Ok(())
    }

    /// Global limits in bytes/s (`None` or 0: unlimited). The download limit is divided
    /// across engines; the upload limit is only stored for now.
    pub fn set_global_limits(&self, down: Option<u64>, up: Option<u64>) {
        let inner = &self.inner;
        {
            let mut st = inner.lock();
            st.settings.global_down_limit = normalize_limit(down);
            st.settings.global_up_limit = normalize_limit(up);
            inner.persist(&st);
        }
        inner.rebalance();
    }

    /// At most `n` downloads starting or active at once. Lowering it does not pause
    /// running downloads; it only holds back new starts.
    pub fn set_max_active(&self, n: u16) -> Result<()> {
        if n == 0 {
            return Err(ManagerError::BadRequest(
                "max_active must be at least 1".into(),
            ));
        }
        let inner = &self.inner;
        {
            let mut st = inner.lock();
            st.settings.max_active = n;
            inner.persist(&st);
        }
        inner.schedule();
        Ok(())
    }

    pub fn settings(&self) -> Settings {
        let inner = &self.inner;
        let st = inner.lock();
        Settings {
            max_active: st.settings.max_active,
            global_down_limit: st.settings.global_down_limit,
            global_up_limit: st.settings.global_up_limit,
            download_dir: inner.cfg.download_dir.clone(),
            connections: inner.cfg.connections,
            theme: inner.cfg.theme.clone(),
        }
    }
}

impl Inner {
    /// After the engine paused a transferring download: record what it says now.
    fn settle_paused(&self, id: DownloadId) {
        let mut fx = Effects::default();
        {
            let mut st = self.lock();
            let Some(e) = st.entries.get_mut(&id) else {
                return;
            };
            let (Some(engine), true, None, DownloadState::Active) =
                (e.engine, e.held, &e.job, &e.state)
            else {
                return;
            };
            let Some(status) = self.engines[engine].status(id) else {
                return;
            };
            Self::record_status(e, &status);
            let next = match status.state {
                s @ (DownloadState::Paused
                | DownloadState::Completed
                | DownloadState::Failed { .. }
                | DownloadState::NeedsAttention { .. }) => s,
                // Still running: something resumed it meanwhile.
                _ => return,
            };
            if self.transition(id, e, next, &mut fx) {
                self.persist(&st);
            }
        }
        self.apply(fx);
    }
}
