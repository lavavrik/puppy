mod support;

use puppy_core::config::HttpConfig;
use puppy_core::engine::{AddOptions, EngineEvent};
use puppy_core::http::download::HttpDownload;
use puppy_core::http::probe::build_client;
use puppy_core::http::resume::{HttpResumeState, part_path, sidecar_path};
use puppy_core::limiter::TokenBucket;
use puppy_core::types::{
    AddRequest, AttentionReason, DownloadState, DownloadStatus, EngineDetail, RequestContext,
    SegmentView, Source,
};
use std::path::Path;
use std::time::{Duration, Instant};
use support::{ServerCfg, TestServer, body_byte};
use tokio::sync::broadcast;

const KIB: u64 = 1024;
const MIB: u64 = 1024 * 1024;
const TIMEOUT: Duration = Duration::from_secs(20);

fn cfg(dir: &Path) -> HttpConfig {
    HttpConfig {
        backoff_base_ms: 10,
        backoff_max_ms: 100,
        flush_interval_ms: 100,
        default_dir: dir.to_path_buf(),
        ..HttpConfig::default()
    }
}

fn request(s: &TestServer, dir: &Path) -> AddRequest {
    AddRequest {
        source: Source::Http {
            url: s.url("/file.bin"),
        },
        output_dir: Some(dir.to_path_buf()),
        filename: None,
        context: RequestContext::default(),
    }
}

async fn server(f: impl FnOnce(&mut ServerCfg)) -> TestServer {
    let mut c = ServerCfg::default();
    f(&mut c);
    TestServer::start(c).await
}

async fn start(
    s: &TestServer,
    dir: &Path,
    cfg: HttpConfig,
) -> (HttpDownload, puppy_core::engine::Added) {
    let (tx, _) = broadcast::channel(256);
    start_with_events(s, dir, cfg, tx).await
}

async fn start_with_events(
    s: &TestServer,
    dir: &Path,
    cfg: HttpConfig,
    tx: broadcast::Sender<EngineEvent>,
) -> (HttpDownload, puppy_core::engine::Added) {
    let client = build_client(&cfg).unwrap();
    HttpDownload::start_fresh(
        1,
        request(s, dir),
        AddOptions::default(),
        client,
        cfg,
        TokenBucket::new(None),
        tx,
    )
    .await
    .unwrap()
}

/// Polls `status()` until `pred` holds; panics with the last status on timeout.
async fn wait_status(
    dl: &HttpDownload,
    pred: impl Fn(&DownloadStatus) -> bool,
    timeout: Duration,
) -> DownloadStatus {
    let deadline = Instant::now() + timeout;
    loop {
        let st = dl.status();
        if pred(&st) {
            return st;
        }
        assert!(Instant::now() < deadline, "timed out; last status {st:?}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

async fn wait_state(
    dl: &HttpDownload,
    pred: impl Fn(&DownloadState) -> bool,
    timeout: Duration,
) -> DownloadStatus {
    wait_status(dl, |s| pred(&s.state), timeout).await
}

fn is_completed(s: &DownloadState) -> bool {
    matches!(s, DownloadState::Completed)
}

fn assert_file(path: &Path, len: u64, version: u32) {
    let data = std::fs::read(path).unwrap();
    assert_eq!(data.len() as u64, len, "length of {}", path.display());
    for (i, b) in data.iter().enumerate() {
        assert_eq!(*b, body_byte(i as u64, version), "byte {i}");
    }
}

fn assert_no_leftovers(final_path: &Path) {
    assert!(!part_path(final_path).exists(), ".part left behind");
    assert!(!sidecar_path(final_path).exists(), "sidecar left behind");
}

fn segments(dl: &HttpDownload) -> (Vec<SegmentView>, bool) {
    match dl.detail() {
        EngineDetail::Http {
            segments,
            resumable,
            ..
        } => (segments, resumable),
        other => panic!("unexpected detail {other:?}"),
    }
}

#[tokio::test]
async fn downloads_multi_segment() {
    let s = server(|_| {}).await;
    let dir = tempfile::tempdir().unwrap();
    let mut c = cfg(dir.path());
    c.connections = 8;
    let (tx, mut rx) = broadcast::channel(256);
    let (dl, added) = start_with_events(&s, dir.path(), c, tx).await;
    assert_eq!(added.name, "file.bin");
    assert_eq!(added.path, dir.path().join("file.bin"));
    assert_eq!(added.total_bytes, Some(4 * MIB));
    let st = wait_state(&dl, is_completed, TIMEOUT).await;
    assert_eq!(st.done_bytes, 4 * MIB);
    assert_file(&added.path, 4 * MIB, 0);
    assert_no_leftovers(&added.path);
    assert!(s.range_requests() >= 4, "{}", s.range_requests());
    let (segs, resumable) = segments(&dl);
    assert!(resumable);
    assert_eq!(segs.len(), 4);
    // Events: Starting ... Active ... Completed, then Completed{id}.
    let mut seen = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        seen.push(ev);
    }
    assert!(matches!(
        seen.first(),
        Some(EngineEvent::StateChanged {
            id: 1,
            state: DownloadState::Starting
        })
    ));
    assert!(seen.iter().any(|e| matches!(
        e,
        EngineEvent::StateChanged {
            state: DownloadState::Active,
            ..
        }
    )));
    assert!(matches!(
        seen.last(),
        Some(EngineEvent::Completed { id: 1 })
    ));
}

#[tokio::test]
async fn downloads_no_ranges_single_stream() {
    let s = server(|c| c.ranges = false).await;
    let dir = tempfile::tempdir().unwrap();
    let (dl, added) = start(&s, dir.path(), cfg(dir.path())).await;
    wait_state(&dl, is_completed, TIMEOUT).await;
    let (segs, resumable) = segments(&dl);
    assert_eq!(segs.len(), 1);
    assert!(!resumable);
    assert_file(&added.path, 4 * MIB, 0);
    assert_no_leftovers(&added.path);
}

#[tokio::test]
async fn ranges_lied_falls_back_to_single() {
    let s = server(|c| c.ranges_lie = true).await;
    let dir = tempfile::tempdir().unwrap();
    let (dl, added) = start(&s, dir.path(), cfg(dir.path())).await;
    wait_state(&dl, is_completed, TIMEOUT).await;
    let (segs, resumable) = segments(&dl);
    assert_eq!(segs.len(), 1);
    assert!(!resumable);
    assert_file(&added.path, 4 * MIB, 0);
    assert_no_leftovers(&added.path);
}

#[tokio::test]
async fn unknown_length_single_stream() {
    let s = server(|c| {
        c.len = MIB;
        c.content_length = false;
        c.ranges = false;
        c.bytes_per_sec = Some(4 * MIB);
    })
    .await;
    let dir = tempfile::tempdir().unwrap();
    let (dl, added) = start(&s, dir.path(), cfg(dir.path())).await;
    assert_eq!(added.total_bytes, None);
    let st = wait_state(
        &dl,
        |s| matches!(s, DownloadState::Active | DownloadState::Completed),
        TIMEOUT,
    )
    .await;
    assert_eq!(st.total_bytes, None);
    assert_eq!(st.eta_secs, None);
    wait_state(&dl, is_completed, TIMEOUT).await;
    assert_file(&added.path, MIB, 0);
    assert_no_leftovers(&added.path);
}

#[tokio::test]
async fn pause_and_resume_same_session() {
    let s = server(|c| c.bytes_per_sec = Some(512 * KIB)).await;
    let dir = tempfile::tempdir().unwrap();
    let (dl, added) = start(&s, dir.path(), cfg(dir.path())).await;
    wait_status(&dl, |s| s.done_bytes >= MIB, TIMEOUT).await;
    dl.pause().await;
    let st = dl.status();
    assert!(matches!(st.state, DownloadState::Paused), "{st:?}");
    assert!(st.done_bytes < 4 * MIB);
    assert_eq!(st.down_bps, 0);
    assert!(sidecar_path(&added.path).exists());
    assert!(part_path(&added.path).exists());
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        dl.status().done_bytes,
        st.done_bytes,
        "progress after pause"
    );
    // Pausing again is a no-op.
    dl.pause().await;
    assert!(matches!(dl.status().state, DownloadState::Paused));

    dl.resume().await.unwrap();
    wait_state(&dl, is_completed, TIMEOUT).await;
    assert_file(&added.path, 4 * MIB, 0);
    assert_no_leftovers(&added.path);
}

#[tokio::test]
async fn pause_immediately_after_start_is_safe() {
    let s = server(|c| {
        c.len = MIB;
        c.bytes_per_sec = Some(MIB);
    })
    .await;
    let dir = tempfile::tempdir().unwrap();
    let (dl, added) = start(&s, dir.path(), cfg(dir.path())).await;
    dl.pause().await;
    assert!(matches!(dl.status().state, DownloadState::Paused));
    dl.resume().await.unwrap();
    wait_state(&dl, is_completed, TIMEOUT).await;
    assert_file(&added.path, MIB, 0);
}

#[tokio::test]
async fn per_download_limit() {
    let s = server(|c| {
        c.len = MIB;
        c.bytes_per_sec = Some(2 * MIB);
    })
    .await;
    let dir = tempfile::tempdir().unwrap();
    let mut c = cfg(dir.path());
    c.min_segment_bytes = 64 * KIB;
    let t = Instant::now();
    let (dl, added) = start(&s, dir.path(), c).await;
    dl.set_limit(Some(256 * KIB));
    let st = wait_status(
        &dl,
        |s| s.done_bytes >= 256 * KIB && s.down_bps > 0,
        TIMEOUT,
    )
    .await;
    assert!(st.down_bps < 512 * KIB, "bps {}", st.down_bps);
    assert!(st.eta_secs.is_some());
    wait_state(&dl, is_completed, TIMEOUT).await;
    let e = t.elapsed();
    assert!(e >= Duration::from_secs(3), "took only {e:?}");
    assert_file(&added.path, MIB, 0);
}

#[tokio::test]
async fn remote_change_mid_download_needs_attention() {
    // One connection; the server drops it once at 768 KiB, forcing a new request
    // (with If-Range) after the version has changed.
    let s = server(|c| {
        c.len = 2 * MIB;
        c.bytes_per_sec = Some(MIB);
        c.fail_after_bytes_once = Some(768 * KIB);
    })
    .await;
    let dir = tempfile::tempdir().unwrap();
    let mut c = cfg(dir.path());
    c.connections = 1;
    let (dl, added) = start(&s, dir.path(), c).await;
    let st = wait_status(&dl, |s| s.done_bytes >= 128 * KIB, TIMEOUT).await;
    assert!(st.done_bytes < 768 * KIB, "too late to change: {st:?}");
    s.set_version(1);
    let st = wait_state(
        &dl,
        |s| matches!(s, DownloadState::NeedsAttention { .. }),
        TIMEOUT,
    )
    .await;
    assert!(matches!(
        st.state,
        DownloadState::NeedsAttention {
            reason: AttentionReason::RemoteChanged
        }
    ));
    assert!(!added.path.exists(), "final file must not be created");
    assert!(part_path(&added.path).exists());
    assert!(sidecar_path(&added.path).exists());
    // Needs a decision first; resume is refused.
    assert!(dl.resume().await.is_err());
}

#[tokio::test]
async fn filename_collision() {
    let s = server(|c| c.len = 64 * KIB).await;
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("file.bin"), b"existing").unwrap();
    let (dl, added) = start(&s, dir.path(), cfg(dir.path())).await;
    assert_eq!(added.path, dir.path().join("file (1).bin"));
    assert_eq!(added.name, "file (1).bin");
    wait_state(&dl, is_completed, TIMEOUT).await;
    assert_file(&added.path, 64 * KIB, 0);
    assert_eq!(
        std::fs::read(dir.path().join("file.bin")).unwrap(),
        b"existing"
    );
}

#[tokio::test]
async fn work_stealing_splits_lagging_segment() {
    // One connection drops early and lags behind; finished workers steal its tail.
    let s = server(|c| {
        c.len = 2 * MIB;
        c.bytes_per_sec = Some(MIB);
        c.fail_after_bytes_once = Some(16 * KIB);
    })
    .await;
    let dir = tempfile::tempdir().unwrap();
    let mut c = cfg(dir.path());
    c.connections = 4;
    c.min_segment_bytes = 16 * KIB;
    let (dl, added) = start(&s, dir.path(), c).await;
    wait_state(&dl, is_completed, TIMEOUT).await;
    let (segs, _) = segments(&dl);
    assert!(segs.len() > 4, "no steal happened: {segs:?}");
    assert_eq!(segs[0].start, 0);
    assert_eq!(segs.last().unwrap().end, 2 * MIB);
    for w in segs.windows(2) {
        assert_eq!(w[0].end, w[1].start, "gap or overlap: {segs:?}");
    }
    for seg in &segs {
        assert_eq!(seg.done, seg.end - seg.start);
    }
    assert_file(&added.path, 2 * MIB, 0);
    assert_no_leftovers(&added.path);
}

#[tokio::test]
async fn cancel_deletes_files() {
    let s = server(|c| c.bytes_per_sec = Some(256 * KIB)).await;
    let dir = tempfile::tempdir().unwrap();
    let (dl, added) = start(&s, dir.path(), cfg(dir.path())).await;
    wait_status(&dl, |s| s.done_bytes > 0, TIMEOUT).await;
    dl.cancel(true).await;
    assert!(!added.path.exists());
    assert_no_leftovers(&added.path);
    assert!(dl.resume().await.is_err());
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
}

#[tokio::test]
async fn zero_length_file() {
    let s = server(|c| c.len = 0).await;
    let dir = tempfile::tempdir().unwrap();
    let (dl, added) = start(&s, dir.path(), cfg(dir.path())).await;
    assert_eq!(added.total_bytes, Some(0));
    wait_state(&dl, is_completed, TIMEOUT).await;
    assert_eq!(std::fs::read(&added.path).unwrap().len(), 0);
    assert_no_leftovers(&added.path);
}

#[tokio::test]
async fn head_zero_length_flaky_get_downloads_full_file() {
    let s = server(|c| {
        c.len = MIB;
        c.head_zero_length = true;
        c.get_503_once = true;
    })
    .await;
    let dir = tempfile::tempdir().unwrap();
    let (dl, added) = start(&s, dir.path(), cfg(dir.path())).await;
    assert_eq!(added.total_bytes, None);
    wait_state(&dl, is_completed, TIMEOUT).await;
    let (segs, resumable) = segments(&dl);
    assert_eq!(segs.len(), 1);
    assert!(!resumable);
    assert_file(&added.path, MIB, 0);
    assert_no_leftovers(&added.path);
}

async fn sidecar_single_stream_flag(f: impl FnOnce(&mut ServerCfg)) -> bool {
    let s = server(|c| {
        c.bytes_per_sec = Some(256 * KIB);
        f(c);
    })
    .await;
    let dir = tempfile::tempdir().unwrap();
    let (dl, added) = start(&s, dir.path(), cfg(dir.path())).await;
    wait_status(&dl, |s| s.done_bytes > 0, TIMEOUT).await;
    dl.pause().await;
    let state = HttpResumeState::load(&added.path).await.unwrap();
    dl.cancel(true).await;
    state.single_stream
}

#[tokio::test]
async fn sidecar_marks_single_stream() {
    assert!(!sidecar_single_stream_flag(|_| {}).await);
    assert!(sidecar_single_stream_flag(|c| c.ranges = false).await);
    assert!(
        sidecar_single_stream_flag(|c| {
            c.ranges = false;
            c.content_length = false;
        })
        .await
    );
    assert!(sidecar_single_stream_flag(|c| c.ranges_lie = true).await);
}

#[tokio::test]
async fn low_limit_does_not_reconnect() {
    // Waiting on the limiter for longer than the read timeout must not count as a stalled
    // connection: the body is already arriving, only we are slow to take it.
    // 48 KiB arrives in three 16 KiB chunks; at 8 KiB/s each limiter wait (2 s) exceeds
    // the read timeout.
    let s = server(|c| c.len = 48 * KIB).await;
    let dir = tempfile::tempdir().unwrap();
    let mut c = cfg(dir.path());
    c.connections = 1;
    c.read_timeout_ms = 1000;
    let (tx, _) = broadcast::channel(256);
    let client = build_client(&c).unwrap();
    let (dl, added) = HttpDownload::start_fresh(
        1,
        request(&s, dir.path()),
        AddOptions::default(),
        client,
        c,
        TokenBucket::new(Some(8 * KIB)),
        tx,
    )
    .await
    .unwrap();
    wait_state(&dl, is_completed, TIMEOUT).await;
    assert_file(&added.path, 48 * KIB, 0);
    // The HEAD probe plus a single ranged GET.
    let seen = s.requests_seen().len();
    assert!(seen <= 2, "reconnected: {seen} requests");
    assert_eq!(s.range_requests(), 1);
}

async fn expiring_download(f: impl FnOnce(&mut ServerCfg)) {
    // One connection; the server drops it once at 768 KiB, after the signed URL it was
    // redirected to has expired, so the reconnect gets 403.
    let s = server(|c| {
        c.len = 2 * MIB;
        c.bytes_per_sec = Some(MIB);
        c.fail_after_bytes_once = Some(768 * KIB);
        f(c);
    })
    .await;
    let dir = tempfile::tempdir().unwrap();
    let mut c = cfg(dir.path());
    c.connections = 1;
    let mut req = request(&s, dir.path());
    req.source = Source::Http {
        url: s.url("/expiring"),
    };
    let (tx, _) = broadcast::channel(256);
    let client = build_client(&c).unwrap();
    let (dl, added) = HttpDownload::start_fresh(
        1,
        req,
        AddOptions::default(),
        client,
        c,
        TokenBucket::new(None),
        tx,
    )
    .await
    .unwrap();
    let st = wait_status(&dl, |s| s.done_bytes >= 128 * KIB, TIMEOUT).await;
    assert!(st.done_bytes < 768 * KIB, "too late to expire: {st:?}");
    s.rotate_signed();
    wait_state(&dl, is_completed, TIMEOUT).await;
    assert_file(&added.path, 2 * MIB, 0);
    assert_no_leftovers(&added.path);
    let paths = s.paths_seen();
    assert!(paths.iter().any(|p| p == "/signed/1"), "{paths:?}");
}

#[tokio::test]
async fn expired_redirect_reresolves_mid_download() {
    expiring_download(|_| {}).await;
}

#[tokio::test]
async fn expired_redirect_reresolves_single_stream() {
    expiring_download(|c| c.ranges = false).await;
}

#[tokio::test]
async fn expired_redirect_reresolves_without_etag() {
    // No strong validator: the new URL is accepted after a spot check of the bytes so far.
    expiring_download(|c| c.etag = false).await;
}
