//! User commands: add, pause, resume, remove, restart, reorder, limits, and a torrent's
//! files and seeding policy.

use super::schedule::JobKind;
use super::{
    Effects, Entry, Inner, Manager, ManagerError, State, is_finished, normalize_limit, now_unix,
};
use crate::store::{StoredDownload, StoredState};
use puppy_core::engine::{EngineError, EngineKind};
use puppy_core::http::resume::{part_path, sidecar_path};
use puppy_core::types::{
    AddRequest, DownloadId, DownloadState, SeedPolicy, Selection, Source, TorrentFile,
};
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

/// Only failed downloads or downloads needing attention, with no job in flight, can
/// be restarted.
fn check_restartable(e: &Entry) -> Result<()> {
    if !matches!(
        e.state,
        DownloadState::Failed { .. } | DownloadState::NeedsAttention { .. }
    ) {
        return Err(ManagerError::InvalidState(
            "only failed downloads or downloads needing attention can be restarted".into(),
        ));
    }
    if e.job.is_some() {
        return Err(ManagerError::InvalidState(
            "the download is being restored; try again shortly".into(),
        ));
    }
    Ok(())
}

/// The engine of a torrent (others have no files to choose and do not seed).
fn torrent_engine(inner: &Inner, e: &Entry) -> Result<usize> {
    e.engine
        .filter(|&i| inner.engines[i].kind() == EngineKind::Torrent)
        .ok_or(ManagerError::Engine(EngineError::Unsupported(
            "only torrents have files to choose and seeding",
        )))
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
        // What the engine can check now fails the add before it gets an id.
        match inner.engines[engine].validate_add(&req).await {
            Ok(()) => {}
            Err(EngineError::Invalid(m)) => return Err(ManagerError::BadRequest(m)),
            Err(e) => return Err(e.into()),
        }
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
            let rec = StoredDownload {
                id,
                request: req,
                limit: normalize_limit(limit),
                state: StoredState::Queued,
                name: None,
                path: None,
                total_bytes: None,
                created_unix: now_unix(),
                finished_unix: None,
            };
            st.entries.insert(id, Entry::new(rec, state, Some(engine)));
            st.order.push(id);
            inner.persist(&st);
            inner.emit(Event::Added { id });
            id
        };
        inner.schedule();
        Ok(id)
    }

    /// Queued ⇒ paused (daemon-level); transferring ⇒ paused in the engine; starting ⇒
    /// paused as soon as the engine call returns; failed with an automatic retry pending
    /// ⇒ paused (the retry is cancelled); seeding ⇒ seeding stops (`Completed`);
    /// choosing files ⇒ nothing changes. Completed ⇒ `InvalidState`.
    pub async fn pause(&self, id: DownloadId) -> Result<()> {
        let inner = &self.inner;
        let (engine, was_active) = {
            let mut st = inner.lock();
            let e = entry_mut(&mut st, id)?;
            match e.state {
                DownloadState::Completed => {
                    return Err(ManagerError::InvalidState("download is completed".into()));
                }
                // Being restored: stopped once the engine call returns.
                DownloadState::Seeding { .. } if e.job.is_some() => {
                    if let Some(job) = &mut e.job {
                        job.pause = true;
                    }
                    (None, false)
                }
                DownloadState::Seeding { .. } => (e.held.then_some(e.engine).flatten(), true),
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
                DownloadState::Failed { .. } if e.retry.is_some() => {
                    e.retry = None;
                    e.state = DownloadState::Paused;
                    inner.persist(&st);
                    (None, false)
                }
                DownloadState::Paused
                | DownloadState::ChoosingFiles
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
    /// free, else queued. Resets the automatic-retry backoff. NeedsAttention ⇒
    /// `InvalidState` (restart it instead), except a seeding torrent whose data failed
    /// its check (resumed like a paused one). Completed/Seeding ⇒ `InvalidState`;
    /// ChoosingFiles ⇒ `InvalidState` (choose files first).
    pub async fn resume(&self, id: DownloadId) -> Result<()> {
        let inner = &self.inner;
        let job = {
            let mut st = inner.lock();
            let e = entry_mut(&mut st, id)?;
            // A seeding torrent's data that failed its check downloads again, through
            // the queue like any download.
            let resumable = matches!(
                &e.state,
                DownloadState::NeedsAttention { reason } if reason.is_resumable()
            ) && e.job.is_none();
            match e.state {
                DownloadState::Completed | DownloadState::Seeding { .. } => {
                    return Err(ManagerError::InvalidState("download is completed".into()));
                }
                DownloadState::ChoosingFiles => {
                    return Err(ManagerError::InvalidState(
                        "choose files first: puppy select <id> ...".into(),
                    ));
                }
                DownloadState::NeedsAttention { .. } if !resumable => {
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
                DownloadState::Paused
                | DownloadState::Failed { .. }
                | DownloadState::NeedsAttention { .. } => {
                    e.reset_retry();
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
    /// including a completed download's: the engine that downloaded it deletes them, see
    /// `Engine::delete_completed`) and from the list. An engine that no longer holds the
    /// download is told to forget it (`Engine::forget`).
    pub async fn remove(&self, id: DownloadId, delete_files: bool) -> Result<()> {
        let inner = &self.inner;
        let (engine, final_files, forget) = {
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
            let in_engine = e.held || e.job.is_some();
            let engine = in_engine.then_some(e.engine).flatten();
            // Not held: the engine dropped it (completed) or never had it (or lost it).
            let forget = (!in_engine).then_some(e.engine).flatten();
            let final_files = (delete_files && e.state == DownloadState::Completed)
                .then_some(e.rec.path)
                .flatten()
                .zip(forget);
            inner.persist(&st);
            inner.emit(Event::Removed { id });
            (engine, final_files, forget)
        };
        if let Some(engine) = engine
            && let Err(e) = ignore_not_found(inner.engines[engine].remove(id, delete_files).await)
        {
            tracing::warn!(id, "removing from the engine: {e}");
        }
        if let Some((path, engine)) = final_files
            && let Err(e) = inner.engines[engine].delete_completed(id, &path).await
        {
            tracing::warn!(path = %path.display(), "cannot delete: {e}");
        }
        if let Some(engine) = forget {
            inner.engines[engine].forget(id).await;
        }
        inner.schedule();
        Ok(())
    }

    /// NeedsAttention/Failed ⇒ drops the engine's files and starts over (same id, fresh
    /// probe), through the queue.
    ///
    /// A download the engine does not hold (e.g. its restore failed) but whose files
    /// are known has its partial files (`.part`, `.part.puppy`) deleted first, unless
    /// another download currently has the same path (then they are that one's).
    pub async fn restart(&self, id: DownloadId) -> Result<()> {
        let inner = &self.inner;
        let stale = {
            let mut st = inner.lock();
            let e = entry_mut(&mut st, id)?;
            check_restartable(e)?;
            let stale = match (&e.rec.request.source, &e.rec.path) {
                (Source::Http { .. }, Some(path)) if !e.held => Some(path.clone()),
                _ => None,
            };
            stale.filter(|path| {
                !st.entries
                    .iter()
                    .any(|(other, o)| *other != id && o.rec.path.as_ref() == Some(path))
            })
        };
        if let Some(final_path) = stale {
            for p in [part_path(&final_path), sidecar_path(&final_path)] {
                match tokio::fs::remove_file(&p).await {
                    Ok(()) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => tracing::warn!(path = %p.display(), "cannot delete: {e}"),
                }
            }
        }
        let job = {
            let mut st = inner.lock();
            let e = entry_mut(&mut st, id)?;
            // Checked again: the state may have changed while the files were deleted.
            check_restartable(e)?;
            e.reset_retry();
            let job = if e.held {
                Inner::begin_job(id, e, JobKind::Reset)
            } else {
                e.rec.name = None;
                e.rec.path = None;
                e.rec.total_bytes = None;
                e.rec.finished_unix = None;
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
    /// (clamped). Completed/Seeding ⇒ `InvalidState`.
    pub async fn move_to(&self, id: DownloadId, position: usize) -> Result<()> {
        let inner = &self.inner;
        {
            let mut st = inner.lock();
            let e = entry_mut(&mut st, id)?;
            if is_finished(&e.state) {
                return Err(ManagerError::InvalidState("download is completed".into()));
            }
            let st = &mut *st;
            let finished = |x: &DownloadId| st.entries.get(x).map(|e| is_finished(&e.state));
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
    /// engine holds the download. A limit the engine cannot take now
    /// (`EngineError::Unsupported`, e.g. a running torrent) is not kept: the previous one
    /// stays and the error is returned. Other engine errors are returned with the limit
    /// stored (and used from the next start).
    pub fn set_limit(&self, id: DownloadId, bps: Option<u64>) -> Result<()> {
        let inner = &self.inner;
        let bps = normalize_limit(bps);
        let mut st = inner.lock();
        let e = entry_mut(&mut st, id)?;
        let previous = std::mem::replace(&mut e.rec.limit, bps);
        let result = match e.held.then_some(e.engine).flatten() {
            Some(engine) => ignore_not_found(inner.engines[engine].set_download_limit(id, bps)),
            None => Ok(()),
        };
        if let Err(ManagerError::Engine(EngineError::Unsupported(_))) = &result {
            e.rec.limit = previous;
        } else {
            inner.persist(&st);
        }
        result
    }

    /// Global limits in bytes/s (`None` or 0: unlimited). The download limit is divided
    /// across engines; the upload limit goes to every engine as is.
    pub fn set_global_limits(&self, down: Option<u64>, up: Option<u64>) {
        let inner = &self.inner;
        {
            let mut st = inner.lock();
            st.settings.global_down_limit = normalize_limit(down);
            st.settings.global_up_limit = normalize_limit(up);
            inner.persist(&st);
        }
        inner.rebalance();
        inner.apply_upload_budget();
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

    /// A torrent's files, once its contents are known (not for other downloads:
    /// `Unsupported`). `InvalidState` while they are not known, or once its engine no
    /// longer holds it (completed).
    pub async fn list_files(&self, id: DownloadId) -> Result<Vec<TorrentFile>> {
        let inner = &self.inner;
        let engine = {
            let mut st = inner.lock();
            let e = entry_mut(&mut st, id)?;
            torrent_engine(inner, e)?
        };
        match inner.engines[engine].list_files(id).await {
            Err(EngineError::NotFound(_)) => Err(ManagerError::InvalidState(
                "the torrent's files are not known now".into(),
            )),
            Err(EngineError::Invalid(m)) => Err(ManagerError::InvalidState(m)),
            r => r.map_err(Into::into),
        }
    }

    /// Makes the one-time file choice of a torrent choosing files (else `InvalidState`;
    /// an empty or unusable selection ⇒ `BadRequest`). It is then queued at the front
    /// and starts as soon as a slot is free.
    pub async fn select_files(&self, id: DownloadId, selection: Selection) -> Result<()> {
        let inner = &self.inner;
        let engine = {
            let mut st = inner.lock();
            let e = entry_mut(&mut st, id)?;
            let engine = torrent_engine(inner, e)?;
            if e.state != DownloadState::ChoosingFiles {
                return Err(ManagerError::InvalidState("not choosing files".into()));
            }
            if selection.indices.is_empty() && selection.globs.is_empty() {
                return Err(ManagerError::BadRequest("no files selected".into()));
            }
            if !e.held || e.job.is_some() {
                return Err(ManagerError::InvalidState(
                    "the download is being restored; try again shortly".into(),
                ));
            }
            engine
        };
        let eng = &inner.engines[engine];
        match eng.select_files(id, selection).await {
            Ok(()) => {}
            Err(EngineError::Invalid(m)) => {
                // Unusable here, unless it is no longer choosing (chosen meanwhile).
                let choosing = eng
                    .status(id)
                    .is_some_and(|s| s.state == DownloadState::ChoosingFiles);
                return Err(if choosing {
                    ManagerError::BadRequest(m)
                } else {
                    ManagerError::InvalidState("not choosing files".into())
                });
            }
            Err(EngineError::NotFound(_)) => return Err(ManagerError::NotFound(id)),
            Err(e) => return Err(e.into()),
        }
        {
            let mut st = inner.lock();
            // Removed meanwhile: its engine removes it (calls on it are serialized).
            if let Some(e) = st.entries.get_mut(&id)
                && e.state == DownloadState::ChoosingFiles
            {
                e.state = DownloadState::Queued;
                st.order.retain(|x| *x != id);
                st.order.insert(0, id);
                inner.persist(&st);
            }
        }
        inner.schedule();
        Ok(())
    }

    /// Sets a torrent's own seeding targets, replacing its whole policy: a `None` field
    /// inherits the global value (read live). Applied at once if its engine holds it,
    /// and kept with the download (restore uses it) once the engine took it. A negative
    /// or non-finite ratio ⇒ `BadRequest`; a completed torrent (seeding is over) ⇒
    /// `InvalidState`.
    pub async fn set_seed(&self, id: DownloadId, policy: SeedPolicy) -> Result<()> {
        if policy.ratio.is_some_and(|r| !r.is_finite() || r < 0.0) {
            return Err(ManagerError::BadRequest(
                "the seed ratio must be a number of at least 0".into(),
            ));
        }
        let inner = &self.inner;
        let engine = {
            let mut st = inner.lock();
            let e = entry_mut(&mut st, id)?;
            let engine = torrent_engine(inner, e)?;
            if e.state == DownloadState::Completed {
                return Err(ManagerError::InvalidState("seeding has finished".into()));
            }
            // Not held: used when it is added (an add in flight catches up).
            e.held.then_some(engine)
        };
        if let Some(engine) = engine {
            ignore_not_found(inner.engines[engine].set_seed(id, policy.clone()).await)?;
        }
        let mut st = inner.lock();
        if let Some(e) = st.entries.get_mut(&id) {
            e.rec.request.seed = Some(policy);
            inner.persist(&st);
        }
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
    /// After the engine paused a transferring or seeding download: record what it says
    /// now (a seeding one is `Completed`).
    fn settle_paused(&self, id: DownloadId) {
        let mut fx = Effects::default();
        {
            let mut st = self.lock();
            let Some(e) = st.entries.get_mut(&id) else {
                return;
            };
            let seeding = matches!(e.state, DownloadState::Seeding { .. });
            let status = match (e.engine, e.held, &e.job, &e.state) {
                (
                    Some(engine),
                    true,
                    None,
                    DownloadState::Active | DownloadState::Seeding { .. },
                ) => self.engines[engine].status(id),
                (_, _, None, DownloadState::Seeding { .. }) => None,
                _ => return,
            };
            let Some(status) = status else {
                // Nothing seeds a seeding download its engine does not hold.
                if seeding {
                    e.held = false;
                    if self.transition(id, e, DownloadState::Completed, &mut fx) {
                        self.persist(&st);
                    }
                }
                drop(st);
                self.apply(fx);
                return;
            };
            Self::record_status(e, &status);
            let next = match status.state {
                DownloadState::Paused if seeding => return,
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
