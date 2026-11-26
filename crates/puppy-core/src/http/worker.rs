//! Segment worker: downloads one byte range over one HTTP connection.

use crate::config::HttpConfig;
use crate::fsutil::write_at;
use crate::http::probe::{apply_context, timed};
use crate::limiter::TokenBucket;
use crate::meter::SpeedMeter;
use crate::types::RequestContext;
use futures_util::StreamExt;
use reqwest::StatusCode;
use reqwest::header;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;

/// Range limits of a segment, guarded by a lock so that shrinking `end` (stealing) and
/// claiming bytes to write can never interleave.
#[derive(Debug, Clone, Copy)]
pub struct Bounds {
    /// Exclusive end offset; shrinks only via [`SegmentShared::try_steal`].
    pub end: u64,
    /// Absolute offset up to which the worker has committed to write.
    pub claimed: u64,
}

/// Live state shared between a worker and the orchestrator.
pub struct SegmentShared {
    /// Bytes written from the segment start.
    pub done: AtomicU64,
    pub bounds: Mutex<Bounds>,
    pub bps: AtomicU64,
    pub active: AtomicBool,
}

impl SegmentShared {
    pub fn new(start: u64, end: u64, done: u64) -> Self {
        debug_assert!(start + done <= end);
        SegmentShared {
            done: AtomicU64::new(done),
            bounds: Mutex::new(Bounds {
                end,
                claimed: start + done,
            }),
            bps: AtomicU64::new(0),
            active: AtomicBool::new(false),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Bounds> {
        self.bounds.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn end(&self) -> u64 {
        self.lock().end
    }

    /// Bytes not yet claimed by the worker.
    pub fn remaining(&self) -> u64 {
        let b = self.lock();
        b.end.saturating_sub(b.claimed)
    }

    /// Splits off the back half of the unclaimed range. Returns the thief's
    /// `(start, end)`, or `None` if less than `2 * min_segment` remains.
    pub fn try_steal(&self, start: u64, min_segment: u64) -> Option<(u64, u64)> {
        let mut b = self.lock();
        let from = b.claimed.max(start + self.done.load(Ordering::Acquire));
        let remaining = b.end.saturating_sub(from);
        if remaining < min_segment.saturating_mul(2) {
            return None;
        }
        let mid = from + remaining / 2;
        let old_end = b.end;
        b.end = mid;
        Some((mid, old_end))
    }
}

#[derive(Clone)]
pub struct WorkerCtx {
    pub client: reqwest::Client,
    pub url: url::Url,
    pub context: RequestContext,
    /// Strong ETag or Last-Modified, sent as `If-Range`.
    pub if_range: Option<String>,
    pub expected_total: Option<u64>,
    pub file: Arc<std::fs::File>,
    pub limiters: [TokenBucket; 2],
    pub cfg: HttpConfig,
    pub cancel: CancellationToken,
    /// The download has other mirrors to fail over to: a retryable failure ends the
    /// worker at once ([`WorkerExit::Retryable`]) instead of being retried here, so the
    /// range can move to another mirror.
    pub failover: bool,
    /// Bytes received from this location (shared by its workers).
    pub got: Arc<AtomicU64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkerExit {
    Done,
    Cancelled,
    RemoteChanged,
    RangeUnsupported,
    /// The URL answered 401/403/404/410: possibly an expired signed redirect target.
    UrlRejected(u16),
    /// A local failure (writing the file): no other server can help.
    Fatal(String),
    /// The server answered with something unusable (an unexpected status, compressed
    /// content): this location cannot serve the range.
    Refused(String),
    /// With `failover`: one attempt failed on an error that may go away by itself;
    /// `progress` if it wrote bytes first.
    Retryable {
        why: String,
        progress: bool,
    },
    /// Every retry failed on errors that may go away by themselves (network errors,
    /// timeouts, HTTP 5xx/429/408).
    Exhausted(String),
}

/// While a rate limit applies, body chunks are taken (limiter tokens acquired and the
/// bytes written) in slices of at most this size, so a low limit gives smooth progress
/// instead of a long wait followed by a whole network chunk at once.
pub(crate) const LIMITED_SLICE: usize = 16 * 1024;

impl WorkerCtx {
    /// How many bytes of a `len`-byte chunk to take at once.
    pub(crate) fn slice_len(&self, len: usize) -> usize {
        if self.limiters.iter().any(TokenBucket::is_limited) {
            len.min(LIMITED_SLICE)
        } else {
            len
        }
    }

    /// Waits for `n` bytes' worth of tokens from both limiters; `false` if cancelled.
    pub(crate) async fn acquire(&self, n: u64) -> bool {
        tokio::select! {
            _ = self.cancel.cancelled() => false,
            _ = async {
                self.limiters[0].acquire(n).await;
                self.limiters[1].acquire(n).await;
            } => true,
        }
    }
}

/// Statuses with which an (expired) signed URL typically refuses a request.
pub(crate) fn is_url_rejected(status: u16) -> bool {
    matches!(status, 401 | 403 | 404 | 410)
}

/// Result of one request attempt.
enum Attempt {
    Exit(WorkerExit),
    /// Retryable failure; `progress` is true if at least one byte was written.
    Retry {
        progress: bool,
        why: String,
    },
}

struct ActiveGuard<'a>(&'a AtomicBool);

impl Drop for ActiveGuard<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Relaxed);
    }
}

pub async fn run_segment(ctx: &WorkerCtx, start: u64, seg: Arc<SegmentShared>) -> WorkerExit {
    let _guard = ActiveGuard(&seg.active);
    let mut meter = SpeedMeter::new();
    let mut attempt: u32 = 0;
    loop {
        if ctx.cancel.is_cancelled() {
            return WorkerExit::Cancelled;
        }
        if start + seg.done.load(Ordering::Acquire) >= seg.end() {
            return WorkerExit::Done;
        }
        seg.active.store(true, Ordering::Relaxed);
        let result = attempt_once(ctx, start, &seg, &mut meter).await;
        seg.active.store(false, Ordering::Relaxed);
        match result {
            Attempt::Exit(e) => return e,
            Attempt::Retry { progress, why } if ctx.failover => {
                return WorkerExit::Retryable { why, progress };
            }
            Attempt::Retry { progress, why } => {
                if progress {
                    attempt = 0;
                }
                if attempt >= ctx.cfg.max_retries {
                    return WorkerExit::Exhausted(format!("retries exhausted: {why}"));
                }
                let delay = backoff(&ctx.cfg, attempt);
                attempt += 1;
                tokio::select! {
                    _ = ctx.cancel.cancelled() => return WorkerExit::Cancelled,
                    _ = tokio::time::sleep(delay) => {}
                }
            }
        }
    }
}

pub(crate) fn backoff(cfg: &HttpConfig, attempt: u32) -> Duration {
    let exp = cfg
        .backoff_base_ms
        .saturating_mul(1u64.checked_shl(attempt).unwrap_or(u64::MAX))
        .min(cfg.backoff_max_ms);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| u64::from(d.subsec_nanos()))
        .unwrap_or(0);
    let jitter = if exp >= 4 { nanos % (exp / 4) } else { 0 };
    Duration::from_millis(exp + jitter)
}

/// A body with a `Content-Encoding` is not the file's bytes (we only accept `identity`):
/// returns the failure reason.
pub(crate) fn compressed(h: &header::HeaderMap) -> Option<String> {
    let enc = h.get(header::CONTENT_ENCODING)?;
    let enc = String::from_utf8_lossy(enc.as_bytes()).trim().to_string();
    (!enc.is_empty() && !enc.eq_ignore_ascii_case("identity"))
        .then(|| format!("server sent compressed content ({enc})"))
}

/// True if the response's ETag or Last-Modified equals the `If-Range` validator.
fn carries_validator(h: &header::HeaderMap, validator: &str) -> bool {
    [header::ETAG, header::LAST_MODIFIED]
        .into_iter()
        .any(|name| {
            h.get(name)
                .and_then(|v| v.to_str().ok())
                .is_some_and(|v| v.trim() == validator)
        })
}

/// Parses `bytes <start>-<end>/<total>`; total is `None` for `*`.
pub(crate) fn parse_content_range(v: &str) -> Option<(u64, Option<u64>)> {
    let rest = v.trim().strip_prefix("bytes")?.trim_start();
    let (range, total) = rest.split_once('/')?;
    let (s, _e) = range.split_once('-')?;
    let total = match total.trim() {
        "*" => None,
        t => Some(t.parse().ok()?),
    };
    Some((s.trim().parse().ok()?, total))
}

async fn attempt_once(
    ctx: &WorkerCtx,
    start: u64,
    seg: &SegmentShared,
    meter: &mut SpeedMeter,
) -> Attempt {
    let cursor = start + seg.done.load(Ordering::Acquire);
    let end = seg.end();
    if cursor >= end {
        return Attempt::Exit(WorkerExit::Done);
    }
    let mut rb = ctx
        .client
        .get(ctx.url.clone())
        .header(header::RANGE, format!("bytes={}-{}", cursor, end - 1));
    if let Some(v) = &ctx.if_range {
        rb = rb.header(header::IF_RANGE, v);
    }
    let rb = apply_context(rb, &ctx.context);

    let read_timeout = Duration::from_millis(ctx.cfg.read_timeout_ms);
    let resp = tokio::select! {
        _ = ctx.cancel.cancelled() => return Attempt::Exit(WorkerExit::Cancelled),
        r = timed(read_timeout, rb.send()) => match r {
            Ok(Ok(r)) => r,
            Ok(Err(e)) => return Attempt::Retry { progress: false, why: e.to_string() },
            Err(e) => return Attempt::Retry { progress: false, why: e.to_string() },
        },
    };

    let status = resp.status();
    match status {
        StatusCode::PARTIAL_CONTENT => {
            let cr = resp
                .headers()
                .get(header::CONTENT_RANGE)
                .and_then(|v| v.to_str().ok())
                .and_then(parse_content_range);
            match cr {
                Some((s, total)) if s == cursor => {
                    if let (Some(exp), Some(t)) = (ctx.expected_total, total)
                        && exp != t
                    {
                        return Attempt::Exit(WorkerExit::RemoteChanged);
                    }
                }
                _ => return Attempt::Exit(WorkerExit::RangeUnsupported),
            }
        }
        StatusCode::OK => {
            // Under If-Range a 200 means "changed" — unless the response still carries our
            // validator, in which case the server simply ignores ranges.
            let changed = ctx
                .if_range
                .as_deref()
                .is_some_and(|v| !carries_validator(resp.headers(), v));
            return Attempt::Exit(if changed {
                WorkerExit::RemoteChanged
            } else {
                WorkerExit::RangeUnsupported
            });
        }
        s if s.is_server_error()
            || s == StatusCode::TOO_MANY_REQUESTS
            || s == StatusCode::REQUEST_TIMEOUT =>
        {
            return Attempt::Retry {
                progress: false,
                why: format!("HTTP {}", s.as_u16()),
            };
        }
        // The range lies beyond the end of the file: it shrank.
        StatusCode::RANGE_NOT_SATISFIABLE => return Attempt::Exit(WorkerExit::RemoteChanged),
        s if is_url_rejected(s.as_u16()) => {
            return Attempt::Exit(WorkerExit::UrlRejected(s.as_u16()));
        }
        s => return Attempt::Exit(WorkerExit::Refused(format!("HTTP {}", s.as_u16()))),
    }

    if let Some(why) = compressed(resp.headers()) {
        return Attempt::Exit(WorkerExit::Refused(why));
    }
    let mut stream = resp.bytes_stream();
    let mut progress = false;
    loop {
        let item = tokio::select! {
            _ = ctx.cancel.cancelled() => return Attempt::Exit(WorkerExit::Cancelled),
            i = timed(read_timeout, stream.next()) => i,
        };
        let chunk = match item {
            Ok(Some(Ok(c))) => c,
            Ok(Some(Err(e))) => {
                return Attempt::Retry {
                    progress,
                    why: e.to_string(),
                };
            }
            Err(e) => {
                return Attempt::Retry {
                    progress,
                    why: e.to_string(),
                };
            }
            Ok(None) => {
                let cursor = start + seg.done.load(Ordering::Acquire);
                if cursor >= seg.end() {
                    return Attempt::Exit(WorkerExit::Done);
                }
                return Attempt::Retry {
                    progress,
                    why: "connection closed early".into(),
                };
            }
        };
        let mut off = 0;
        while off < chunk.len() {
            let len = ctx.slice_len(chunk.len() - off);
            let cursor = start + seg.done.load(Ordering::Acquire);
            let want = (len as u64).min(seg.end().saturating_sub(cursor));
            if want == 0 {
                return Attempt::Exit(WorkerExit::Done);
            }
            if !ctx.acquire(want).await {
                return Attempt::Exit(WorkerExit::Cancelled);
            }
            // Claim under the lock: `end` may have shrunk while waiting for the limiters.
            let n = {
                let mut b = seg.lock();
                let n = (len as u64).min(b.end.saturating_sub(cursor));
                if n > 0 {
                    b.claimed = cursor + n;
                }
                n
            };
            if n == 0 {
                return Attempt::Exit(WorkerExit::Done);
            }
            let buf = chunk.slice(off..off + n as usize);
            let file = ctx.file.clone();
            let written = tokio::task::spawn_blocking(move || write_at(&file, &buf, cursor)).await;
            match written {
                Ok(Ok(())) => {}
                Ok(Err(e)) => return Attempt::Exit(WorkerExit::Fatal(e.to_string())),
                Err(e) => return Attempt::Exit(WorkerExit::Fatal(e.to_string())),
            }
            progress = true;
            seg.done.fetch_add(n, Ordering::Release);
            ctx.got.fetch_add(n, Ordering::Relaxed);
            debug_assert!(start + seg.done.load(Ordering::Acquire) <= seg.lock().end);
            let now = Instant::now();
            meter.record(n, now);
            seg.bps.store(meter.bps(now), Ordering::Relaxed);
            if cursor + n >= seg.end() {
                return Attempt::Exit(WorkerExit::Done);
            }
            off += n as usize;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn try_steal_respects_claimed() {
        let s = SegmentShared::new(1000, 11_000, 0);
        s.lock().claimed = 5000; // committed ahead of `done`
        let (mid, end) = s.try_steal(1000, 1000).unwrap();
        assert_eq!((mid, end), (8000, 11_000));
        assert_eq!(s.end(), 8000);
        assert_eq!(s.remaining(), 3000);
        assert!(s.try_steal(1000, 2000).is_none());
    }
}
