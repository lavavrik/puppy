//! `HttpDownload`: drives one HTTP download end-to-end — probe, file placement,
//! segmented (or single-stream) transfer with work stealing, crash-safe sidecar,
//! pause/resume/cancel and finalization.
//!
//! Ownership rule (Windows cannot rename/delete a file with an open handle): every handle
//! to `.part` is created inside one `transfer::transfer` call, held only by that call and the
//! worker tasks it spawns, and all workers are reaped before it returns. Finalization and
//! deletion run strictly after `transfer` has returned, so no handle can be open then.

use crate::config::HttpConfig;
use crate::engine::{AddOptions, Added, EngineError, EngineEvent};
use crate::http::probe::RemoteInfo;
use crate::http::probe::probe;
use crate::http::resume::{
    HttpResumeState, VERSION, context_hash, part_path, sidecar_path, without_context,
};
use crate::http::segments::{Segment, plan_segments};
use crate::http::validate::{Validation, validate_remote};
use crate::http::worker::{SegmentShared, is_url_rejected};
use crate::limiter::TokenBucket;
use crate::types::{
    AddRequest, AttentionReason, DownloadId, DownloadState, DownloadStatus, EngineDetail,
    SegmentView, Source,
};
use files::{blocking, open_part, prepare};
use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;
use tokio::sync::{broadcast, watch};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use transfer::spawn_run;

mod files;
mod restore;
mod transfer;

const SPEED_SAMPLE: Duration = Duration::from_millis(250);

/// One HTTP download. Control operations (`pause`/`resume`/`cancel`) are serialized.
pub struct HttpDownload {
    inner: Arc<Inner>,
}

struct Inner {
    id: DownloadId,
    /// The caller's request, including its context (which is never persisted).
    request: AddRequest,
    client: reqwest::Client,
    cfg: HttpConfig,
    global: TokenBucket,
    limit: TokenBucket,
    events: broadcast::Sender<EngineEvent>,
    /// Facts from the probe; `final_url` is only the initial value of `Shared::final_url`.
    remote: RemoteInfo,
    final_path: PathBuf,
    name: String,
    st: Mutex<Shared>,
    /// Whole-download speed, sampled by the running transfer.
    bps: AtomicU64,
    control: tokio::sync::Mutex<Control>,
    /// Bumped by `pause`/`cancel` before they wait for the control lock, so they can
    /// interrupt a validation in progress.
    interrupt: watch::Sender<u64>,
}

struct Shared {
    state: DownloadState,
    plan: Plan,
    /// Where segment requests go; replaced when an expired redirect target is re-resolved.
    final_url: url::Url,
}

struct Plan {
    /// One plain GET from offset 0 (no ranges or unknown size); not resumable.
    single: bool,
    /// Already restarted as a single stream after a server ignored ranges.
    fell_back: bool,
    /// Sorted, contiguous; grows by work stealing.
    segments: Vec<SegEntry>,
}

#[derive(Clone)]
struct SegEntry {
    start: u64,
    shared: Arc<SegmentShared>,
}

#[derive(Default)]
struct Control {
    run: Option<RunHandle>,
    cancelled: bool,
}

struct RunHandle {
    cancel: CancellationToken,
    join: JoinHandle<RunEnd>,
}

/// How a run task ended: stopped on request (state left for the controller to set),
/// or finished with a terminal state already recorded.
enum RunEnd {
    Stopped,
    Finished,
}

impl SegEntry {
    fn new(start: u64, end: u64, done: u64) -> Self {
        SegEntry {
            start,
            shared: Arc::new(SegmentShared::new(start, end, done)),
        }
    }

    fn done(&self) -> u64 {
        self.shared.done.load(Ordering::Acquire)
    }

    fn is_complete(&self) -> bool {
        self.start + self.done() >= self.shared.end()
    }

    /// Only bytes already written (`done`), against the current (possibly shrunk) end.
    fn snapshot(&self) -> Segment {
        let end = self.shared.end();
        Segment {
            start: self.start,
            end,
            done: self.done().min(end - self.start),
        }
    }
}

impl Plan {
    fn new(total: Option<u64>, accepts_ranges: bool, cfg: &HttpConfig) -> Self {
        match total {
            Some(t) if accepts_ranges => Plan {
                single: false,
                fell_back: false,
                segments: plan_segments(t, cfg.connections, cfg.min_segment_bytes)
                    .into_iter()
                    .map(|s| SegEntry::new(s.start, s.end, s.done))
                    .collect(),
            },
            _ => Plan::single(total, false),
        }
    }

    fn single(total: Option<u64>, fell_back: bool) -> Self {
        Plan {
            single: true,
            fell_back,
            segments: vec![SegEntry::new(0, total.unwrap_or(u64::MAX), 0)],
        }
    }

    fn done_bytes(&self) -> u64 {
        self.segments.iter().map(SegEntry::done).sum()
    }
}

/// What every way of creating a download is given by its caller.
struct Env {
    id: DownloadId,
    client: reqwest::Client,
    cfg: HttpConfig,
    global: TokenBucket,
    events: broadcast::Sender<EngineEvent>,
    opts: AddOptions,
}

impl Env {
    fn new(
        id: DownloadId,
        opts: AddOptions,
        client: reqwest::Client,
        mut cfg: HttpConfig,
        global: TokenBucket,
        events: broadcast::Sender<EngineEvent>,
    ) -> Self {
        cfg.min_segment_bytes = cfg.min_segment_bytes.max(1);
        cfg.connections = cfg.connections.max(1);
        Env {
            id,
            client,
            cfg,
            global,
            events,
            opts,
        }
    }

    fn emit(&self, state: DownloadState) {
        let _ = self
            .events
            .send(EngineEvent::StateChanged { id: self.id, state });
    }

    /// The download in state `Starting`, not yet running.
    fn build(
        self,
        request: AddRequest,
        remote: RemoteInfo,
        final_path: PathBuf,
        plan: Plan,
    ) -> (Arc<Inner>, Added) {
        let name = file_name(&final_path);
        let added = Added {
            name: name.clone(),
            path: final_path.clone(),
            total_bytes: remote.total,
        };
        let final_url = remote.final_url.clone();
        let inner = Arc::new(Inner {
            id: self.id,
            request,
            client: self.client,
            cfg: self.cfg,
            global: self.global,
            // In force before anything is transferred.
            limit: TokenBucket::new(self.opts.limit),
            events: self.events,
            remote,
            final_path,
            name,
            st: Mutex::new(Shared {
                state: DownloadState::Starting,
                plan,
                final_url,
            }),
            bps: AtomicU64::new(0),
            control: tokio::sync::Mutex::new(Control::default()),
            interrupt: watch::channel(0).0,
        });
        (inner, added)
    }
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

impl HttpDownload {
    /// Probes, creates `<final>.part` (preallocated when the size is known) and starts
    /// transferring in the background. Returns once the name and path are known.
    ///
    /// With `opts.start_paused` it writes the sidecar instead and stays `Paused`.
    pub async fn start_fresh(
        id: DownloadId,
        req: AddRequest,
        opts: AddOptions,
        client: reqwest::Client,
        cfg: HttpConfig,
        global: TokenBucket,
        events: broadcast::Sender<EngineEvent>,
    ) -> Result<(Self, Added), EngineError> {
        let Source::Http { url } = &req.source else {
            return Err(EngineError::Unsupported("not an HTTP source"));
        };
        let url = url.clone();
        let start_paused = opts.start_paused;
        let env = Env::new(id, opts, client, cfg, global, events);
        env.emit(DownloadState::Starting);
        let (remote, final_path) = match prepare(&env.client, &url, &req, &env.cfg).await {
            Ok(v) => v,
            Err(e) => {
                env.emit(DownloadState::Failed {
                    reason: e.to_string(),
                });
                return Err(e);
            }
        };
        let plan = Plan::new(remote.total, remote.accepts_ranges, &env.cfg);
        let (inner, added) = env.build(req, remote, final_path, plan);
        if start_paused {
            // Recorded so a restart can restore it, although nothing was transferred.
            if let Err(e) = inner.write_sidecar().await {
                inner.set_state(DownloadState::Failed {
                    reason: e.to_string(),
                });
                let part = part_path(&inner.final_path);
                if let Err(e) = tokio::fs::remove_file(&part).await {
                    tracing::warn!(path = %part.display(), "cannot delete: {e}");
                }
                return Err(e);
            }
            inner.set_state(DownloadState::Paused);
        } else {
            inner.control.lock().await.run = Some(spawn_run(&inner));
        }
        Ok((HttpDownload { inner }, added))
    }

    /// Stops all workers, waits for them, flushes the sidecar; state becomes `Paused`.
    /// No-op unless transferring.
    pub async fn pause(&self) {
        self.inner.interrupt.send_modify(|n| *n += 1);
        let mut c = self.inner.control.lock().await;
        self.inner.stop(&mut c).await;
    }

    /// Re-runs the remaining segments (single streams restart from zero) once the remote
    /// file is validated as unchanged. A changed remote leaves the download in
    /// `NeedsAttention` (`Ok`); a validation error leaves it `Failed` and is returned.
    pub async fn resume(&self) -> Result<(), EngineError> {
        // Subscribed before locking: a pause/cancel issued after this call interrupts it.
        let interrupt = self.inner.interrupt.subscribe();
        let mut c = self.inner.control.lock().await;
        if c.cancelled {
            return Err(EngineError::Invalid("download was cancelled".into()));
        }
        if let Some(h) = &c.run
            && !h.join.is_finished()
        {
            return Ok(());
        }
        if let Some(h) = c.run.take() {
            // Already finished: reaping only surfaces a panic.
            if let Err(e) = h.join.await {
                self.inner.task_failed(e);
            }
        }
        match self.inner.lock().state {
            DownloadState::Completed => return Ok(()),
            DownloadState::NeedsAttention { .. } => {
                return Err(EngineError::Invalid(
                    "download needs attention and cannot be resumed as is".into(),
                ));
            }
            _ => {}
        }
        if !self.inner.validate(interrupt).await? {
            return Ok(());
        }
        c.run = Some(spawn_run(&self.inner));
        Ok(())
    }

    /// Stops the download for good. With `delete_files`, removes `.part`, the sidecar and,
    /// if the download had completed, the final file.
    pub async fn cancel(&self, delete_files: bool) {
        self.inner.interrupt.send_modify(|n| *n += 1);
        let mut c = self.inner.control.lock().await;
        c.cancelled = true;
        self.inner.stop(&mut c).await;
        if !delete_files {
            return;
        }
        // All workers are reaped, so no handle to `.part` is open any more.
        let completed = matches!(self.inner.lock().state, DownloadState::Completed);
        let final_path = self.inner.final_path.clone();
        let mut paths = vec![part_path(&final_path), sidecar_path(&final_path)];
        if completed {
            paths.push(final_path);
        }
        for p in paths {
            match tokio::fs::remove_file(&p).await {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => tracing::warn!(path = %p.display(), "cannot delete: {e}"),
            }
        }
    }

    pub fn status(&self) -> DownloadStatus {
        let st = self.inner.lock();
        let done = st.plan.done_bytes();
        let bps = match st.state {
            DownloadState::Active => self.inner.bps.load(Ordering::Relaxed),
            _ => 0,
        };
        let total = self.inner.remote.total;
        let eta_secs = match total {
            Some(t) if bps > 0 => Some(t.saturating_sub(done).div_ceil(bps)),
            _ => None,
        };
        DownloadStatus {
            state: st.state.clone(),
            name: self.inner.name.clone(),
            total_bytes: total,
            done_bytes: done,
            down_bps: bps,
            up_bps: 0,
            eta_secs,
        }
    }

    pub fn detail(&self) -> EngineDetail {
        let st = self.inner.lock();
        let segments: Vec<SegmentView> = st
            .plan
            .segments
            .iter()
            .map(|e| {
                let active = e.shared.active.load(Ordering::Relaxed);
                SegmentView {
                    start: e.start,
                    end: e.shared.end(),
                    done: e.done(),
                    bps: if active {
                        e.shared.bps.load(Ordering::Relaxed)
                    } else {
                        0
                    },
                    active,
                }
            })
            .collect();
        let connections = segments.iter().filter(|s| s.active).count();
        EngineDetail::Http {
            segments,
            connections: u16::try_from(connections).unwrap_or(u16::MAX),
            resumable: !st.plan.single,
        }
    }

    /// Per-download rate limit in bytes/s; `None` is unlimited.
    pub fn set_limit(&self, bps: Option<u64>) {
        self.inner.limit.set_rate(bps);
    }
}

impl Inner {
    fn lock(&self) -> MutexGuard<'_, Shared> {
        self.st.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Records a transition and emits `StateChanged` (repeated states are not re-emitted).
    fn set_state(&self, state: DownloadState) {
        let mut st = self.lock();
        if std::mem::discriminant(&st.state) == std::mem::discriminant(&state) {
            return;
        }
        st.state = state.clone();
        let _ = self
            .events
            .send(EngineEvent::StateChanged { id: self.id, state });
    }

    fn needs_attention(&self, reason: AttentionReason) {
        self.set_state(DownloadState::NeedsAttention { reason });
    }

    /// Checks that the remote still serves the file `.part` holds bytes of. `Ok(true)`:
    /// continue. `Ok(false)`: now `NeedsAttention`. `Err`: now `Failed` (retryable).
    /// Single streams restart from zero, so there is nothing to validate. A change of
    /// `interrupt` (pause/cancel) abandons validation: `Ok(false)`, now `Paused`.
    async fn validate(&self, mut interrupt: watch::Receiver<u64>) -> Result<bool, EngineError> {
        if self.lock().plan.single {
            return Ok(true);
        }
        let state = self.resume_state();
        let part = part_path(&self.final_path);
        let result = tokio::select! {
            r = self.validate_or_reresolve(&state, &part) => r,
            Ok(()) = interrupt.changed() => {
                self.set_state(DownloadState::Paused);
                return Ok(false);
            }
        };
        match result {
            Ok(Validation::Ok) => Ok(true),
            Ok(Validation::Changed) => {
                self.needs_attention(AttentionReason::RemoteChanged);
                Ok(false)
            }
            Ok(Validation::NotResumable) => {
                self.needs_attention(AttentionReason::NotResumable);
                Ok(false)
            }
            Err(e) => {
                self.set_state(DownloadState::Failed {
                    reason: format!("validating the remote file failed: {e}"),
                });
                Err(e)
            }
        }
    }

    async fn validate_or_reresolve(
        &self,
        state: &HttpResumeState,
        part: &Path,
    ) -> Result<Validation, EngineError> {
        let ctx = &self.request.context;
        match validate_remote(&self.client, ctx, state, part, &self.cfg).await {
            Err(EngineError::Http { status }) if is_url_rejected(status) => {
                Ok(match self.reresolve().await? {
                    true => Validation::Ok,
                    false => Validation::Changed,
                })
            }
            r => r,
        }
    }

    /// The final URL refused a request (401/403/404/410), as an expired signed redirect
    /// target does. Probes the original source again and adopts its new final URL, in
    /// memory and in the sidecar, if it serves the same file: same total and the same
    /// strong ETag, or (no strong ETag) the bytes already downloaded check out.
    /// `Ok(false)`: the source now serves a different file. `Err`: the probe failed.
    async fn reresolve(&self) -> Result<bool, EngineError> {
        let Source::Http { url } = &self.request.source else {
            return Ok(false);
        };
        let read_timeout = Duration::from_millis(self.cfg.read_timeout_ms);
        let info = probe(&self.client, url, &self.request.context, read_timeout).await?;
        let r = &self.remote;
        if info.total != r.total {
            return Ok(false);
        }
        if r.etag_strong {
            if !(info.etag_strong && info.etag == r.etag) {
                return Ok(false);
            }
        } else if !self.lock().plan.single {
            // No strong validator: compare the downloaded bytes against the new URL. (A
            // single stream restarts from zero and checks its length, reusing nothing.)
            let mut state = self.resume_state();
            state.final_url = info.final_url.clone();
            let part = part_path(&self.final_path);
            let ctx = &self.request.context;
            if validate_remote(&self.client, ctx, &state, &part, &self.cfg).await? != Validation::Ok
            {
                return Ok(false);
            }
        }
        tracing::info!(id = self.id, url = %info.final_url, "re-resolved the download URL");
        self.lock().final_url = info.final_url;
        if let Err(e) = self.write_sidecar().await {
            // The old sidecar only makes a restore re-resolve again.
            tracing::warn!(id = self.id, "resume sidecar flush failed: {e}");
        }
        Ok(true)
    }

    fn task_failed(&self, e: tokio::task::JoinError) {
        self.set_state(DownloadState::Failed {
            reason: format!("download task failed: {e}"),
        });
    }

    /// Cancels the run task (if any) and waits for it; `Paused` if it was interrupted.
    async fn stop(&self, c: &mut Control) {
        let Some(h) = c.run.take() else { return };
        h.cancel.cancel();
        match h.join.await {
            Ok(RunEnd::Stopped) => self.set_state(DownloadState::Paused),
            Ok(RunEnd::Finished) => {}
            Err(e) => self.task_failed(e),
        }
    }

    fn resume_state(&self) -> HttpResumeState {
        let (segments, single_stream, final_url) = {
            let st = self.lock();
            let segs = st.plan.segments.iter().map(SegEntry::snapshot).collect();
            (segs, st.plan.single, st.final_url.clone())
        };
        let r = &self.remote;
        HttpResumeState {
            version: VERSION,
            request: without_context(&self.request),
            context_hash: context_hash(&self.request.context),
            final_url,
            total: r.total,
            etag: r.etag.clone(),
            etag_strong: r.etag_strong,
            last_modified: r.last_modified.clone(),
            final_path: self.final_path.clone(),
            segments,
            single_stream,
        }
    }

    /// Writes the sidecar while no transfer is running.
    async fn write_sidecar(&self) -> Result<(), EngineError> {
        let final_path = self.final_path.clone();
        let file = blocking(move || open_part(&final_path)).await?;
        let result = self.resume_state().flush(&file).await;
        drop(file);
        result
    }

    /// Writes the sidecar (data fsync first). Failure is logged: an older sidecar only
    /// under-reports progress, which is safe.
    async fn flush(&self, file: &File) {
        if let Err(e) = self.resume_state().flush(file).await {
            tracing::warn!(id = self.id, "resume sidecar flush failed: {e}");
        }
    }
}
