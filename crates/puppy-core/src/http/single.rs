//! Single-stream transfer: one plain GET (no `Range`), written sequentially from offset 0.
//!
//! Used when the server does not support ranges or the size is unknown. Such a transfer
//! cannot continue mid-file, so every attempt (retry or resume) restarts at offset 0.

use crate::fsutil::write_at;
use crate::http::probe::{apply_context, timed};
use crate::http::worker::{SegmentShared, WorkerCtx, WorkerExit, backoff, is_url_rejected};
use crate::meter::SpeedMeter;
use futures_util::StreamExt;
use reqwest::StatusCode;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

enum Attempt {
    Exit(WorkerExit),
    Retry(String),
}

/// Downloads the whole body into `seg` (which must start at 0; its end is the expected
/// total or `u64::MAX` when unknown). `ctx.expected_total`, when set, must match the body
/// length exactly, otherwise the remote is considered changed. `ctx.if_range` is unused.
pub async fn run_single(ctx: &WorkerCtx, seg: Arc<SegmentShared>) -> WorkerExit {
    let mut meter = SpeedMeter::new();
    let mut attempt: u32 = 0;
    // Retries only reset the budget when an attempt gets further than any before it,
    // since each attempt restarts from zero.
    let mut furthest = 0u64;
    loop {
        if ctx.cancel.is_cancelled() {
            return WorkerExit::Cancelled;
        }
        restart(&seg);
        seg.active.store(true, Ordering::Relaxed);
        let result = attempt_once(ctx, &seg, &mut meter).await;
        seg.active.store(false, Ordering::Relaxed);
        seg.bps.store(0, Ordering::Relaxed);
        let why = match result {
            Attempt::Exit(e) => return e,
            Attempt::Retry(why) => why,
        };
        let reached = seg.done.load(Ordering::Acquire);
        if reached > furthest {
            furthest = reached;
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

/// Resets progress to offset 0. Nothing else writes to a single-stream segment.
fn restart(seg: &SegmentShared) {
    let mut b = seg.bounds.lock().unwrap_or_else(|e| e.into_inner());
    seg.done.store(0, Ordering::Release);
    b.claimed = 0;
}

async fn attempt_once(ctx: &WorkerCtx, seg: &SegmentShared, meter: &mut SpeedMeter) -> Attempt {
    let rb = apply_context(ctx.client.get(ctx.url.clone()), &ctx.context);
    let read_timeout = Duration::from_millis(ctx.cfg.read_timeout_ms);
    let resp = tokio::select! {
        _ = ctx.cancel.cancelled() => return Attempt::Exit(WorkerExit::Cancelled),
        r = timed(read_timeout, rb.send()) => match r {
            Ok(Ok(r)) => r,
            Ok(Err(e)) => return Attempt::Retry(e.to_string()),
            Err(e) => return Attempt::Retry(e.to_string()),
        },
    };
    match resp.status() {
        StatusCode::OK => {}
        s if s.is_server_error()
            || s == StatusCode::TOO_MANY_REQUESTS
            || s == StatusCode::REQUEST_TIMEOUT =>
        {
            return Attempt::Retry(format!("HTTP {}", s.as_u16()));
        }
        s if is_url_rejected(s.as_u16()) => {
            return Attempt::Exit(WorkerExit::UrlRejected(s.as_u16()));
        }
        s => return Attempt::Exit(WorkerExit::Fatal(format!("HTTP {}", s.as_u16()))),
    }
    if let (Some(exp), Some(len)) = (ctx.expected_total, resp.content_length())
        && exp != len
    {
        return Attempt::Exit(WorkerExit::RemoteChanged);
    }

    let mut stream = resp.bytes_stream();
    loop {
        let item = tokio::select! {
            _ = ctx.cancel.cancelled() => return Attempt::Exit(WorkerExit::Cancelled),
            i = timed(read_timeout, stream.next()) => i,
        };
        let chunk = match item {
            Ok(Some(Ok(c))) => c,
            Ok(Some(Err(e))) => return Attempt::Retry(e.to_string()),
            Err(e) => return Attempt::Retry(e.to_string()),
            Ok(None) => {
                let done = seg.done.load(Ordering::Acquire);
                return match ctx.expected_total {
                    Some(exp) if done < exp => Attempt::Retry("connection closed early".into()),
                    _ => Attempt::Exit(WorkerExit::Done),
                };
            }
        };
        if chunk.is_empty() {
            continue;
        }
        let cursor = seg.done.load(Ordering::Acquire);
        let n = chunk.len() as u64;
        let next = cursor.saturating_add(n);
        if next > seg.end() {
            // Longer than announced: the file is not what was probed.
            return Attempt::Exit(WorkerExit::RemoteChanged);
        }
        tokio::select! {
            _ = ctx.cancel.cancelled() => return Attempt::Exit(WorkerExit::Cancelled),
            _ = async {
                ctx.limiters[0].acquire(n).await;
                ctx.limiters[1].acquire(n).await;
            } => {}
        }
        seg.bounds.lock().unwrap_or_else(|e| e.into_inner()).claimed = next;
        let file = ctx.file.clone();
        let written = tokio::task::spawn_blocking(move || write_at(&file, &chunk, cursor)).await;
        match written {
            Ok(Ok(())) => {}
            Ok(Err(e)) => return Attempt::Exit(WorkerExit::Fatal(e.to_string())),
            Err(e) => return Attempt::Exit(WorkerExit::Fatal(e.to_string())),
        }
        seg.done.store(next, Ordering::Release);
        let now = Instant::now();
        meter.record(n, now);
        seg.bps.store(meter.bps(now), Ordering::Relaxed);
    }
}
