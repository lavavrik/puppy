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
use crate::http::worker::SegmentShared;
use crate::limiter::TokenBucket;
use crate::types::{
    AddRequest, DownloadId, DownloadState, DownloadStatus, EngineDetail, SegmentView, Source,
};
use files::prepare;
use std::fs::File;
use std::io;
use std::path::PathBuf;
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

    fn done_bytes(&self) -> u64 {
        self.segments.iter().map(SegEntry::done).sum()
    }
}

impl HttpDownload {
    /// Probes, creates `<final>.part` (preallocated when the size is known) and starts
    /// transferring in the background. Returns once the name and path are known.
    pub async fn start_fresh(
        id: DownloadId,
        req: AddRequest,
        client: reqwest::Client,
        mut cfg: HttpConfig,
        global: TokenBucket,
        events: broadcast::Sender<EngineEvent>,
    ) -> Result<(Self, Added), EngineError> {
        let Source::Http { url } = &req.source else {
            return Err(EngineError::Unsupported("not an HTTP source"));
        };
        let url = url.clone();
        cfg.min_segment_bytes = cfg.min_segment_bytes.max(1);
        cfg.connections = cfg.connections.max(1);
        let _ = events.send(EngineEvent::StateChanged {
            id,
            state: DownloadState::Starting,
        });
        let (remote, final_path) = match prepare(&client, &url, &req, &cfg).await {
            Ok(v) => v,
            Err(e) => {
                let _ = events.send(EngineEvent::StateChanged {
                    id,
                    state: DownloadState::Failed {
                        reason: e.to_string(),
                    },
                });
                return Err(e);
            }
        };
        let plan = Plan::new(remote.total, remote.accepts_ranges, &cfg);
        let name = final_path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let added = Added {
            name: name.clone(),
            path: final_path.clone(),
            total_bytes: remote.total,
        };
        let inner = Arc::new(Inner {
            id,
            request: req,
            client,
            cfg,
            global,
            limit: TokenBucket::new(None),
            events,
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
        inner.control.lock().await.run = Some(spawn_run(&inner));
        Ok((HttpDownload { inner }, added))
    }

    /// Stops all workers, waits for them, flushes the sidecar; state becomes `Paused`.
    /// No-op unless transferring.
    pub async fn pause(&self) {
        let mut c = self.inner.control.lock().await;
        self.inner.stop(&mut c).await;
    }

    /// Re-runs the remaining segments (single streams restart from zero).
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
        // Seam: remote validation before continuing is added here by restore support.
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
