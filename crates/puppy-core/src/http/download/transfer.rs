//! The transfer loop: runs segment workers (or one stream), steals work, keeps the
//! sidecar flushed, and turns worker exits into an outcome.

use super::files::{blocking, finalize, open_part};
use super::mirrors::{self, DropKind, Health, MirrorRt};
use super::verify::complete;
use super::{Inner, Plan, RunEnd, RunHandle, SPEED_SAMPLE, SegEntry};
use crate::config::HttpConfig;
use crate::engine::EngineError;
use crate::http::resume::part_path;
use crate::http::segments::Segment;
use crate::http::single::run_single;
use crate::http::validate::{Validation, spot_check};
use crate::http::worker::{SegmentShared, WorkerCtx, WorkerExit, is_url_rejected, run_segment};
use crate::meter::SpeedMeter;
use crate::types::{AttentionReason, DownloadState};
use std::fs::File;
use std::path::PathBuf;
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
                // Without mirrors only (with mirrors a location is re-resolved while the
                // others go on).
                let done = inner.lock().plan.done_bytes();
                let again = inner.lock().mirrors[mirror]
                    .reresolved_at
                    .is_some_and(|at| done <= at);
                if again {
                    DownloadState::Failed {
                        reason: format!("HTTP {status}"),
                        transient: false,
                    }
                } else {
                    let result = tokio::select! {
                        _ = cancel.cancelled() => return RunEnd::Stopped,
                        r = inner.reresolve(mirror, true) => r,
                    };
                    match result {
                        Ok(true) => {
                            inner.lock().mirrors[mirror].reresolved_at = Some(done);
                            continue;
                        }
                        Ok(false) => DownloadState::NeedsAttention {
                            reason: AttentionReason::RemoteChanged,
                        },
                        Err(e) => DownloadState::Failed {
                            reason: format!("HTTP {status}; re-resolving the URL failed: {e}"),
                            transient: e.is_transient(),
                        },
                    }
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

/// A check of a validator-less location run before its worker downloads: the window of
/// `.part` (already downloaded bytes) to compare against it.
#[derive(Clone)]
struct PreCheck {
    window: (u64, u64),
    total: u64,
    part: PathBuf,
}

/// Running worker tasks of one transfer, and what they are built from.
struct Pool {
    set: JoinSet<(Arc<SegmentShared>, usize, WorkerExit)>,
    /// Each running segment with the location its worker downloads from and its own
    /// cancellation (a child of `workers`), to move a connection to another location.
    running: Vec<(Arc<SegmentShared>, usize, CancellationToken)>,
    single_started: bool,
    /// Re-resolving one location's expired redirect target, while the others go on.
    tasks: JoinSet<(usize, Result<bool, EngineError>)>,
    /// Workers cancelled to give their connection to another location; their
    /// `Cancelled` exit only frees the segment.
    preempted: Vec<Arc<SegmentShared>>,
    /// One worker context per location (rebuilt when its final URL changes).
    ctxs: Vec<Arc<WorkerCtx>>,
    file: Arc<File>,
    workers: CancellationToken,
    failover: bool,
}

impl Pool {
    fn is_running(&self, seg: &Arc<SegmentShared>) -> bool {
        self.running.iter().any(|(r, _, _)| Arc::ptr_eq(r, seg))
    }

    fn spawn(&mut self, mirror: usize, e: SegEntry, single: bool, check: Option<PreCheck>) {
        let token = self.workers.child_token();
        self.running.push((e.shared.clone(), mirror, token.clone()));
        let mut ctx = (*self.ctxs[mirror]).clone();
        ctx.cancel = token;
        self.set.spawn(async move {
            let exit = match check.filter(|_| !single) {
                Some(c) => match pre_check(&ctx, &c).await {
                    None => run_worker(&ctx, e.start, e.shared.clone(), false).await,
                    Some(exit) => exit,
                },
                None => run_worker(&ctx, e.start, e.shared.clone(), single).await,
            };
            (e.shared, mirror, exit)
        });
    }
}

/// One worker over the location's protocol.
async fn run_worker(
    ctx: &WorkerCtx,
    start: u64,
    seg: Arc<SegmentShared>,
    single: bool,
) -> WorkerExit {
    if let Some(sftp) = &ctx.sftp {
        return crate::sftp::worker::run(ctx, sftp, start, seg, single).await;
    }
    match &ctx.ftp {
        Some(ftp) => crate::ftp::worker::run(ctx, ftp, start, seg, single).await,
        None if single => run_single(ctx, seg).await,
        None => run_segment(ctx, start, seg).await,
    }
}

/// Compares the window of already downloaded bytes against the worker's location;
/// `None` if it matches (go on downloading), otherwise how the worker ends.
async fn pre_check(ctx: &WorkerCtx, c: &PreCheck) -> Option<WorkerExit> {
    let (start, end) = c.window;
    let check = async {
        if let Some(sftp) = &ctx.sftp {
            return crate::sftp::validate::spot_check(
                &sftp.site, c.total, start, end, &c.part, &ctx.cfg,
            )
            .await;
        }
        match &ctx.ftp {
            Some(ftp) => {
                crate::ftp::validate::spot_check(&ftp.site, c.total, start, end, &c.part, &ctx.cfg)
                    .await
            }
            None => {
                spot_check(
                    &ctx.client,
                    &ctx.context,
                    &ctx.url,
                    c.total,
                    start,
                    end,
                    &c.part,
                    &ctx.cfg,
                )
                .await
            }
        }
    };
    let result = tokio::select! {
        _ = ctx.cancel.cancelled() => return Some(WorkerExit::Cancelled),
        r = check => r,
    };
    match result {
        Ok(Validation::Ok) => None,
        Ok(Validation::Changed) => Some(WorkerExit::RemoteChanged),
        Ok(Validation::NotResumable) => Some(WorkerExit::RangeUnsupported),
        Err(EngineError::Http { status }) if is_url_rejected(status) => {
            Some(WorkerExit::UrlRejected(status))
        }
        Err(e) if e.is_transient() => Some(WorkerExit::Retryable {
            why: e.to_string(),
            progress: false,
        }),
        Err(EngineError::Io(e)) => {
            Some(WorkerExit::Fatal(format!("reading the partial file: {e}")))
        }
        Err(e) => Some(WorkerExit::Refused(e.to_string())),
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
        tasks: JoinSet::new(),
        preempted: Vec::new(),
        ctxs,
        file: file.clone(),
        workers: workers.clone(),
        failover,
    };
    let outcome = drive(inner, cancel, &file, &mut pool).await;

    workers.cancel();
    while pool.set.join_next().await.is_some() {}
    // A re-resolution may be reading `.part`: it must be gone too.
    pool.tasks.abort_all();
    while pool.tasks.join_next().await.is_some() {}
    inner.bps.store(0, Ordering::Relaxed);
    for m in &mut inner.lock().mirrors {
        m.active = 0;
        m.busy = false;
        if let Some(site) = &m.ftp {
            site.reset();
        }
        if let Some(site) = &m.sftp {
            site.reset();
        }
    }
    if !matches!(outcome, Outcome::Complete | Outcome::RangeUnsupported) {
        inner.flush(&file).await;
    }
    // The workers held the only other clones; all are gone now.
    drop(pool);
    drop(file);
    outcome
}

async fn drive(
    inner: &Arc<Inner>,
    cancel: &CancellationToken,
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
    let failover = pool.failover;

    inner.schedule(pool);
    loop {
        // With mirrors: when the next benched one may be used again.
        let wake = if failover {
            mirrors::next_unbench(&inner.lock().mirrors)
        } else {
            None
        };
        if pool.running.is_empty() && pool.tasks.is_empty() {
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
                pool.running.retain(|(r, _, _)| !Arc::ptr_eq(r, &seg));
                {
                    let mut st = inner.lock();
                    let m = &mut st.mirrors[mirror];
                    m.active = m.active.saturating_sub(1);
                }
                let preempted = pool.preempted.iter().position(|p| Arc::ptr_eq(p, &seg));
                if let Some(i) = preempted {
                    pool.preempted.swap_remove(i);
                }
                let next = match (failover, exit) {
                    // Moved to another location on purpose: the segment is free again.
                    (true, WorkerExit::Cancelled) if preempted.is_some() => Next::Go,
                    (true, exit) => inner.worker_failed_over(mirror, exit),
                    (false, exit) => match single_location(exit, mirror) {
                        Some(o) => Next::End(o),
                        None => Next::Go,
                    },
                };
                match next {
                    Next::Go => {}
                    Next::End(outcome) => return outcome,
                    Next::Reresolve(i) => {
                        let inner = inner.clone();
                        pool.tasks.spawn(async move { (i, inner.reresolve(i, false).await) });
                    }
                }
                inner.schedule(pool);
            }
            Some(joined) = pool.tasks.join_next() => {
                let (i, result) = match joined {
                    Ok(v) => v,
                    Err(e) => return Outcome::Fatal(format!("re-resolving failed: {e}")),
                };
                if let Some(outcome) = inner.reresolved(i, result) {
                    return outcome;
                }
                pool.ctxs[i] = Arc::new(inner.worker_ctx(
                    i,
                    pool.file.clone(),
                    pool.workers.clone(),
                    failover,
                ));
                inner.schedule(pool);
            }
            _ = tokio::time::sleep_until(wake.unwrap_or_else(Instant::now).into()), if wake.is_some() => {
                inner.schedule(pool);
            }
            _ = flush_iv.tick() => inner.flush_if_changed(file, &mut flushed).await,
            _ = speed_iv.tick() => {
                let now = Instant::now();
                {
                    let mut st = inner.lock();
                    let done = st.plan.done_bytes();
                    meter.record(done.saturating_sub(last_done), now);
                    last_done = done;
                    inner.bps.store(meter.bps(now), Ordering::Relaxed);
                    for m in &mut st.mirrors {
                        m.sample(now);
                    }
                }
                // With mirrors, also between worker exits: a location that became
                // usable (or checkable) gets a connection moved to it.
                if failover {
                    inner.schedule(pool);
                }
            }
        }
    }
}

/// What a worker's exit means for the transfer (with mirrors).
enum Next {
    Go,
    End(Outcome),
    /// Re-resolve this location's expired redirect target in the background.
    Reresolve(usize),
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
            ftp: m.ftp.clone().map(|site| crate::ftp::FtpTarget {
                site,
                mdtm: m.v.last_modified.clone(),
            }),
            sftp: m.sftp.clone().map(|site| crate::sftp::SftpTarget {
                site,
                mtime: m.v.last_modified.clone(),
            }),
        }
    }

    /// With mirrors: what a worker's exit means for its location. A failure benches or
    /// drops only that location; its range is picked up again by the next `schedule`,
    /// on another location if one is usable. The primary's file changing (it has
    /// validators, so this is certain) ends the download as without mirrors.
    fn worker_failed_over(&self, mirror: usize, exit: WorkerExit) -> Next {
        let now = Instant::now();
        let mut st = self.lock();
        let m = &mut st.mirrors[mirror];
        match exit {
            WorkerExit::Done => m.succeeded(),
            WorkerExit::Cancelled => return Next::End(Outcome::Stopped),
            // Writing the file failed: no location can help.
            WorkerExit::Fatal(reason) => return Next::End(Outcome::Fatal(reason)),
            WorkerExit::RemoteChanged if m.primary && !m.v.is_empty() => {
                m.drop_with(DropKind::Changed, "the file changed on it");
                return Next::End(Outcome::RemoteChanged);
            }
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
                // An expired redirect target is re-resolved (once per progress of this
                // location) while the others go on; a URL refused as given is dropped.
                let got = m.got.load(Ordering::Relaxed);
                let redirected = m.v.final_url != m.url;
                if redirected
                    && !m.is_dropped()
                    && !m.busy
                    && !m.reresolved_at.is_some_and(|at| got <= at)
                {
                    m.busy = true;
                    m.reresolved_at = Some(got);
                    return Next::Reresolve(mirror);
                }
                if !m.busy {
                    m.drop_with(DropKind::Rejected(status), format!("HTTP {status}"));
                }
            }
            WorkerExit::Retryable { why, progress } => m.failed(why, progress, &self.cfg, now),
            WorkerExit::Exhausted(why) => m.failed(why, false, &self.cfg, now),
        }
        Next::Go
    }

    /// A location's re-resolution ended: usable again with its new final URL, or dropped.
    /// `Some` if that ends the download (the primary now serves another file).
    fn reresolved(&self, i: usize, result: Result<bool, EngineError>) -> Option<Outcome> {
        let mut st = self.lock();
        let m = &mut st.mirrors[i];
        m.busy = false;
        match result {
            Ok(true) => m.health = Health::Usable,
            Ok(false) => {
                m.drop_with(DropKind::Changed, "its URL now serves a different file");
                if m.primary && !m.v.is_empty() {
                    return Some(Outcome::RemoteChanged);
                }
            }
            // It may answer later: benched (and dropped after `max_retries`), and
            // re-resolved again when refused again.
            Err(e) if e.is_transient() => {
                m.reresolved_at = None;
                m.failed(
                    format!("re-resolving its URL failed: {e}"),
                    false,
                    &self.cfg,
                    Instant::now(),
                );
            }
            Err(e) => m.drop_with(
                mirrors::kind_of(&e),
                format!("re-resolving its URL failed: {e}"),
            ),
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
    ///
    /// With mirrors, a location without validators (nothing for `If-Range`) first has a
    /// window of already downloaded bytes compared against it, before every range it
    /// gets. Until there are such bytes it waits, unless no location has validators.
    /// A location that is usable again but has no connection while all are taken gets
    /// one from the busiest location.
    fn schedule(&self, pool: &mut Pool) {
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
                && let Some(m) = mirrors::pick(&mut st.mirrors, now, |_| true)
            {
                pool.single_started = true;
                st.mirrors[m].active += 1;
                pool.spawn(m, plan.segments[0].clone(), true, None);
            }
            return;
        }
        let window = pool
            .failover
            .then(|| check_window(plan, &self.cfg))
            .flatten();
        let validated = st
            .mirrors
            .iter()
            .any(|m| !m.is_dropped() && m.v.if_range().is_some());
        let allowed = |m: &MirrorRt| m.v.if_range().is_some() || window.is_some() || !validated;
        let failover = pool.failover;
        let check = |m: &MirrorRt| -> Option<PreCheck> {
            let window = window.filter(|_| failover && m.v.if_range().is_none())?;
            Some(PreCheck {
                window,
                total: self.remote.total.unwrap_or(0),
                part: part_path(&self.final_path),
            })
        };
        while pool.running.len() < usize::from(self.cfg.connections) {
            let Some(m) = mirrors::pick(&mut st.mirrors, now, allowed) else {
                break;
            };
            let pre = check(&st.mirrors[m]);
            if let Some(e) = plan
                .segments
                .iter()
                .find(|e| !e.is_complete() && !pool.is_running(&e.shared))
            {
                st.mirrors[m].active += 1;
                pool.spawn(m, e.clone(), false, pre);
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
            pool.spawn(m, thief, false, pre);
        }
        // All connections taken while a usable location has none: move one over.
        if pool.failover
            && pool.preempted.is_empty()
            && pool.running.len() >= usize::from(self.cfg.connections)
        {
            let idle = st
                .mirrors
                .iter()
                .any(|m| m.active == 0 && !m.busy && m.available_now(now) && allowed(m));
            let busiest = (0..st.mirrors.len()).max_by_key(|&i| st.mirrors[i].active);
            if idle
                && let Some(b) = busiest.filter(|&b| st.mirrors[b].active >= 2)
                && let Some((seg, _, token)) = pool
                    .running
                    .iter()
                    .filter(|(_, m, _)| *m == b)
                    .max_by_key(|(s, _, _)| s.remaining())
            {
                pool.preempted.push(seg.clone());
                token.cancel();
            }
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

/// The bytes a validator-less location is compared on: the last (at most
/// `spot_check_bytes`) bytes of the largest downloaded run. `None` before any byte.
fn check_window(plan: &Plan, cfg: &HttpConfig) -> Option<(u64, u64)> {
    let best = plan
        .segments
        .iter()
        .map(|e| (e.start, e.done()))
        .filter(|&(_, done)| done > 0)
        .max_by_key(|&(_, done)| done)?;
    let (start, done) = best;
    let len = cfg.spot_check_bytes.clamp(1, done);
    Some((start + done - len, start + done))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::EngineEvent;
    use tokio::sync::broadcast;

    /// A primary plus a mirror whose final URL is a redirect target.
    fn two_locations() -> Arc<Inner> {
        let (tx, _) = broadcast::channel::<EngineEvent>(16);
        let inner = Inner::for_test(tx);
        {
            let mut st = inner.lock();
            let mut req = inner.request.clone();
            req.mirrors = vec![crate::types::Mirror::new(
                url::Url::parse("http://127.0.0.1:9/mirror").unwrap(),
            )];
            st.mirrors = mirrors::locations(&req);
            st.mirrors[1].v.final_url = url::Url::parse("http://127.0.0.1:9/signed/0").unwrap();
        }
        inner
    }

    /// A refused redirect target re-resolves only that location (the transfer goes
    /// on), once per progress of that location: the others' progress does not re-arm
    /// it.
    #[test]
    fn a_refused_mirror_is_re_resolved_alone() {
        let inner = two_locations();
        let rejected = || WorkerExit::UrlRejected(403);
        assert!(matches!(
            inner.worker_failed_over(1, rejected()),
            Next::Reresolve(1)
        ));
        assert!(inner.lock().mirrors[1].busy);
        // Its other connections refused meanwhile: nothing more (no second re-resolve).
        assert!(matches!(inner.worker_failed_over(1, rejected()), Next::Go));
        assert!(inner.reresolved(1, Ok(true)).is_none());
        assert!(!inner.lock().mirrors[1].busy);
        // Progress elsewhere does not count: refused again, it is dropped.
        inner.lock().mirrors[0]
            .got
            .fetch_add(1 << 20, Ordering::Relaxed);
        assert!(matches!(inner.worker_failed_over(1, rejected()), Next::Go));
        assert!(inner.lock().mirrors[1].is_dropped());
    }

    /// With progress of its own since, it may be re-resolved again.
    #[test]
    fn own_progress_re_arms_re_resolving() {
        let inner = two_locations();
        let rejected = || WorkerExit::UrlRejected(403);
        assert!(matches!(
            inner.worker_failed_over(1, rejected()),
            Next::Reresolve(1)
        ));
        assert!(inner.reresolved(1, Ok(true)).is_none());
        inner.lock().mirrors[1].got.fetch_add(1, Ordering::Relaxed);
        assert!(matches!(
            inner.worker_failed_over(1, rejected()),
            Next::Reresolve(1)
        ));
    }

    /// The primary serving another file after re-resolving ends the download.
    #[test]
    fn primary_re_resolved_to_another_file_needs_attention() {
        let inner = two_locations();
        inner.lock().mirrors[0].v.etag = Some("\"v0\"".into());
        inner.lock().mirrors[0].v.etag_strong = true;
        assert!(matches!(
            inner.reresolved(0, Ok(false)),
            Some(Outcome::RemoteChanged)
        ));
        assert!(inner.reresolved(1, Ok(false)).is_none());
    }

    /// A re-resolution that fails for now benches the location (to be re-resolved
    /// again) instead of dropping it.
    #[test]
    fn transient_re_resolve_failure_benches() {
        let inner = two_locations();
        assert!(matches!(
            inner.worker_failed_over(1, WorkerExit::UrlRejected(403)),
            Next::Reresolve(1)
        ));
        let e = EngineError::Http { status: 503 };
        assert!(inner.reresolved(1, Err(e)).is_none());
        let st = inner.lock();
        assert!(matches!(st.mirrors[1].health, Health::Benched { .. }));
        assert_eq!(st.mirrors[1].reresolved_at, None);
    }
}
