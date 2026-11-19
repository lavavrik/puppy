//! The transfer loop: runs segment workers (or one stream), steals work, keeps the
//! sidecar flushed, and turns worker exits into an outcome.

use super::files::{blocking, finalize, open_part};
use super::{Inner, Plan, RunEnd, RunHandle, SPEED_SAMPLE, SegEntry};
use crate::engine::{EngineError, EngineEvent};
use crate::http::single::run_single;
use crate::http::worker::{SegmentShared, WorkerCtx, WorkerExit, run_segment};
use crate::meter::SpeedMeter;
use crate::types::{AttentionReason, DownloadState};
use std::fs::File;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;
use tokio::task::JoinSet;
use tokio::time::MissedTickBehavior;
use tokio_util::sync::CancellationToken;

enum Outcome {
    Complete,
    Stopped,
    RemoteChanged,
    RangeUnsupported,
    Fatal(String),
}

pub(super) fn spawn_run(inner: &Arc<Inner>) -> RunHandle {
    let cancel = CancellationToken::new();
    let join = tokio::spawn(run(inner.clone(), cancel.clone()));
    RunHandle { cancel, join }
}

/// One run of the download: transfers until complete, stopped or failed. Shared by fresh
/// starts and resumes.
async fn run(inner: Arc<Inner>, cancel: CancellationToken) -> RunEnd {
    loop {
        let next = match transfer(&inner, &cancel).await {
            Outcome::Stopped => return RunEnd::Stopped,
            Outcome::Complete => match finalize(&inner).await {
                Ok(()) => {
                    inner.set_state(DownloadState::Completed);
                    let _ = inner.events.send(EngineEvent::Completed { id: inner.id });
                    return RunEnd::Finished;
                }
                Err(e) => DownloadState::Failed {
                    reason: format!("finalizing failed: {e}"),
                },
            },
            Outcome::RemoteChanged => DownloadState::NeedsAttention {
                reason: AttentionReason::RemoteChanged,
            },
            Outcome::Fatal(reason) => DownloadState::Failed { reason },
            Outcome::RangeUnsupported if inner.lock().plan.fell_back => DownloadState::Failed {
                reason: "server does not honour range requests".into(),
            },
            Outcome::RangeUnsupported => match fall_back_to_single(&inner).await {
                Ok(()) => continue,
                Err(e) => DownloadState::Failed {
                    reason: format!("restarting as a single stream failed: {e}"),
                },
            },
        };
        inner.set_state(next);
        return RunEnd::Finished;
    }
}

/// Switches to one stream from offset 0 and discards what the segments wrote.
async fn fall_back_to_single(inner: &Arc<Inner>) -> Result<(), EngineError> {
    inner.lock().plan = Plan::single(inner.remote.total, true);
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
    set: JoinSet<(Arc<SegmentShared>, WorkerExit)>,
    running: Vec<Arc<SegmentShared>>,
    single_started: bool,
}

impl Pool {
    fn is_running(&self, seg: &Arc<SegmentShared>) -> bool {
        self.running.iter().any(|r| Arc::ptr_eq(r, seg))
    }

    fn spawn(&mut self, ctx: &Arc<WorkerCtx>, e: SegEntry, single: bool) {
        self.running.push(e.shared.clone());
        let ctx = ctx.clone();
        self.set.spawn(async move {
            let exit = if single {
                run_single(&ctx, e.shared.clone()).await
            } else {
                run_segment(&ctx, e.start, e.shared.clone()).await
            };
            (e.shared, exit)
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
    let ctx = Arc::new(inner.worker_ctx(file.clone(), workers.clone()));
    inner.set_state(DownloadState::Active);

    let mut pool = Pool {
        set: JoinSet::new(),
        running: Vec::new(),
        single_started: false,
    };
    let outcome = drive(inner, cancel, &ctx, &file, &mut pool).await;

    workers.cancel();
    while pool.set.join_next().await.is_some() {}
    inner.bps.store(0, Ordering::Relaxed);
    if !matches!(outcome, Outcome::Complete | Outcome::RangeUnsupported) {
        inner.flush(&file).await;
    }
    // The workers held the only other clones; both are gone now.
    drop(ctx);
    drop(file);
    outcome
}

async fn drive(
    inner: &Arc<Inner>,
    cancel: &CancellationToken,
    ctx: &Arc<WorkerCtx>,
    file: &File,
    pool: &mut Pool,
) -> Outcome {
    let mut flush_iv =
        tokio::time::interval(Duration::from_millis(inner.cfg.flush_interval_ms.max(1)));
    flush_iv.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut speed_iv = tokio::time::interval(SPEED_SAMPLE);
    let mut meter = SpeedMeter::new();
    let mut last_done = inner.lock().plan.done_bytes();

    inner.schedule(ctx, pool);
    loop {
        if pool.running.is_empty() {
            return if inner.all_complete(pool) {
                Outcome::Complete
            } else {
                Outcome::Fatal("internal error: no segment could be scheduled".into())
            };
        }
        tokio::select! {
            biased;
            _ = cancel.cancelled() => return Outcome::Stopped,
            Some(joined) = pool.set.join_next() => {
                let (seg, exit) = match joined {
                    Ok(v) => v,
                    Err(e) => return Outcome::Fatal(format!("segment worker failed: {e}")),
                };
                pool.running.retain(|r| !Arc::ptr_eq(r, &seg));
                match exit {
                    WorkerExit::Done => inner.schedule(ctx, pool),
                    // Workers are only cancelled through our own (parent) token.
                    WorkerExit::Cancelled => return Outcome::Stopped,
                    WorkerExit::RemoteChanged => return Outcome::RemoteChanged,
                    WorkerExit::RangeUnsupported => return Outcome::RangeUnsupported,
                    WorkerExit::Fatal(reason) => return Outcome::Fatal(reason),
                }
            }
            _ = flush_iv.tick() => inner.flush(file).await,
            _ = speed_iv.tick() => {
                let done = inner.lock().plan.done_bytes();
                let now = std::time::Instant::now();
                meter.record(done.saturating_sub(last_done), now);
                last_done = done;
                inner.bps.store(meter.bps(now), Ordering::Relaxed);
            }
        }
    }
}

impl Inner {
    fn worker_ctx(&self, file: Arc<File>, cancel: CancellationToken) -> WorkerCtx {
        let r = &self.remote;
        // Weak ETags cannot validate ranges; Last-Modified only when there is no ETag.
        let if_range = match &r.etag {
            Some(e) if r.etag_strong => Some(e.clone()),
            Some(_) => None,
            None => r.last_modified.clone(),
        };
        WorkerCtx {
            client: self.client.clone(),
            url: r.final_url.clone(),
            context: self.request.context.clone(),
            if_range,
            expected_total: r.total,
            file,
            limiters: [self.limit.clone(), self.global.clone()],
            cfg: self.cfg.clone(),
            cancel,
        }
    }

    /// Starts workers up to `connections`: first on idle unfinished segments, then by
    /// stealing the back half of the running segment with the most unclaimed bytes.
    fn schedule(&self, ctx: &Arc<WorkerCtx>, pool: &mut Pool) {
        let mut st = self.lock();
        let plan = &mut st.plan;
        if plan.single {
            if !pool.single_started {
                pool.single_started = true;
                pool.spawn(ctx, plan.segments[0].clone(), true);
            }
            return;
        }
        while pool.running.len() < usize::from(self.cfg.connections) {
            if let Some(e) = plan
                .segments
                .iter()
                .find(|e| !e.is_complete() && !pool.is_running(&e.shared))
            {
                pool.spawn(ctx, e.clone(), false);
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
            pool.spawn(ctx, thief, false);
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
