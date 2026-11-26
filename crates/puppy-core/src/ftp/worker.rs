//! FTP segment worker: downloads one byte range (`REST` + `RETR`) over one session, with
//! the same contract as the HTTP worker ([`crate::http::worker::run_segment`]): it
//! writes into the shared `.part` through [`SegmentShared`] (so work stealing applies),
//! honours both rate limiters and retries with the usual backoff.
//!
//! `RETR` sends the file to its end. A segment that ends before the file does closes its
//! session once it has its bytes (an aborted transfer leaves a server in no shape to
//! reuse); one that ends with the file reads the end of the data, takes the server's
//! "transfer complete" and gives the session back for the next segment.

use super::session::{RetrError, Stream};
use super::{FtpTarget, Lease};
use crate::engine::EngineError;
use crate::fsutil::write_at;
use crate::http::probe::timed;
use crate::http::worker::{SegmentShared, WorkerCtx, WorkerExit, backoff};
use crate::meter::SpeedMeter;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};
use tokio::io::AsyncReadExt;

const BUF: usize = 64 * 1024;

enum Attempt {
    Exit(WorkerExit),
    Retry { progress: bool, why: String },
}

/// Downloads `seg` (starting at `start`) from `target`. `single`: one stream of the
/// whole file from offset 0 (no restart support or no known size); each attempt then
/// starts over.
pub(crate) async fn run(
    ctx: &WorkerCtx,
    target: &FtpTarget,
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
            restart(&seg);
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

/// A single stream starts over: nothing written counts.
fn restart(seg: &SegmentShared) {
    seg.done.store(0, Ordering::Release);
    seg.bounds.lock().unwrap_or_else(|e| e.into_inner()).claimed = 0;
}

/// An error opening a session or starting the transfer, as an attempt's outcome.
fn failed(e: EngineError) -> Attempt {
    if e.is_transient() {
        Attempt::Retry {
            progress: false,
            why: e.to_string(),
        }
    } else {
        Attempt::Exit(WorkerExit::Refused(e.to_string()))
    }
}

async fn attempt_once(
    ctx: &WorkerCtx,
    target: &FtpTarget,
    start: u64,
    seg: &SegmentShared,
    single: bool,
    meter: &mut SpeedMeter,
) -> Attempt {
    let site = &target.site;
    let ep = match site.endpoint() {
        Ok(ep) => ep.clone(),
        Err(e) => return Attempt::Exit(WorkerExit::Refused(e.to_string())),
    };
    let mut lease = tokio::select! {
        _ = ctx.cancel.cancelled() => return Attempt::Exit(WorkerExit::Cancelled),
        l = site.session(&ctx.cfg) => match l {
            Ok(l) => l,
            Err(e) => return failed(e),
        },
    };
    seg.active.store(true, Ordering::Relaxed);
    // Checked before every transfer, as HTTP requests carry `If-Range`: bytes of another
    // file must never be mixed in.
    let checked = tokio::select! {
        _ = ctx.cancel.cancelled() => return Attempt::Exit(WorkerExit::Cancelled),
        r = check(ctx, target, &ep.path, &mut lease) => r,
    };
    match checked {
        Ok(true) => {}
        Ok(false) => return Attempt::Exit(WorkerExit::RemoteChanged),
        Err(e) => return failed(e),
    }
    let cursor = if single {
        0
    } else {
        start + seg.done.load(Ordering::Acquire)
    };
    let opened = tokio::select! {
        _ = ctx.cancel.cancelled() => return Attempt::Exit(WorkerExit::Cancelled),
        r = lease.get().retr(&ep.path, cursor, site.pasv(), &ctx.cfg) => r,
    };
    let stream = match opened {
        Ok((stream, switched)) => {
            if let Some(pasv) = switched {
                site.set_pasv(pasv);
            }
            stream
        }
        Err(RetrError::RestRefused) => return Attempt::Exit(WorkerExit::RangeUnsupported),
        // The file is gone (550) or the like: this location cannot serve it.
        Err(RetrError::Other(e)) => return failed(e),
    };
    read_into(ctx, start, seg, single, meter, stream, lease).await
}

/// Whether the file is still the one being downloaded: its size and modification time
/// as recorded (what the server does not tell is not compared).
async fn check(
    ctx: &WorkerCtx,
    target: &FtpTarget,
    path: &str,
    lease: &mut Lease,
) -> Result<bool, EngineError> {
    let s = lease.get();
    if let Some(total) = ctx.expected_total
        && let Some(size) = s.size(path, &ctx.cfg).await?
        && size != total
    {
        return Ok(false);
    }
    if let Some(recorded) = &target.mdtm {
        // A server that told the modification time and no longer does cannot vouch for
        // the file any more.
        match s.mdtm(path, &ctx.cfg).await? {
            Some(now) if &now == recorded => {}
            _ => return Ok(false),
        }
    }
    Ok(true)
}

/// Reads the transfer into the segment until it has its bytes.
async fn read_into(
    ctx: &WorkerCtx,
    start: u64,
    seg: &SegmentShared,
    single: bool,
    meter: &mut SpeedMeter,
    mut stream: Stream,
    lease: Lease,
) -> Attempt {
    let read_timeout = Duration::from_millis(ctx.cfg.read_timeout_ms);
    let total = ctx.expected_total;
    let mut buf = vec![0u8; BUF];
    let mut progress = false;
    loop {
        let cursor = start + seg.done.load(Ordering::Acquire);
        let end = seg.end();
        if cursor >= end {
            // Ends with the file: its end must follow, then the session can be reused.
            if single || total == Some(end) {
                return finish_at_end(ctx, stream, lease, &mut buf, read_timeout).await;
            }
            return close(ctx, stream, lease).await;
        }
        let want = ctx
            .slice_len(BUF)
            .min(usize::try_from(end - cursor).unwrap_or(BUF));
        let n = tokio::select! {
            _ = ctx.cancel.cancelled() => return Attempt::Exit(WorkerExit::Cancelled),
            r = timed(read_timeout, stream.read(&mut buf[..want])) => match r {
                Ok(Ok(n)) => n,
                Ok(Err(e)) => return Attempt::Retry { progress, why: e.to_string() },
                Err(e) => return Attempt::Retry { progress, why: e.to_string() },
            },
        };
        if n == 0 {
            return ended(ctx, start, seg, single, stream, lease, progress).await;
        }
        if !ctx.acquire(n as u64).await {
            return Attempt::Exit(WorkerExit::Cancelled);
        }
        // Claim under the lock: `end` may have shrunk while waiting for the limiters.
        let n = {
            let mut b = seg.bounds.lock().unwrap_or_else(|e| e.into_inner());
            let n = (n as u64).min(b.end.saturating_sub(cursor));
            if n > 0 {
                b.claimed = cursor + n;
            }
            n
        };
        if n == 0 {
            // Stolen meanwhile: this segment is complete.
            return close(ctx, stream, lease).await;
        }
        let file = ctx.file.clone();
        let len = n as usize;
        let written = tokio::task::spawn_blocking(move || {
            let r = write_at(&file, &buf[..len], cursor);
            (buf, r)
        })
        .await;
        buf = match written {
            Ok((b, Ok(()))) => b,
            Ok((_, Err(e))) => return Attempt::Exit(WorkerExit::Fatal(e.to_string())),
            Err(e) => return Attempt::Exit(WorkerExit::Fatal(e.to_string())),
        };
        progress = true;
        seg.done.fetch_add(n, Ordering::Release);
        ctx.got.fetch_add(n, Ordering::Relaxed);
        let now = Instant::now();
        meter.record(n, now);
        seg.bps.store(meter.bps(now), Ordering::Relaxed);
    }
}

/// The segment has its bytes before the file ends: the transfer is cut short and the
/// session closed.
async fn close(ctx: &WorkerCtx, stream: Stream, lease: Lease) -> Attempt {
    drop(stream);
    lease.close(&ctx.cfg).await;
    Attempt::Exit(WorkerExit::Done)
}

/// The data ended (EOF) before the segment did, or a single stream ended.
async fn ended(
    ctx: &WorkerCtx,
    start: u64,
    seg: &SegmentShared,
    single: bool,
    stream: Stream,
    lease: Lease,
    progress: bool,
) -> Attempt {
    let read_timeout = Duration::from_millis(ctx.cfg.read_timeout_ms);
    let reply = timed(read_timeout, stream.finish()).await;
    let complete = matches!(reply, Ok(Ok(())));
    let reached = start + seg.done.load(Ordering::Acquire);
    if complete {
        // The server sent the whole file.
        if single && ctx.expected_total.is_none_or(|t| t == reached) {
            lease.keep();
            return Attempt::Exit(WorkerExit::Done);
        }
        // Shorter than recorded: the file shrank.
        return Attempt::Exit(WorkerExit::RemoteChanged);
    }
    let why = match reply {
        Ok(Err(e)) => super::ftp_err(e).to_string(),
        Err(e) => e.to_string(),
        Ok(Ok(())) => unreachable!("handled above"),
    };
    Attempt::Retry {
        progress,
        why: format!("transfer ended early: {why}"),
    }
}

/// A segment ending with the file has its bytes: the data must end here (more means the
/// file grew), then the server confirms the transfer and the session is reused.
async fn finish_at_end(
    ctx: &WorkerCtx,
    mut stream: Stream,
    lease: Lease,
    buf: &mut [u8],
    read_timeout: Duration,
) -> Attempt {
    let more = tokio::select! {
        _ = ctx.cancel.cancelled() => return Attempt::Exit(WorkerExit::Cancelled),
        r = timed(read_timeout, stream.read(&mut buf[..1])) => r,
    };
    match more {
        Ok(Ok(0)) => {}
        Ok(Ok(_)) => return Attempt::Exit(WorkerExit::RemoteChanged),
        // The bytes are all here; only the session is lost.
        _ => return Attempt::Exit(WorkerExit::Done),
    }
    if let Ok(Ok(())) = timed(read_timeout, stream.finish()).await {
        lease.keep();
    }
    Attempt::Exit(WorkerExit::Done)
}
