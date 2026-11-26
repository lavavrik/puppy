//! SFTP segment worker: downloads one byte range over one connection, with the same
//! contract as the HTTP and FTP workers ([`crate::http::worker::run_segment`]): it
//! writes into the shared `.part` through [`SegmentShared`] (so work stealing applies),
//! honours both rate limiters and retries with the usual backoff.
//!
//! Each attempt opens the file and checks the open handle's size and modification time
//! against the recorded ones (the counterpart of HTTP's `If-Range` and FTP's check
//! before each `RETR`), reads with several requests in flight (in offset order: a short
//! answer gets its rest asked for next), and checks the attributes again once the range
//! is done, so bytes of a file replaced meanwhile are noticed.

use super::SftpTarget;
use super::conn::{Attrs, Conn, FileHandle};
use crate::engine::EngineError;
use crate::fsutil::write_at;
use crate::http::probe::timed;
use crate::http::worker::{LIMITED_SLICE, SegmentShared, WorkerCtx, WorkerExit, backoff};
use crate::limiter::TokenBucket;
use crate::meter::SpeedMeter;
use futures_util::StreamExt;
use futures_util::stream::FuturesOrdered;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

/// Read requests in flight per connection (as OpenSSH's `sftp`), and while a rate
/// limit applies.
const DEPTH: usize = 32;
const LIMITED_DEPTH: usize = 2;

enum Attempt {
    Exit(WorkerExit),
    Retry { progress: bool, why: String },
}

/// Downloads `seg` (starting at `start`) from `target`. `single`: one stream of the
/// whole file from offset 0 (no known size); each attempt then starts over.
pub(crate) async fn run(
    ctx: &WorkerCtx,
    target: &SftpTarget,
    start: u64,
    seg: Arc<SegmentShared>,
    single: bool,
) -> WorkerExit {
    let mut meter = SpeedMeter::new();
    let mut attempt: u32 = 0;
    let mut furthest = 0u64;
    loop {
        if ctx.cancel.is_cancelled() {
            return WorkerExit::Cancelled;
        }
        if single {
            seg.done.store(0, Ordering::Release);
            seg.bounds.lock().unwrap_or_else(|e| e.into_inner()).claimed = 0;
        } else if start + seg.done.load(Ordering::Acquire) >= seg.end() {
            return WorkerExit::Done;
        }
        let result = attempt_once(ctx, target, start, &seg, single, &mut meter).await;
        seg.active.store(false, Ordering::Relaxed);
        seg.bps.store(0, Ordering::Relaxed);
        let (progress, why) = match result {
            Attempt::Exit(e) => return e,
            Attempt::Retry { progress, why } => (progress, why),
        };
        if ctx.failover {
            return WorkerExit::Retryable { why, progress };
        }
        let progressed = if single {
            let reached = seg.done.load(Ordering::Acquire);
            let further = reached > furthest;
            furthest = furthest.max(reached);
            further
        } else {
            progress
        };
        if progressed {
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

fn failed(e: EngineError, progress: bool) -> Attempt {
    if e.is_transient() {
        Attempt::Retry {
            progress,
            why: e.to_string(),
        }
    } else {
        Attempt::Exit(WorkerExit::Refused(e.to_string()))
    }
}

/// Whether the file is still the one being downloaded: its size and modification time
/// as recorded (a size the server does not tell is not compared).
fn unchanged(ctx: &WorkerCtx, target: &SftpTarget, a: &Attrs) -> bool {
    if let (Some(total), Some(size)) = (ctx.expected_total, a.size)
        && size != total
    {
        return false;
    }
    // A location that told its modification time and no longer does is not trusted.
    match (&target.mtime, super::mtime_validator(a.mtime)) {
        (Some(recorded), Some(now)) => *recorded == now,
        (Some(_), None) => false,
        (None, _) => true,
    }
}

async fn attempt_once(
    ctx: &WorkerCtx,
    target: &SftpTarget,
    start: u64,
    seg: &SegmentShared,
    single: bool,
    meter: &mut SpeedMeter,
) -> Attempt {
    let site = &target.site;
    let path = match site.endpoint() {
        Ok(ep) => ep.path.clone(),
        Err(e) => return Attempt::Exit(WorkerExit::Refused(e.to_string())),
    };
    let mut lease = tokio::select! {
        _ = ctx.cancel.cancelled() => return Attempt::Exit(WorkerExit::Cancelled),
        l = site.conn(&ctx.cfg) => match l {
            Ok(l) => l,
            Err(e) => return failed(e, false),
        },
    };
    seg.active.store(true, Ordering::Relaxed);
    let conn = lease.get();
    let opened = tokio::select! {
        _ = ctx.cancel.cancelled() => return Attempt::Exit(WorkerExit::Cancelled),
        r = async {
            let h = conn.open_file(&path, &ctx.cfg).await?;
            match conn.fstat(&h, &path, &ctx.cfg).await {
                Ok(a) => Ok((h, a)),
                Err(e) => {
                    conn.close_file(h, &ctx.cfg).await;
                    Err(e)
                }
            }
        } => r,
    };
    let (h, attrs) = match opened {
        Ok(x) => x,
        Err(e) => return failed(e, false),
    };
    // Checked before every transfer: bytes of another file must never be mixed in.
    if !unchanged(ctx, target, &attrs) {
        conn.close_file(h, &ctx.cfg).await;
        lease.keep();
        return Attempt::Exit(WorkerExit::RemoteChanged);
    }
    let result = read_into(ctx, conn, &h, &path, start, seg, single, meter).await;
    let result = match result {
        // The range is done: the file must still be the same.
        Attempt::Exit(WorkerExit::Done) => {
            match timed(
                Duration::from_millis(ctx.cfg.read_timeout_ms),
                conn.fstat(&h, &path, &ctx.cfg),
            )
            .await
            {
                Ok(Ok(a)) if !unchanged(ctx, target, &a) => {
                    Attempt::Exit(WorkerExit::RemoteChanged)
                }
                // The bytes are here; a lost connection only loses the check.
                _ => Attempt::Exit(WorkerExit::Done),
            }
        }
        other => other,
    };
    match &result {
        Attempt::Exit(WorkerExit::Cancelled) => {}
        Attempt::Exit(_) => {
            conn.close_file(h, &ctx.cfg).await;
            lease.keep();
        }
        // The connection may be in trouble: not reused.
        Attempt::Retry { .. } => {}
    }
    result
}

type Read<'a> =
    Pin<Box<dyn Future<Output = (u64, u32, Result<Option<Vec<u8>>, EngineError>)> + Send + 'a>>;

fn request<'a>(conn: &'a Conn, h: &'a FileHandle, path: &'a str, off: u64, len: u32) -> Read<'a> {
    Box::pin(async move { (off, len, conn.read(h, path, off, len).await) })
}

/// Reads the range into the segment until it has its bytes (or, `single`, the file
/// ends).
#[allow(clippy::too_many_arguments)]
async fn read_into(
    ctx: &WorkerCtx,
    conn: &Conn,
    h: &FileHandle,
    path: &str,
    start: u64,
    seg: &SegmentShared,
    single: bool,
    meter: &mut SpeedMeter,
) -> Attempt {
    let read_timeout = Duration::from_millis(ctx.cfg.read_timeout_ms);
    let mut inflight: FuturesOrdered<Read<'_>> = FuturesOrdered::new();
    let mut next = start + seg.done.load(Ordering::Acquire);
    let mut eof = false;
    let mut progress = false;
    loop {
        let cursor = start + seg.done.load(Ordering::Acquire);
        let end = seg.end();
        if cursor >= end {
            return Attempt::Exit(WorkerExit::Done);
        }
        let limited = ctx.limiters.iter().any(TokenBucket::is_limited);
        let (depth, size) = if limited {
            (LIMITED_DEPTH, (LIMITED_SLICE as u32).min(conn.read_len))
        } else {
            (DEPTH, conn.read_len)
        };
        while !eof && inflight.len() < depth && next < end {
            let len = u32::try_from((end - next).min(u64::from(size))).unwrap_or(size);
            inflight.push_back(request(conn, h, path, next, len));
            next += u64::from(len);
        }
        let Some((off, len, got)) = (tokio::select! {
            _ = ctx.cancel.cancelled() => return Attempt::Exit(WorkerExit::Cancelled),
            r = timed(read_timeout, inflight.next()) => match r {
                Ok(r) => r,
                Err(e) => return Attempt::Retry { progress, why: e.to_string() },
            },
        }) else {
            // Nothing in flight and nothing left to ask for: the file ended.
            return ended(ctx, start, seg, single);
        };
        debug_assert_eq!(off, cursor);
        let data = match got {
            Ok(Some(d)) if !d.is_empty() => d,
            Ok(_) => {
                // End of file at `off`: later requests are past it too.
                eof = true;
                inflight = FuturesOrdered::new();
                next = off;
                continue;
            }
            Err(e) => return failed(e, progress),
        };
        let n = data.len();
        if !ctx.acquire(n as u64).await {
            return Attempt::Exit(WorkerExit::Cancelled);
        }
        // Claim under the lock: `end` may have shrunk while waiting for the limiters.
        let claimed = {
            let mut b = seg.bounds.lock().unwrap_or_else(|e| e.into_inner());
            let c = (n as u64).min(b.end.saturating_sub(off));
            if c > 0 {
                b.claimed = off + c;
            }
            c
        };
        if claimed == 0 {
            // Stolen meanwhile: this segment is complete.
            return Attempt::Exit(WorkerExit::Done);
        }
        let file = ctx.file.clone();
        let written =
            tokio::task::spawn_blocking(move || write_at(&file, &data[..claimed as usize], off))
                .await;
        match written {
            Ok(Ok(())) => {}
            Ok(Err(e)) => return Attempt::Exit(WorkerExit::Fatal(e.to_string())),
            Err(e) => return Attempt::Exit(WorkerExit::Fatal(e.to_string())),
        }
        progress = true;
        seg.done.fetch_add(claimed, Ordering::Release);
        ctx.got.fetch_add(claimed, Ordering::Relaxed);
        let now = Instant::now();
        meter.record(claimed, now);
        seg.bps.store(meter.bps(now), Ordering::Relaxed);
        if (n as u64) < u64::from(len) && claimed == n as u64 {
            // A short answer: its rest is asked for before anything after it.
            let rest = len - n as u32;
            inflight.push_front(request(conn, h, path, off + n as u64, rest));
        }
    }
}

/// The file ended at the cursor before the range did.
fn ended(ctx: &WorkerCtx, start: u64, seg: &SegmentShared, single: bool) -> Attempt {
    let reached = start + seg.done.load(Ordering::Acquire);
    if single && ctx.expected_total.is_none_or(|t| t == reached) {
        return Attempt::Exit(WorkerExit::Done);
    }
    // Shorter than recorded: the file shrank.
    Attempt::Exit(WorkerExit::RemoteChanged)
}
