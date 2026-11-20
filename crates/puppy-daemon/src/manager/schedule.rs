//! Starting downloads: the scheduler and the jobs that run long engine calls.

use super::{Effects, Entry, Inner, JobCtl, State};
use puppy_core::engine::{AddOptions, Added, EngineError, StartMode};
use puppy_core::types::{AddRequest, DownloadId, DownloadState};
use std::sync::Arc;

/// A long engine call, run outside the state lock.
pub(super) enum JobKind {
    /// `engine.add`: a fresh start, or a restore from the files at the recorded path.
    Add {
        req: Box<AddRequest>,
        mode: StartMode,
        opts: AddOptions,
    },
    /// `engine.resume` of an engine-held download (validates the remote file).
    Resume,
    /// `engine.remove(id, true)` before starting over (restart).
    Reset,
    /// `engine.add` of a stored download after a daemon restart, paused (local checks
    /// only). Unlike the others it leaves the download's state alone while it runs.
    Restore {
        req: Box<AddRequest>,
        mode: StartMode,
        opts: AddOptions,
    },
}

pub(super) struct Job {
    id: DownloadId,
    engine: usize,
    kind: JobKind,
}

enum Outcome {
    Added,
    AddFailed(EngineError),
    Resumed(Result<(), EngineError>),
    Reset,
    Restored,
    RestoreFailed(EngineError),
}

impl Inner {
    /// Marks `e` as `Starting` with a job in flight and returns the job to spawn.
    pub(super) fn begin_job(id: DownloadId, e: &mut Entry, kind: JobKind) -> Option<Job> {
        let engine = e.engine?;
        e.state = DownloadState::Starting;
        e.job = Some(JobCtl::default());
        Some(Job { id, engine, kind })
    }

    /// Marks `e` as being restored (its state is left as is) and returns the job.
    pub(super) fn begin_restore(id: DownloadId, e: &mut Entry, kind: JobKind) -> Option<Job> {
        let engine = e.engine?;
        e.job = Some(JobCtl::default());
        Some(Job { id, engine, kind })
    }

    /// The job that starts a queued download.
    fn start_job(id: DownloadId, e: &mut Entry) -> Option<Job> {
        let kind = if e.held {
            JobKind::Resume
        } else {
            let mode = match &e.rec.path {
                Some(path) => StartMode::Restore { path: path.clone() },
                None => StartMode::Fresh,
            };
            JobKind::Add {
                req: Box::new(e.rec.request.clone()),
                mode,
                opts: AddOptions {
                    start_paused: false,
                    limit: e.rec.limit,
                },
            }
        };
        Self::begin_job(id, e, kind)
    }

    /// Starts queued downloads, in queue order, while fewer than `max_active` are
    /// starting or active (a queued download still being restored holds a slot, so
    /// nothing overtakes it); then rebalances the global limit. Never blocks.
    pub(super) fn schedule(self: &Arc<Self>) {
        let jobs = {
            let mut st = self.lock();
            if st.shutting_down {
                return;
            }
            let jobs = Self::pick_starts(&mut st);
            if !jobs.is_empty() {
                self.persist(&st);
            }
            jobs
        };
        for job in jobs {
            self.spawn(job);
        }
        self.rebalance();
    }

    fn pick_starts(st: &mut State) -> Vec<Job> {
        let max = usize::from(st.settings.max_active);
        let mut active = st.active_count();
        let mut jobs = Vec::new();
        for id in &st.order {
            if active >= max {
                break;
            }
            let Some(e) = st.entries.get_mut(id) else {
                continue;
            };
            if e.state != DownloadState::Queued {
                continue;
            }
            if e.job.is_some() {
                // Being restored: started once the restore returns.
                active += 1;
            } else if let Some(job) = Self::start_job(*id, e) {
                jobs.push(job);
                active += 1;
            }
        }
        jobs
    }

    /// Starts `id` now if a slot is free (used by `resume` of an engine-held download).
    /// Returns the job, already marked in the state, or `None` if no slot is free or a
    /// job is in flight.
    pub(super) fn start_now(st: &mut State, id: DownloadId) -> Option<Job> {
        if st.active_count() >= usize::from(st.settings.max_active) {
            return None;
        }
        let e = st.entries.get_mut(&id)?;
        if e.job.is_some() {
            return None;
        }
        Self::start_job(id, e)
    }

    pub(super) fn spawn(self: &Arc<Self>, job: Job) {
        let inner = self.clone();
        self.spawn_job(async move { inner.run_job(job).await });
    }

    async fn run_job(self: Arc<Self>, job: Job) {
        let Job { id, engine, kind } = job;
        let eng = self.engines[engine].clone();
        let outcome = match kind {
            JobKind::Add { req, mode, opts } => {
                let seed = req.seed.clone();
                match eng.add(id, *req, mode, opts).await {
                    Ok(added) => {
                        self.record_added(id, &added);
                        self.catch_up_seed_policy(id, &eng, seed).await;
                        Outcome::Added
                    }
                    Err(e) => Outcome::AddFailed(e),
                }
            }
            JobKind::Restore { req, mode, opts } => {
                let seed = req.seed.clone();
                match eng.add(id, *req, mode, opts).await {
                    Ok(added) => {
                        self.record_added(id, &added);
                        self.catch_up_seed_policy(id, &eng, seed).await;
                        Outcome::Restored
                    }
                    Err(e) => Outcome::RestoreFailed(e),
                }
            }
            JobKind::Resume => Outcome::Resumed(eng.resume(id).await),
            JobKind::Reset => {
                if let Err(e) = eng.remove(id, true).await
                    && !matches!(e, EngineError::NotFound(_))
                {
                    tracing::warn!(id, "removing for restart: {e}");
                }
                Outcome::Reset
            }
        };
        let held = matches!(
            outcome,
            Outcome::Added | Outcome::Resumed(_) | Outcome::Restored
        );
        let mut outcome = Some(outcome);
        let mut paused = false;
        loop {
            enum Next {
                Orphan(bool),
                Pause,
                Done(Effects),
            }
            let next = {
                let mut st = self.lock();
                match st.entries.get(&id) {
                    None => Next::Orphan(st.orphans.remove(&id).is_some_and(|(_, d)| d)),
                    Some(e) if held && !paused && e.job.as_ref().is_some_and(|j| j.pause) => {
                        Next::Pause
                    }
                    Some(_) => {
                        let outcome = outcome.take().expect("settled once");
                        let fx = self.finish_job(&mut st, id, paused, outcome);
                        self.persist(&st);
                        Next::Done(fx)
                    }
                }
            };
            match next {
                Next::Orphan(delete_files) => {
                    // Removed while the job ran (the remove command may have removed it
                    // already, hence NotFound is fine).
                    if held
                        && let Err(e) = eng.remove(id, delete_files).await
                        && !matches!(e, EngineError::NotFound(_))
                    {
                        tracing::warn!(id, "removing a download removed while starting: {e}");
                    }
                    self.schedule();
                    return;
                }
                Next::Pause => {
                    if let Err(e) = eng.pause(id).await {
                        tracing::warn!(id, "pausing a download paused while starting: {e}");
                    }
                    paused = true;
                }
                Next::Done(fx) => {
                    self.apply(fx);
                    self.schedule();
                    return;
                }
            }
        }
    }

    /// Records a successful `add` right away, so the download is known to be
    /// engine-held (and its path known) even if the job is aborted later.
    fn record_added(&self, id: DownloadId, added: &Added) {
        let mut st = self.lock();
        let Some(e) = st.entries.get_mut(&id) else {
            return;
        };
        e.held = true;
        e.record_name(&added.name, added.total_bytes);
        e.rec.path = Some(added.path.clone());
        e.rec.total_bytes = added.total_bytes;
        if let Some(engine) = e.engine
            && let Err(err) = self.engines[engine].set_download_limit(id, e.rec.limit)
        {
            tracing::warn!(id, "applying the download limit: {err}");
        }
    }

    /// A seeding policy set while `add` ran (with `sent`) is handed to the engine now.
    async fn catch_up_seed_policy(
        &self,
        id: DownloadId,
        eng: &Arc<dyn puppy_core::engine::Engine>,
        sent: Option<puppy_core::types::SeedPolicy>,
    ) {
        let now = self
            .lock()
            .entries
            .get(&id)
            .and_then(|e| e.rec.request.seed.clone());
        if let Some(policy) = now.filter(|p| Some(p) != sent.as_ref())
            && let Err(e) = eng.set_seed(id, policy).await
            && !matches!(e, EngineError::NotFound(_))
        {
            tracing::warn!(id, "applying the seeding policy: {e}");
        }
    }

    /// Settles a finished job: the download leaves `Starting` for the state the engine
    /// (and any pause requested meanwhile) says.
    fn finish_job(
        &self,
        st: &mut State,
        id: DownloadId,
        paused: bool,
        outcome: Outcome,
    ) -> Effects {
        let mut fx = Effects::default();
        let Some(e) = st.entries.get_mut(&id) else {
            return fx;
        };
        let ctl = e.job.take().unwrap_or_default();
        let want_pause = ctl.pause;
        // Chosen before the restart: queued at the front (as right after the choice).
        let mut to_front = false;
        let next = match outcome {
            Outcome::Restored => {
                let engine = e.engine.expect("a job has an engine");
                match self.engines[engine].status(id) {
                    // Nothing seeds a seeding download the engine does not hold.
                    None if matches!(e.state, DownloadState::Seeding { .. }) => {
                        e.held = false;
                        DownloadState::Completed
                    }
                    None => {
                        e.held = false;
                        e.state.clone()
                    }
                    Some(status) => {
                        Self::record_status(e, &status);
                        let seeding = matches!(e.state, DownloadState::Seeding { .. });
                        match status.state {
                            s @ DownloadState::Completed => s,
                            // One already needing attention keeps its stored reason.
                            s @ DownloadState::NeedsAttention { .. }
                                if !matches!(e.state, DownloadState::NeedsAttention { .. }) =>
                            {
                                s
                            }
                            // Goes on seeding (its counters), or still choosing.
                            s @ DownloadState::Seeding { .. } if seeding => s,
                            // The engine no longer has it seeding (its counters were
                            // lost): resumed through the queue, which completes it.
                            DownloadState::Paused if seeding => DownloadState::Queued,
                            // Stored while choosing, but the engine has the choice already
                            // (made just before the daemon stopped): queued at the front,
                            // as right after the choice.
                            DownloadState::Paused if e.state == DownloadState::ChoosingFiles => {
                                to_front = true;
                                DownloadState::Queued
                            }
                            _ => e.state.clone(),
                        }
                    }
                }
            }
            Outcome::RestoreFailed(err) => {
                tracing::warn!(id, "restoring failed: {err}");
                e.held = false;
                let reason = match &err {
                    // Says it all (and the same as for a new add).
                    EngineError::Unavailable(m) => m.clone(),
                    _ => format!("restoring failed: {err}"),
                };
                DownloadState::Failed {
                    reason,
                    transient: err.is_transient(),
                }
            }
            Outcome::AddFailed(err) => {
                e.held = false;
                DownloadState::Failed {
                    reason: err.to_string(),
                    transient: err.is_transient(),
                }
            }
            Outcome::Reset => {
                e.held = false;
                e.rec.name = None;
                e.rec.path = None;
                e.rec.total_bytes = None;
                e.rec.finished_unix = None;
                if want_pause {
                    DownloadState::Paused
                } else {
                    DownloadState::Queued
                }
            }
            Outcome::Added | Outcome::Resumed(_) => {
                let engine = e.engine.expect("a job has an engine");
                match self.engines[engine].status(id) {
                    None => {
                        e.held = false;
                        DownloadState::Failed {
                            reason: "the engine lost the download".into(),
                            transient: false,
                        }
                    }
                    Some(status) => {
                        Self::record_status(e, &status);
                        match status.state {
                            s @ (DownloadState::Completed
                            | DownloadState::Seeding { .. }
                            | DownloadState::ChoosingFiles
                            | DownloadState::Failed { .. }
                            | DownloadState::NeedsAttention { .. }) => s,
                            DownloadState::Paused if want_pause => DownloadState::Paused,
                            // Paused by this job or interrupted by a pause, then resumed
                            // again: the scheduler resumes it.
                            DownloadState::Paused if paused || ctl.interrupted => {
                                DownloadState::Queued
                            }
                            DownloadState::Paused => match outcome {
                                Outcome::Resumed(Err(err)) => DownloadState::Failed {
                                    reason: err.to_string(),
                                    transient: err.is_transient(),
                                },
                                // Resumed; the transfer has not marked itself active yet.
                                _ => DownloadState::Active,
                            },
                            _ => DownloadState::Active,
                        }
                    }
                }
            }
        };
        self.transition(id, e, next, &mut fx);
        if to_front {
            st.order.retain(|x| *x != id);
            st.order.insert(0, id);
        }
        fx
    }
}
