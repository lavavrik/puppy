//! Segment worker: downloads one byte range over one HTTP connection.

use crate::config::HttpConfig;
use crate::fsutil::write_at;
use crate::http::probe::apply_context;
use crate::limiter::TokenBucket;
use crate::meter::SpeedMeter;
use crate::types::RequestContext;
use futures_util::StreamExt;
use reqwest::StatusCode;
use reqwest::header;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;

/// Live state shared between a worker and the orchestrator.
pub struct SegmentShared {
    /// Bytes written from the segment start.
    pub done: AtomicU64,
    /// Exclusive end offset; may shrink while the worker runs (work stealing).
    pub end: AtomicU64,
    pub bps: AtomicU64,
    pub active: AtomicBool,
}

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
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkerExit {
    Done,
    Cancelled,
    RemoteChanged,
    RangeUnsupported,
    Fatal(String),
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
        if start + seg.done.load(Ordering::Relaxed) >= seg.end.load(Ordering::Relaxed) {
            return WorkerExit::Done;
        }
        seg.active.store(true, Ordering::Relaxed);
        let result = attempt_once(ctx, start, &seg, &mut meter).await;
        seg.active.store(false, Ordering::Relaxed);
        match result {
            Attempt::Exit(e) => return e,
            Attempt::Retry { progress, why } => {
                if progress {
                    attempt = 0;
                }
                if attempt >= ctx.cfg.max_retries {
                    return WorkerExit::Fatal(format!("retries exhausted: {why}"));
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

fn backoff(cfg: &HttpConfig, attempt: u32) -> Duration {
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

/// Parses `bytes <start>-<end>/<total>`; total is `None` for `*`.
fn parse_content_range(v: &str) -> Option<(u64, Option<u64>)> {
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
    let cursor = start + seg.done.load(Ordering::Relaxed);
    let end = seg.end.load(Ordering::Relaxed);
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

    let resp = tokio::select! {
        _ = ctx.cancel.cancelled() => return Attempt::Exit(WorkerExit::Cancelled),
        r = rb.send() => match r {
            Ok(r) => r,
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
            return Attempt::Exit(if ctx.if_range.is_some() {
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
        s => return Attempt::Exit(WorkerExit::Fatal(format!("HTTP {}", s.as_u16()))),
    }

    let mut stream = resp.bytes_stream();
    let mut progress = false;
    loop {
        let item = tokio::select! {
            _ = ctx.cancel.cancelled() => return Attempt::Exit(WorkerExit::Cancelled),
            i = stream.next() => i,
        };
        let chunk = match item {
            Some(Ok(c)) => c,
            Some(Err(e)) => {
                return Attempt::Retry {
                    progress,
                    why: e.to_string(),
                };
            }
            None => {
                let cursor = start + seg.done.load(Ordering::Relaxed);
                if cursor >= seg.end.load(Ordering::Relaxed) {
                    return Attempt::Exit(WorkerExit::Done);
                }
                return Attempt::Retry {
                    progress,
                    why: "connection closed early".into(),
                };
            }
        };
        let cursor = start + seg.done.load(Ordering::Relaxed);
        let end = seg.end.load(Ordering::Relaxed);
        if cursor >= end {
            return Attempt::Exit(WorkerExit::Done);
        }
        let n = (chunk.len() as u64).min(end - cursor);
        if n == 0 {
            continue;
        }
        tokio::select! {
            _ = ctx.cancel.cancelled() => return Attempt::Exit(WorkerExit::Cancelled),
            _ = async {
                ctx.limiters[0].acquire(n).await;
                ctx.limiters[1].acquire(n).await;
            } => {}
        }
        // `end` may have shrunk while waiting for the limiters.
        let end = seg.end.load(Ordering::Relaxed);
        if cursor >= end {
            return Attempt::Exit(WorkerExit::Done);
        }
        let n = n.min(end - cursor) as usize;
        let buf = chunk.slice(..n);
        let file = ctx.file.clone();
        let written = tokio::task::spawn_blocking(move || write_at(&file, &buf, cursor)).await;
        match written {
            Ok(Ok(())) => {}
            Ok(Err(e)) => return Attempt::Exit(WorkerExit::Fatal(e.to_string())),
            Err(e) => return Attempt::Exit(WorkerExit::Fatal(e.to_string())),
        }
        progress = true;
        seg.done.fetch_add(n as u64, Ordering::Relaxed);
        let now = Instant::now();
        meter.record(n as u64, now);
        seg.bps.store(meter.bps(now), Ordering::Relaxed);
        if cursor + n as u64 >= end {
            return Attempt::Exit(WorkerExit::Done);
        }
    }
}
