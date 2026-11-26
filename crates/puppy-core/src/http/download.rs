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
use crate::http::resume::{
    HttpResumeState, VERSION, context_hash, part_path, sidecar_path, without_context,
};
use crate::http::segments::{Segment, plan_segments};
use crate::http::validate::Validation;
use crate::http::worker::{SegmentShared, is_url_rejected};
use crate::limiter::TokenBucket;
use crate::types::{
    AddRequest, AttentionReason, DownloadId, DownloadState, DownloadStatus, EngineDetail,
    SegmentView, Source, without_password,
};
use files::{blocking, open_part, prepare};
use mirrors::{DropKind, Health, MirrorRt, Validators};
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
mod mirrors;
mod restore;
mod transfer;
mod transport;
mod verify;

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
    /// Facts about the file from the probe (size, range support, name). Each location's
    /// own final URL and validators are in `Shared::mirrors`.
    remote: RemoteInfo,
    final_path: PathBuf,
    name: String,
    st: Mutex<Shared>,
    /// Whole-download speed, sampled by the running transfer.
    bps: AtomicU64,
    /// Bytes hashed so far while `Verifying`.
    hashed: AtomicU64,
    control: tokio::sync::Mutex<Control>,
    /// Bumped by `pause`/`cancel` before they wait for the control lock, so they can
    /// interrupt a validation in progress.
    interrupt: watch::Sender<u64>,
}

struct Shared {
    state: DownloadState,
    plan: Plan,
    /// The primary URL first, then the mirrors. A location's final URL (where its
    /// requests go) is replaced when an expired redirect target is re-resolved.
    mirrors: Vec<MirrorRt>,
    /// Where the finished file is (or will be): `final_path` unless finalizing found that
    /// name taken.
    path: PathBuf,
    /// The file is complete and at `path` (`.part` renamed), but not yet `Completed`
    /// unless verified: a run only verifies it.
    finalized: bool,
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
        mirrors: Vec<MirrorRt>,
        final_path: PathBuf,
        plan: Plan,
    ) -> (Arc<Inner>, Added) {
        let name = file_name(&final_path);
        let added = Added {
            name: name.clone(),
            path: final_path.clone(),
            total_bytes: remote.total,
        };
        let path = final_path.clone();
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
                mirrors,
                path,
                finalized: false,
            }),
            bps: AtomicU64::new(0),
            hashed: AtomicU64::new(0),
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
        if !matches!(req.source, Source::Http { .. }) {
            return Err(EngineError::Unsupported("not an HTTP source"));
        }
        let start_paused = opts.start_paused;
        let env = Env::new(id, opts, client, cfg, global, events);
        env.emit(DownloadState::Starting);
        let (remote, mirrors, final_path) = match prepare(&env.client, &req, &env.cfg).await {
            Ok(v) => v,
            Err(e) => {
                env.emit(DownloadState::Failed {
                    reason: e.to_string(),
                    transient: e.is_transient(),
                });
                return Err(e);
            }
        };
        let plan = Plan::new(remote.total, remote.accepts_ranges, &env.cfg);
        let (inner, added) = env.build(req, remote, mirrors, final_path, plan);
        if start_paused {
            // Recorded so a restart can restore it, although nothing was transferred.
            if let Err(e) = inner.write_sidecar().await {
                inner.set_state(DownloadState::Failed {
                    reason: e.to_string(),
                    transient: e.is_transient(),
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
        {
            // A resume tries every mirror again; validation decides which are used.
            let mut st = self.inner.lock();
            if !st.plan.single {
                mirrors::reset(&mut st.mirrors);
            }
        }
        if !self.inner.validate(interrupt).await? {
            return Ok(());
        }
        c.run = Some(spawn_run(&self.inner));
        Ok(())
    }

    /// Stops the download for good. With `delete_files`, removes `.part`, the sidecar and,
    /// if the download had completed (or finished but failed its checksum), the final
    /// file.
    pub async fn cancel(&self, delete_files: bool) {
        self.inner.interrupt.send_modify(|n| *n += 1);
        let mut c = self.inner.control.lock().await;
        c.cancelled = true;
        self.inner.stop(&mut c).await;
        if !delete_files {
            return;
        }
        // All workers are reaped, so no handle to `.part` is open any more.
        let (completed, path) = {
            let st = self.inner.lock();
            (
                st.finalized || matches!(st.state, DownloadState::Completed),
                st.path.clone(),
            )
        };
        let final_path = &self.inner.final_path;
        let mut paths = vec![part_path(final_path), sidecar_path(final_path)];
        if completed {
            paths.push(path);
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
        let mut done = st.plan.done_bytes();
        let bps = match st.state {
            DownloadState::Active => self.inner.bps.load(Ordering::Relaxed),
            _ => 0,
        };
        let mut total = self.inner.remote.total;
        if st.state == DownloadState::Verifying {
            // The progress of the check (the size of a stream of unknown length is
            // known by now).
            total = total.or(Some(done));
            done = self.inner.hashed.load(Ordering::Relaxed);
        }
        let eta_secs = match total {
            Some(t) if bps > 0 => Some(t.saturating_sub(done).div_ceil(bps)),
            _ => None,
        };
        DownloadStatus {
            state: st.state.clone(),
            name: self.inner.name.clone(),
            path: st.path.clone(),
            total_bytes: total,
            done_bytes: done,
            down_bps: bps,
            up_bps: 0,
            eta_secs,
            peers: None,
            note: None,
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
                let done = e.done();
                // An unknown-length stream has no real end (internally `u64::MAX`).
                let end = match self.inner.remote.total {
                    None if st.plan.single => e.start + done,
                    _ => e.shared.end(),
                };
                SegmentView {
                    start: e.start,
                    end,
                    done,
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
        let now = std::time::Instant::now();
        let mirrors = match st.mirrors.as_slice() {
            [_] | [] => Vec::new(),
            all => all.iter().map(|m| m.view(now)).collect(),
        };
        EngineDetail::Http {
            segments,
            connections: u16::try_from(connections).unwrap_or(u16::MAX),
            resumable: !st.plan.single,
            mirrors,
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
        if st.state == state {
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
        // A finished file (awaiting its checksum check) needs nothing from the remote.
        if self.lock().plan.single || self.lock().finalized {
            return Ok(true);
        }
        let state = self.resume_state();
        let part = part_path(&self.final_path);
        let several = self.lock().mirrors.len() > 1;
        let result = tokio::select! {
            r = async {
                if several {
                    self.validate_mirrors(&state, &part).await
                } else {
                    self.validate_or_reresolve(&state, &part).await
                }
            } => r,
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
                    transient: e.is_transient(),
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
        let remote = self.lock().mirrors[0].remote();
        match remote.validate(&self.client, state, part, &self.cfg).await {
            Err(EngineError::Http { status }) if is_url_rejected(status) => {
                Ok(match self.reresolve(0, true).await? {
                    true => Validation::Ok,
                    false => Validation::Changed,
                })
            }
            r => r,
        }
    }

    /// Validates every location of a download with mirrors on its own (concurrently),
    /// keeping the ones that still serve the file `.part` holds bytes of and dropping the
    /// others with their reason. `Ok` if at least one is usable; otherwise an error if
    /// any failed to answer (a retry may find it working), else `Changed` if any serves
    /// another file now, else `NotResumable`.
    async fn validate_mirrors(
        &self,
        state: &HttpResumeState,
        part: &Path,
    ) -> Result<Validation, EngineError> {
        let n = self.lock().mirrors.len();
        let checks =
            futures_util::future::join_all((0..n).map(|i| self.check_mirror(i, state, part))).await;
        let mut usable = false;
        let mut error = None;
        let mut changed = false;
        let mut primary_changed = false;
        let now = std::time::Instant::now();
        for (m, check) in self.lock().mirrors.iter_mut().zip(checks) {
            match check {
                Check::Ok(learned) => {
                    usable = true;
                    m.health = Health::Usable;
                    if let Some(v) = learned {
                        m.v = v;
                    }
                }
                Check::Changed(why) => {
                    changed = true;
                    // The primary had validators, so its file did change: as without
                    // mirrors, that needs attention.
                    primary_changed |= m.primary && !m.v.is_empty();
                    m.drop_with(DropKind::Changed, why);
                }
                Check::NoRanges => {
                    m.drop_with(DropKind::RangeUnsupported, "does not honour range requests")
                }
                // It may answer later: benched, and checked again before it gets a range
                // (`If-Range`, or a byte comparison without validators).
                Check::Failed(e) if e.is_transient() => {
                    m.failed(format!("validation failed: {e}"), false, &self.cfg, now);
                    error = error.or(Some(e));
                }
                Check::Failed(e) => {
                    let kind = mirrors::kind_of(&e);
                    m.drop_with(kind, format!("validation failed: {e}"));
                    error = error.or(Some(e));
                }
            }
        }
        if primary_changed {
            return Ok(Validation::Changed);
        }
        if usable {
            if let Err(e) = self.write_sidecar().await {
                // The old sidecar only makes a restore validate the mirrors again.
                tracing::warn!(id = self.id, "resume sidecar flush failed: {e}");
            }
            return Ok(Validation::Ok);
        }
        match error {
            Some(e) => Err(e),
            None if changed => Ok(Validation::Changed),
            None => Ok(Validation::NotResumable),
        }
    }

    /// Whether location `i` still serves the file. With recorded validators it is
    /// validated like a download without mirrors (re-resolving an expired redirect).
    /// Without any (it never answered, or offers neither ETag nor Last-Modified) it is
    /// probed (same size and range support, as for a fresh start) and the bytes already
    /// downloaded are compared against it, whichever location they came from.
    async fn check_mirror(&self, i: usize, base: &HttpResumeState, part: &Path) -> Check {
        let (url, remote, v) = {
            let st = self.lock();
            let m = &st.mirrors[i];
            (m.url.clone(), m.remote(), m.v.clone())
        };
        let mut learned = None;
        let state = if v.is_empty() {
            let info = match remote.probe(&self.client, &self.cfg).await {
                Ok(info) => info,
                Err(e) => return Check::Failed(e),
            };
            if info.total != base.total {
                return Check::Changed(mirrors::size_differs(info.total, base.total));
            }
            if !info.accepts_ranges {
                return Check::NoRanges;
            }
            // Compare bytes: no validator is trusted for bytes it did not serve.
            let state = state_with(base, &Validators::unknown(&info.final_url));
            learned = Some(Validators::of(&info));
            state
        } else {
            state_with(base, &v)
        };
        let result = remote.validate(&self.client, &state, part, &self.cfg).await;
        let result = match result {
            Err(EngineError::Http { status })
                if is_url_rejected(status) && state.final_url != url && !remote.is_ftp() =>
            {
                match self.reresolve(i, false).await {
                    Ok(true) => {
                        // Keep the URL re-resolution found, not the probe's stale one.
                        if let Some(v) = &mut learned {
                            v.final_url = self.lock().mirrors[i].v.final_url.clone();
                        }
                        Ok(Validation::Ok)
                    }
                    Ok(false) => Ok(Validation::Changed),
                    Err(e) => Err(e),
                }
            }
            r => r,
        };
        match result {
            Ok(Validation::Ok) => Check::Ok(learned),
            Ok(Validation::Changed) => {
                Check::Changed("the file changed since the download started".into())
            }
            Ok(Validation::NotResumable) => Check::NoRanges,
            Err(e) => Check::Failed(e),
        }
    }

    /// Location `i`'s final URL refused a request (401/403/404/410), as an expired
    /// signed redirect target does. Probes its URL again and adopts the new final URL,
    /// in memory and (with `persist`) in the sidecar, if it serves the same file: same
    /// total and the same strong ETag, or (no strong ETag) the bytes already downloaded
    /// check out. `Ok(false)`: it now serves a different file. `Err`: the probe failed.
    async fn reresolve(&self, i: usize, persist: bool) -> Result<bool, EngineError> {
        let (remote, v) = {
            let st = self.lock();
            let m = &st.mirrors[i];
            (m.remote(), m.v.clone())
        };
        let info = remote.probe(&self.client, &self.cfg).await?;
        if info.total != self.remote.total {
            return Ok(false);
        }
        if v.etag_strong {
            if !(info.etag_strong && info.etag == v.etag) {
                return Ok(false);
            }
        } else if !self.lock().plan.single {
            // No strong validator: compare the downloaded bytes against the new URL. (A
            // single stream restarts from zero and checks its length, reusing nothing.)
            let mut state = state_with(&self.resume_state(), &v);
            state.final_url = info.final_url.clone();
            let part = part_path(&self.final_path);
            if remote
                .validate(&self.client, &state, &part, &self.cfg)
                .await?
                != Validation::Ok
            {
                return Ok(false);
            }
        }
        tracing::info!(id = self.id, url = %info.final_url, "re-resolved the download URL");
        self.lock().mirrors[i].v.final_url = info.final_url;
        if persist && let Err(e) = self.write_sidecar().await {
            // The old sidecar only makes a restore re-resolve again.
            tracing::warn!(id = self.id, "resume sidecar flush failed: {e}");
        }
        Ok(true)
    }

    fn task_failed(&self, e: tokio::task::JoinError) {
        self.set_state(DownloadState::Failed {
            reason: format!("download task failed: {e}"),
            transient: false,
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

    #[cfg(test)]
    fn for_test(events: broadcast::Sender<EngineEvent>) -> Arc<Inner> {
        let url = url::Url::parse("http://127.0.0.1:9/f").expect("static URL");
        let env = Env::new(
            1,
            AddOptions::default(),
            reqwest::Client::new(),
            HttpConfig::default(),
            TokenBucket::new(None),
            events,
        );
        let remote = RemoteInfo {
            final_url: url.clone(),
            total: None,
            accepts_ranges: false,
            etag: None,
            etag_strong: false,
            last_modified: None,
            filename: "f".into(),
        };
        let req = AddRequest {
            source: Source::Http { url },
            output_dir: None,
            filename: None,
            context: Default::default(),
            files: Default::default(),
            seed: None,
            checksum: None,
            mirrors: Vec::new(),
        };
        let mirrors = mirrors::locations(&req);
        env.build(
            req,
            remote,
            mirrors,
            PathBuf::from("f"),
            Plan::single(None, false),
        )
        .0
    }

    fn resume_state(&self) -> HttpResumeState {
        let (segments, single_stream, primary, mirrors) = {
            let st = self.lock();
            let segs = st.plan.segments.iter().map(SegEntry::snapshot).collect();
            let primary = st.mirrors[0].v.clone();
            let mirrors = st.mirrors[1..].iter().map(MirrorRt::record).collect();
            (segs, st.plan.single, primary, mirrors)
        };
        HttpResumeState {
            version: VERSION,
            request: without_context(&self.request),
            context_hash: context_hash(&self.request.context),
            final_url: without_password(&primary.final_url),
            total: self.remote.total,
            etag: primary.etag,
            etag_strong: primary.etag_strong,
            last_modified: primary.last_modified,
            final_path: self.final_path.clone(),
            segments,
            single_stream,
            mirrors,
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

    /// [`Inner::flush`], unless the segments are unchanged since `last` was flushed.
    async fn flush_if_changed(&self, file: &File, last: &mut Option<Vec<Segment>>) {
        let state = self.resume_state();
        if last.as_ref() == Some(&state.segments) {
            return;
        }
        match state.flush(file).await {
            Ok(()) => *last = Some(state.segments),
            Err(e) => tracing::warn!(id = self.id, "resume sidecar flush failed: {e}"),
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

/// How one location fared in [`Inner::validate_mirrors`].
enum Check {
    /// Still serves the file; the validators it was found to have, if newly learned.
    Ok(Option<Validators>),
    Changed(String),
    NoRanges,
    Failed(EngineError),
}

/// `base` (the sidecar state) with the final URL and validators of one location.
fn state_with(base: &HttpResumeState, v: &Validators) -> HttpResumeState {
    HttpResumeState {
        final_url: v.final_url.clone(),
        etag: v.etag.clone(),
        etag_strong: v.etag_strong,
        last_modified: v.last_modified.clone(),
        ..base.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn set_state_propagates_a_changed_failure_reason() {
        let (tx, mut rx) = broadcast::channel(16);
        let inner = Inner::for_test(tx);
        let failed = |r: &str| DownloadState::Failed {
            reason: r.into(),
            transient: false,
        };
        inner.set_state(failed("a"));
        inner.set_state(failed("b"));
        inner.set_state(failed("b")); // unchanged: not re-emitted
        let mut seen = Vec::new();
        while let Ok(EngineEvent::StateChanged { state, .. }) = rx.try_recv() {
            seen.push(state);
        }
        assert_eq!(seen, vec![failed("a"), failed("b")]);
        assert_eq!(inner.lock().state, failed("b"));
    }
}
