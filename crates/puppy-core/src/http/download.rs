//! `HttpDownload`: drives one HTTP download end-to-end — probe, file placement,
//! segmented (or single-stream) transfer with work stealing, crash-safe sidecar,
//! pause/resume/cancel and finalization.
//!
//! Ownership rule (Windows cannot rename/delete a file with an open handle): every handle
//! to `.part` is created inside one `transfer::transfer` call, held only by that call and the
//! worker tasks it spawns, and all workers are reaped before it returns. Finalization and
//! deletion run strictly after `transfer` has returned, so no handle can be open then.

use crate::config::HttpConfig;
use crate::engine::{Added, EngineError, EngineEvent};
use crate::http::probe::RemoteInfo;
use crate::http::resume::{HttpResumeState, VERSION, part_path, sidecar_path};
use crate::http::segments::{Segment, plan_segments};
use crate::http::validate::{Validation, validate_remote};
use crate::http::worker::SegmentShared;
use crate::limiter::TokenBucket;
use crate::types::{
    AddRequest, AttentionReason, DownloadId, DownloadState, DownloadStatus, EngineDetail,
    RequestContext, SegmentView, Source,
};
use files::{file_len, prepare};
use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;
use tokio::sync::broadcast;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use transfer::spawn_run;

mod files;
mod transfer;

const SPEED_SAMPLE: Duration = Duration::from_millis(250);

/// One HTTP download. Control operations (`pause`/`resume`/`cancel`) are serialized.
pub struct HttpDownload {
    inner: Arc<Inner>,
}

struct Inner {
    id: DownloadId,
    request: AddRequest,
    client: reqwest::Client,
    cfg: HttpConfig,
    global: TokenBucket,
    limit: TokenBucket,
    events: broadcast::Sender<EngineEvent>,
    remote: RemoteInfo,
    final_path: PathBuf,
    name: String,
    st: Mutex<Shared>,
    /// Whole-download speed, sampled by the running transfer.
    bps: AtomicU64,
    control: tokio::sync::Mutex<Control>,
}

struct Shared {
    state: DownloadState,
    plan: Plan,
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

    /// The plan recorded in a sidecar. Single streams restart from zero.
    fn restored(state: &HttpResumeState) -> Self {
        if state.single_stream || state.total.is_none() {
            return Plan::single(state.total, false);
        }
        Plan {
            single: false,
            fell_back: false,
            segments: state
                .segments
                .iter()
                .map(|s| SegEntry::new(s.start, s.end, s.done))
                .collect(),
        }
    }

    /// A finished file of `len` bytes.
    fn finished(len: u64) -> Self {
        Plan {
            single: false,
            fell_back: false,
            segments: (len > 0)
                .then(|| SegEntry::new(0, len, len))
                .into_iter()
                .collect(),
        }
    }

    /// Nothing known (no sidecar); never run.
    fn unknown() -> Self {
        Plan {
            single: true,
            fell_back: false,
            segments: Vec::new(),
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
}

impl Env {
    fn new(
        id: DownloadId,
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
        let inner = Arc::new(Inner {
            id: self.id,
            request,
            client: self.client,
            cfg: self.cfg,
            global: self.global,
            limit: TokenBucket::new(None),
            events: self.events,
            remote,
            final_path,
            name,
            st: Mutex::new(Shared {
                state: DownloadState::Starting,
                plan,
            }),
            bps: AtomicU64::new(0),
            control: tokio::sync::Mutex::new(Control::default()),
        });
        (inner, added)
    }
}

/// Request and remote facts recorded in a sidecar.
fn from_sidecar(state: &HttpResumeState, final_path: &Path) -> (AddRequest, RemoteInfo) {
    let remote = RemoteInfo {
        final_url: state.final_url.clone(),
        total: state.total,
        accepts_ranges: !state.single_stream,
        etag: state.etag.clone(),
        etag_strong: state.etag_strong,
        last_modified: state.last_modified.clone(),
        filename: file_name(final_path),
    };
    (state.request.clone(), remote)
}

/// Stand-ins when the sidecar cannot be read: such a download is only ever shown or
/// cancelled, never run.
fn placeholder(final_path: &Path) -> (AddRequest, RemoteInfo) {
    let url = url::Url::from_file_path(final_path)
        .unwrap_or_else(|()| url::Url::parse("file:///").expect("static URL"));
    let request = AddRequest {
        source: Source::Http { url: url.clone() },
        output_dir: final_path.parent().map(Path::to_path_buf),
        filename: Some(file_name(final_path)),
        context: RequestContext::default(),
    };
    let remote = RemoteInfo {
        final_url: url,
        total: None,
        accepts_ranges: false,
        etag: None,
        etag_strong: false,
        last_modified: None,
        filename: file_name(final_path),
    };
    (request, remote)
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// The sidecar's segments must tile `[0, total)` and `.part` must be `total` bytes long.
fn check_layout(state: &HttpResumeState, total: u64, part_len: u64) -> Result<(), String> {
    let mut cursor = 0;
    for s in &state.segments {
        if s.start != cursor {
            return Err(format!("resume sidecar: gap or overlap at offset {cursor}"));
        }
        cursor = s.end;
    }
    if cursor != total {
        return Err(format!(
            "resume sidecar: segments end at {cursor}, file is {total} bytes"
        ));
    }
    if part_len != total {
        return Err(format!(
            "partial file is {part_len} bytes, expected {total}"
        ));
    }
    Ok(())
}

impl HttpDownload {
    /// Probes, creates `<final>.part` (preallocated when the size is known) and starts
    /// transferring in the background. Returns once the name and path are known.
    pub async fn start_fresh(
        id: DownloadId,
        req: AddRequest,
        client: reqwest::Client,
        cfg: HttpConfig,
        global: TokenBucket,
        events: broadcast::Sender<EngineEvent>,
    ) -> Result<(Self, Added), EngineError> {
        let Source::Http { url } = &req.source else {
            return Err(EngineError::Unsupported("not an HTTP source"));
        };
        let url = url.clone();
        let env = Env::new(id, client, cfg, global, events);
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
        inner.control.lock().await.run = Some(spawn_run(&inner));
        Ok((HttpDownload { inner }, added))
    }

    /// Continues a download from its sidecar after a crash or restart, but only once the
    /// remote file is proven unchanged (see [`validate_remote`]). Outcomes that need the
    /// user (`NeedsAttention`) or a retry (`Failed`, from a transient validation error;
    /// `resume` retries) are returned as `Ok` with the download in that state. Files are
    /// never deleted here, except a stale sidecar next to an already finished file.
    pub async fn restore(
        id: DownloadId,
        final_path: &Path,
        client: reqwest::Client,
        cfg: HttpConfig,
        global: TokenBucket,
        events: broadcast::Sender<EngineEvent>,
    ) -> Result<(Self, Added), EngineError> {
        let env = Env::new(id, client, cfg, global, events);
        env.emit(DownloadState::Starting);
        let final_path = final_path.to_path_buf();
        let sidecar = sidecar_path(&final_path);
        let loaded = HttpResumeState::load(&final_path).await;
        let part_len = file_len(&part_path(&final_path)).await?;

        // Finalizing renames `.part` before deleting the sidecar: a final file with a
        // sidecar but no `.part` was completed (and fsynced) before the crash.
        if part_len.is_none()
            && file_len(&sidecar).await?.is_some()
            && let Some(len) = file_len(&final_path).await?
        {
            match tokio::fs::remove_file(&sidecar).await {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => tracing::warn!(path = %sidecar.display(), "cannot delete: {e}"),
            }
            let (request, mut remote) = match &loaded {
                Ok(state) => from_sidecar(state, &final_path),
                Err(_) => placeholder(&final_path),
            };
            remote.total = Some(len);
            let (inner, added) = env.build(request, remote, final_path, Plan::finished(len));
            inner.set_state(DownloadState::Completed);
            let _ = inner.events.send(EngineEvent::Completed { id });
            return Ok((HttpDownload { inner }, added));
        }

        let state = match loaded {
            Ok(s) => s,
            Err(e) => {
                let (request, remote) = placeholder(&final_path);
                let (inner, added) = env.build(request, remote, final_path, Plan::unknown());
                inner.needs_attention(AttentionReason::ValidationFailed {
                    detail: format!("cannot load resume sidecar: {e}"),
                });
                return Ok((HttpDownload { inner }, added));
            }
        };
        let (request, remote) = from_sidecar(&state, &final_path);
        let (inner, added) = env.build(request, remote, final_path, Plan::restored(&state));
        let reason = match (part_len, state.total) {
            (None, _) => Some(AttentionReason::ValidationFailed {
                detail: "partial file is missing".into(),
            }),
            (Some(_), None) => Some(AttentionReason::NotResumable),
            _ if state.single_stream => Some(AttentionReason::NotResumable),
            (Some(len), Some(total)) => check_layout(&state, total, len)
                .err()
                .map(|detail| AttentionReason::ValidationFailed { detail }),
        };
        if let Some(reason) = reason {
            inner.needs_attention(reason);
            return Ok((HttpDownload { inner }, added));
        }
        let mut c = inner.control.lock().await;
        if let Ok(true) = inner.validate().await {
            c.run = Some(spawn_run(&inner));
        }
        drop(c);
        Ok((HttpDownload { inner }, added))
    }

    /// Stops all workers, waits for them, flushes the sidecar; state becomes `Paused`.
    /// No-op unless transferring.
    pub async fn pause(&self) {
        let mut c = self.inner.control.lock().await;
        self.inner.stop(&mut c).await;
    }

    /// Re-runs the remaining segments (single streams restart from zero) once the remote
    /// file is validated as unchanged. A changed remote leaves the download in
    /// `NeedsAttention` (`Ok`); a validation error leaves it `Failed` and is returned.
    pub async fn resume(&self) -> Result<(), EngineError> {
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
        if !self.inner.validate().await? {
            return Ok(());
        }
        c.run = Some(spawn_run(&self.inner));
        Ok(())
    }

    /// Stops the download for good. With `delete_files`, removes `.part`, the sidecar and,
    /// if the download had completed, the final file.
    pub async fn cancel(&self, delete_files: bool) {
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
    /// Single streams restart from zero, so there is nothing to validate.
    async fn validate(&self) -> Result<bool, EngineError> {
        if self.lock().plan.single {
            return Ok(true);
        }
        let state = self.resume_state();
        let part = part_path(&self.final_path);
        match validate_remote(&self.client, &state, &part, &self.cfg).await {
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
        let (segments, single_stream) = {
            let st = self.lock();
            let segs = st.plan.segments.iter().map(SegEntry::snapshot).collect();
            (segs, st.plan.single)
        };
        let r = &self.remote;
        HttpResumeState {
            version: VERSION,
            request: self.request.clone(),
            final_url: r.final_url.clone(),
            total: r.total,
            etag: r.etag.clone(),
            etag_strong: r.etag_strong,
            last_modified: r.last_modified.clone(),
            final_path: self.final_path.clone(),
            segments,
            single_stream,
        }
    }

    /// Writes the sidecar (data fsync first). Failure is logged: an older sidecar only
    /// under-reports progress, which is safe.
    async fn flush(&self, file: &File) {
        if let Err(e) = self.resume_state().flush(file).await {
            tracing::warn!(id = self.id, "resume sidecar flush failed: {e}");
        }
    }
}
