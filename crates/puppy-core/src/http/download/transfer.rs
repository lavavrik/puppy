//! The transfer loop: runs segment workers (or one stream), steals work, keeps the
//! sidecar flushed, and turns worker exits into an outcome.

use super::files::{blocking, finalize, open_part};
use super::mirrors::{self, DropKind, Health};
use super::verify::complete;
use super::{Inner, Plan, RunEnd, RunHandle, SPEED_SAMPLE, SegEntry};
use crate::engine::EngineError;
use crate::http::segments::Segment;
use crate::http::single::run_single;
use crate::http::worker::{SegmentShared, WorkerCtx, WorkerExit, run_segment};
use crate::meter::SpeedMeter;
use crate::types::{AttentionReason, DownloadState};
use std::fs::File;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};
use tokio::task::JoinSet;
use tokio::time::MissedTickBehavior;
use tokio_util::sync::CancellationToken;

enum Outcome {
    Complete,
    Stopped,
    RemoteChanged,
    RangeUnsupported,
    /// Location `mirror`'s final URL refused a request with `status`: re-resolve it.
    UrlRejected {
        status: u16,
        mirror: usize,
    },
    Fatal(String),
    /// Failed for a reason that may go away by itself (retries exhausted on network
    /// errors or 5xx/429/408).
    Transient(String),
}

pub(super) fn spawn_run(inner: &Arc<Inner>) -> RunHandle {
    let cancel = CancellationToken::new();
    let join = tokio::spawn(run(inner.clone(), cancel.clone()));
    RunHandle { cancel, join }
}

/// One run of the download: transfers until complete, stopped or failed. Shared by fresh
/// starts and resumes.
async fn run(inner: Arc<Inner>, cancel: CancellationToken) -> RunEnd {
    if inner.lock().finalized {
        return complete(&inner, &cancel).await;
    }
    loop {
        let next = match transfer(&inner, &cancel).await {
            Outcome::Stopped => return RunEnd::Stopped,
            Outcome::Complete => match finalize(&inner).await {
                Ok(()) => return complete(&inner, &cancel).await,
                Err(e) => DownloadState::Failed {
                    reason: format!("finalizing failed: {e}"),
                    transient: false,
                },
            },
            Outcome::RemoteChanged => DownloadState::NeedsAttention {
                reason: AttentionReason::RemoteChanged,
            },
            Outcome::Fatal(reason) => DownloadState::Failed {
                reason,
                transient: false,
            },
            Outcome::Transient(reason) => DownloadState::Failed {
                reason,
                transient: true,
            },
            Outcome::UrlRejected { status, mirror } => {
                let done = inner.lock().plan.done_bytes();
                let failover = inner.failover();
                let again = inner.lock().mirrors[mirror]
                    .reresolved_at
                    .is_some_and(|at| done <= at);
                let result = if again {
                    None
                } else {
                    tokio::select! {
                        _ = cancel.cancelled() => return RunEnd::Stopped,
                        r = inner.reresolve(mirror, true) => Some(r),
                    }
                };
                if failover {
                    // Another location may carry on: only this one is dropped.
                    let mut st = inner.lock();
                    let m = &mut st.mirrors[mirror];
                    match result {
                        Some(Ok(true)) => {
                            m.reresolved_at = Some(done);
                            m.health = Health::Usable;
                        }
                        None => m.drop_with(DropKind::Rejected(status), format!("HTTP {status}")),
                        Some(Ok(false)) => m.drop_with(
                            DropKind::Changed,
                            format!("HTTP {status}; its URL now serves a different file"),
                        ),
                        Some(Err(e)) => m.drop_with(
                            mirrors::kind_of(&e),
                            format!("HTTP {status}; re-resolving the URL failed: {e}"),
                        ),
                    }
                    continue;
                }
                match result {
                    None => DownloadState::Failed {
                        reason: format!("HTTP {status}"),
                        transient: false,
                    },
                    Some(Ok(true)) => {
                        inner.lock().mirrors[mirror].reresolved_at = Some(done);
                        continue;
                    }
                    Some(Ok(false)) => DownloadState::NeedsAttention {
                        reason: AttentionReason::RemoteChanged,
                    },
                    Some(Err(e)) => DownloadState::Failed {
                        reason: format!("HTTP {status}; re-resolving the URL failed: {e}"),
                        transient: e.is_transient(),
                    },
                }
            }
            Outcome::RangeUnsupported if inner.lock().plan.fell_back => DownloadState::Failed {
                reason: "server does not honour range requests".into(),
                transient: false,
            },
            Outcome::RangeUnsupported => match fall_back_to_single(&inner).await {
                Ok(()) => continue,
                Err(e) => DownloadState::Failed {
                    reason: format!("restarting as a single stream failed: {e}"),
                    transient: false,
                },
            },
        };
        inner.set_state(next);
        return RunEnd::Finished;
    }
}

/// Switches to one stream from offset 0 and discards what the segments wrote.
async fn fall_back_to_single(inner: &Arc<Inner>) -> Result<(), EngineError> {
    {
        let mut st = inner.lock();
        st.plan = Plan::single(inner.remote.total, true);
        // The stream goes to a location that still answers: one that only failed on
        // ranges (the primary first) when none is usable any more.
        let now = Instant::now();
        if !st.mirrors.iter_mut().any(|m| m.available(now))
            && let Some(m) = st.mirrors.iter_mut().find(|m| {
                matches!(
                    m.health,
                    Health::Dropped {
                        kind: DropKind::RangeUnsupported,
                        ..
                    }
                )
            })
        {
            m.health = Health::Usable;
        }
    }
    let final_path = inner.final_path.clone();
    let file = blocking(move || open_part(&final_path)).await?;
    // Record "nothing done" before truncating so the sidecar never claims discarded bytes.
    inner.resume_state().flush(&file).await?;
    let total = inner.remote.total;
    blocking(move || {
        file.set_len(0)?;
        if let Some(t) = total {
            file.set_len(t)?;
        }
        Ok(())
    })
    .await?;
    Ok(())
}

/// Running worker tasks of one transfer.
struct Pool {
    set: JoinSet<(Arc<SegmentShared>, usize, WorkerExit)>,
    /// Each running segment with the location its worker downloads from.
    running: Vec<(Arc<SegmentShared>, usize)>,
    single_started: bool,
}

impl Pool {
    fn is_running(&self, seg: &Arc<SegmentShared>) -> bool {
        self.running.iter().any(|(r, _)| Arc::ptr_eq(r, seg))
    }

    fn spawn(&mut self, ctx: &Arc<WorkerCtx>, mirror: usize, e: SegEntry, single: bool) {
        self.running.push((e.shared.clone(), mirror));
        let ctx = ctx.clone();
        self.set.spawn(async move {
            let exit = if single {
                run_single(&ctx, e.shared.clone()).await
            } else {
                run_segment(&ctx, e.start, e.shared.clone()).await
            };
            (e.shared, mirror, exit)
        });
    }
}

/// Opens `.part`, runs workers until an outcome, then reaps every worker and (unless
/// complete or about to be discarded) flushes the sidecar. All `.part` handles are
/// closed when this returns.
async fn transfer(inner: &Arc<Inner>, cancel: &CancellationToken) -> Outcome {
    if cancel.is_cancelled() {
        return Outcome::Stopped;
    }
    let final_path = inner.final_path.clone();
    let file = match blocking(move || open_part(&final_path)).await {
        Ok(f) => Arc::new(f),
        Err(e) => return Outcome::Fatal(format!("cannot open partial file: {e}")),
    };
    let workers = cancel.child_token();
    let failover = inner.failover();
    let locations = inner.lock().mirrors.len();
    let ctxs: Vec<Arc<WorkerCtx>> = (0..locations)
        .map(|i| Arc::new(inner.worker_ctx(i, file.clone(), workers.clone(), failover)))
        .collect();
    inner.set_state(DownloadState::Active);

    let mut pool = Pool {
        set: JoinSet::new(),
        running: Vec::new(),
        single_started: false,
    };
    let outcome = drive(inner, cancel, &ctxs, &file, &mut pool).await;

    workers.cancel();
    while pool.set.join_next().await.is_some() {}
    inner.bps.store(0, Ordering::Relaxed);
    for m in &mut inner.lock().mirrors {
        m.active = 0;
    }
    if !matches!(outcome, Outcome::Complete | Outcome::RangeUnsupported) {
        inner.flush(&file).await;
    }
    // The workers held the only other clones; both are gone now.
    drop(ctxs);
    drop(file);
    outcome
}

async fn drive(
    inner: &Arc<Inner>,
    cancel: &CancellationToken,
    ctxs: &[Arc<WorkerCtx>],
    file: &File,
    pool: &mut Pool,
) -> Outcome {
    let mut flush_iv =
        tokio::time::interval(Duration::from_millis(inner.cfg.flush_interval_ms.max(1)));
    flush_iv.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut speed_iv = tokio::time::interval(SPEED_SAMPLE);
    let mut meter = SpeedMeter::new();
    let mut last_done = inner.lock().plan.done_bytes();
    // Segments as last flushed: an unchanged layout is not flushed again (an fsync is
    // expensive, e.g. F_FULLFSYNC on macOS).
    let mut flushed: Option<Vec<Segment>> = None;
    let failover = inner.failover();

    inner.schedule(ctxs, pool);
    loop {
        // With mirrors: when the next benched one may be used again.
        let wake = if failover {
            mirrors::next_unbench(&inner.lock().mirrors)
        } else {
            None
        };
        if pool.running.is_empty() {
            if inner.all_complete(pool) {
                return Outcome::Complete;
            }
            if wake.is_none() {
                return match failover {
                    true => inner.no_mirror_left(),
                    false => Outcome::Fatal("internal error: no segment could be scheduled".into()),
                };
            }
        }
        tokio::select! {
            biased;
            _ = cancel.cancelled() => return Outcome::Stopped,
            Some(joined) = pool.set.join_next() => {
                let (seg, mirror, exit) = match joined {
                    Ok(v) => v,
                    Err(e) => return Outcome::Fatal(format!("segment worker failed: {e}")),
                };
                pool.running.retain(|(r, _)| !Arc::ptr_eq(r, &seg));
                {
                    let mut st = inner.lock();
                    let m = &mut st.mirrors[mirror];
                    m.active = m.active.saturating_sub(1);
                }
                let outcome = match failover {
                    true => inner.worker_failed_over(mirror, exit),
                    false => single_location(exit, mirror),
                };
                if let Some(outcome) = outcome {
                    return outcome;
                }
                inner.schedule(ctxs, pool);
            }
            _ = tokio::time::sleep_until(wake.unwrap_or_else(Instant::now).into()), if wake.is_some() => {
                inner.schedule(ctxs, pool);
            }
            _ = flush_iv.tick() => inner.flush_if_changed(file, &mut flushed).await,
            _ = speed_iv.tick() => {
                let now = Instant::now();
                let mut st = inner.lock();
                let done = st.plan.done_bytes();
                meter.record(done.saturating_sub(last_done), now);
                last_done = done;
                inner.bps.store(meter.bps(now), Ordering::Relaxed);
                for m in &mut st.mirrors {
                    m.sample(now);
                }
            }
        }
    }
}

/// A worker's exit without mirrors: anything but finishing ends the transfer (the worker
/// has already retried what can be retried).
fn single_location(exit: WorkerExit, mirror: usize) -> Option<Outcome> {
    Some(match exit {
        WorkerExit::Done => return None,
        // Workers are only cancelled through our own (parent) token.
        WorkerExit::Cancelled => Outcome::Stopped,
        WorkerExit::RemoteChanged => Outcome::RemoteChanged,
        WorkerExit::RangeUnsupported => Outcome::RangeUnsupported,
        WorkerExit::UrlRejected(status) => Outcome::UrlRejected { status, mirror },
        WorkerExit::Fatal(reason) | WorkerExit::Refused(reason) => Outcome::Fatal(reason),
        WorkerExit::Exhausted(reason) | WorkerExit::Retryable { why: reason, .. } => {
            Outcome::Transient(reason)
        }
    })
}

impl Inner {
    /// Whether segments fail over between locations: the download has mirrors and is
    /// segmented (a single stream uses one location).
    pub(super) fn failover(&self) -> bool {
        let st = self.lock();
        st.mirrors.len() > 1 && !st.plan.single
    }

    fn worker_ctx(
        &self,
        mirror: usize,
        file: Arc<File>,
        cancel: CancellationToken,
        failover: bool,
    ) -> WorkerCtx {
        let st = self.lock();
        let m = &st.mirrors[mirror];
        let url = m.v.final_url.clone();
        WorkerCtx {
            client: self.client.clone(),
            context: m.context_for(&url),
            url,
            if_range: m.v.if_range(),
            expected_total: self.remote.total,
            file,
            limiters: [self.limit.clone(), self.global.clone()],
            cfg: self.cfg.clone(),
            cancel,
            failover,
            got: m.got.clone(),
        }
    }

    /// With mirrors: what a worker's exit means for its location, and `Some` if it ends
    /// the transfer. A failure benches or drops only that location; its range is picked
    /// up again by the next `schedule`, on another location if one is usable.
    fn worker_failed_over(&self, mirror: usize, exit: WorkerExit) -> Option<Outcome> {
        let now = Instant::now();
        let mut st = self.lock();
        let done = st.plan.done_bytes();
        let m = &mut st.mirrors[mirror];
        match exit {
            WorkerExit::Done => m.succeeded(),
            WorkerExit::Cancelled => return Some(Outcome::Stopped),
            // Writing the file failed: no location can help.
            WorkerExit::Fatal(reason) => return Some(Outcome::Fatal(reason)),
            WorkerExit::RemoteChanged => {
                m.drop_with(DropKind::Changed, "the file changed on it");
            }
            WorkerExit::RangeUnsupported => {
                m.drop_with(
                    DropKind::RangeUnsupported,
                    "it stopped honouring range requests",
                );
            }
            WorkerExit::Refused(reason) => m.drop_with(DropKind::Fatal, reason),
            WorkerExit::UrlRejected(status) => {
                // An expired redirect target is re-resolved (once per progress), which
                // restarts the transfer; a URL refused as given is dropped.
                let redirected = m.v.final_url != m.url;
                if redirected && !m.is_dropped() && !m.reresolved_at.is_some_and(|at| done <= at) {
                    return Some(Outcome::UrlRejected { status, mirror });
                }
                m.drop_with(DropKind::Rejected(status), format!("HTTP {status}"));
            }
            WorkerExit::Retryable { why, progress } => m.failed(why, progress, &self.cfg, now),
            WorkerExit::Exhausted(why) => m.failed(why, false, &self.cfg, now),
        }
        None
    }

    /// With mirrors, once none is usable or benched: the outcome their reasons add up
    /// to. A location that may come back (transient errors) makes it a transient failure
    /// (retried later); otherwise a changed file needs attention, and lost range support
    /// falls back to one stream.
    fn no_mirror_left(&self) -> Outcome {
        let st = self.lock();
        let kinds: Vec<DropKind> = st
            .mirrors
            .iter()
            .filter_map(|m| match &m.health {
                Health::Dropped { kind, .. } => Some(*kind),
                _ => None,
            })
            .collect();
        let summary = mirrors::summary(&st.mirrors);
        if kinds.contains(&DropKind::Transient) {
            Outcome::Transient(summary)
        } else if kinds.contains(&DropKind::Changed) {
            Outcome::RemoteChanged
        } else if kinds.contains(&DropKind::RangeUnsupported) {
            Outcome::RangeUnsupported
        } else {
            Outcome::Fatal(summary)
        }
    }

    /// Starts workers up to `connections`: first on idle unfinished segments, then by
    /// stealing the back half of the running segment with the most unclaimed bytes. Each
    /// goes to the location [`mirrors::pick`] chooses; none starts while every location
    /// is benched or dropped.
    fn schedule(&self, ctxs: &[Arc<WorkerCtx>], pool: &mut Pool) {
        let now = Instant::now();
        let mut st = self.lock();
        let st = &mut *st;
        // Lift benches that ran out even when no connection is free: the drive loop
        // only waits for benches still running.
        for m in st.mirrors.iter_mut() {
            m.available(now);
        }
        let plan = &mut st.plan;
        if plan.single {
            if !pool.single_started
                && let Some(m) = mirrors::pick(&mut st.mirrors, now)
            {
                pool.single_started = true;
                st.mirrors[m].active += 1;
                pool.spawn(&ctxs[m], m, plan.segments[0].clone(), true);
            }
            return;
        }
        while pool.running.len() < usize::from(self.cfg.connections) {
            let Some(m) = mirrors::pick(&mut st.mirrors, now) else {
                break;
            };
            if let Some(e) = plan
                .segments
                .iter()
                .find(|e| !e.is_complete() && !pool.is_running(&e.shared))
            {
                st.mirrors[m].active += 1;
                pool.spawn(&ctxs[m], m, e.clone(), false);
                continue;
            }
            let victim = plan
                .segments
                .iter()
                .enumerate()
                .filter(|(_, e)| pool.is_running(&e.shared))
                .max_by_key(|(_, e)| e.shared.remaining())
                .map(|(i, _)| i);
            let Some(i) = victim else { break };
            let v = &plan.segments[i];
            let Some((mid, end)) = v.shared.try_steal(v.start, self.cfg.min_segment_bytes) else {
                break;
            };
            let thief = SegEntry::new(mid, end, 0);
            plan.segments.insert(i + 1, thief.clone());
            st.mirrors[m].active += 1;
            pool.spawn(&ctxs[m], m, thief, false);
        }
    }

    fn all_complete(&self, pool: &Pool) -> bool {
        let st = self.lock();
        if st.plan.single {
            // A single stream has no known end; its worker returning `Done` is completion.
            pool.single_started
        } else {
            st.plan.segments.iter().all(SegEntry::is_complete)
        }
    }
}
