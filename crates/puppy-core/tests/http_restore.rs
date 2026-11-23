use puppy_core::config::HttpConfig;
use puppy_core::engine::{AddOptions, Added, EngineEvent};
use puppy_core::http::download::HttpDownload;
use puppy_core::http::probe::build_client;
use puppy_core::http::resume::{HttpResumeState, part_path, sidecar_path};
use puppy_core::limiter::TokenBucket;
use puppy_core::types::{
    AddRequest, AttentionReason, DownloadState, DownloadStatus, EngineDetail, RequestContext,
    Source,
};
use puppy_testserver::{ServerCfg, TestServer, body_byte};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use tokio::sync::broadcast;

const KIB: u64 = 1024;
const MIB: u64 = 1024 * 1024;
const LEN: u64 = 4 * MIB;
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
        files: Default::default(),
        seed: None,
    }
}

/// A throttled server so downloads can be interrupted mid-way.
async fn server(f: impl FnOnce(&mut ServerCfg)) -> TestServer {
    let mut c = ServerCfg {
        len: LEN,
        bytes_per_sec: Some(256 * KIB),
        ..ServerCfg::default()
    };
    f(&mut c);
    TestServer::start(c).await
}

async fn start(req: AddRequest, cfg: HttpConfig) -> (HttpDownload, Added) {
    let (tx, _) = broadcast::channel(256);
    let client = build_client(&cfg).unwrap();
    HttpDownload::start_fresh(
        1,
        req,
        AddOptions::default(),
        client,
        cfg,
        TokenBucket::new(None),
        tx,
    )
    .await
    .unwrap()
}

async fn restore(s: &TestServer, final_path: &Path, dir: &Path) -> (HttpDownload, Added) {
    let (tx, _) = broadcast::channel(256);
    restore_with_events(s, final_path, dir, tx).await
}

async fn restore_with_events(
    s: &TestServer,
    final_path: &Path,
    dir: &Path,
    tx: broadcast::Sender<EngineEvent>,
) -> (HttpDownload, Added) {
    restore_with(s, final_path, cfg(dir), tx).await
}

async fn restore_with(
    s: &TestServer,
    final_path: &Path,
    c: HttpConfig,
    tx: broadcast::Sender<EngineEvent>,
) -> (HttpDownload, Added) {
    let dir = final_path.parent().unwrap();
    restore_req(request(s, dir), final_path, c, AddOptions::default(), tx).await
}

async fn restore_req(
    req: AddRequest,
    final_path: &Path,
    c: HttpConfig,
    opts: AddOptions,
    tx: broadcast::Sender<EngineEvent>,
) -> (HttpDownload, Added) {
    let client = build_client(&c).unwrap();
    HttpDownload::restore(
        2,
        req,
        final_path,
        opts,
        client,
        c,
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

async fn wait_completed(dl: &HttpDownload) -> DownloadStatus {
    wait_status(dl, |s| matches!(s.state, DownloadState::Completed), TIMEOUT).await
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

fn attention(dl: &HttpDownload) -> AttentionReason {
    match dl.status().state {
        DownloadState::NeedsAttention { reason } => reason,
        other => panic!("expected NeedsAttention, got {other:?}"),
    }
}

/// Starts a download, pauses it once ≥1 MiB is done, drops it, and lifts the throttle.
/// Returns the final path.
async fn paused_download(s: &TestServer, dir: &Path) -> PathBuf {
    let (dl, added) = start(request(s, dir), cfg(dir)).await;
    let st = wait_status(&dl, |s| s.done_bytes >= MIB, TIMEOUT).await;
    dl.pause().await;
    assert!(st.done_bytes < LEN, "finished before the pause: {st:?}");
    assert!(matches!(dl.status().state, DownloadState::Paused));
    drop(dl);
    s.set_cfg(|c| c.bytes_per_sec = None);
    added.path
}

#[tokio::test]
async fn restore_after_kill_completes_correctly() {
    let s = server(|_| {}).await;
    let dir = tempfile::tempdir().unwrap();
    let req = request(&s, dir.path());
    let c = cfg(dir.path());
    // The first run lives on its own runtime, which is shut down without pausing or
    // flushing: as close to a killed process as a test can get.
    let killed = std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let (dl, added) = rt.block_on(async {
            let (dl, added) = start(req, c).await;
            wait_status(&dl, |s| s.done_bytes >= MIB, TIMEOUT).await;
            (dl, added)
        });
        let st = dl.status();
        rt.shutdown_background();
        drop(dl);
        (added.path, st)
    });
    let (final_path, st) = tokio::task::spawn_blocking(move || killed.join().unwrap())
        .await
        .unwrap();
    assert!(matches!(st.state, DownloadState::Active), "{st:?}");
    assert!(st.done_bytes < LEN, "finished before the kill: {st:?}");
    assert!(part_path(&final_path).exists());
    assert!(sidecar_path(&final_path).exists());
    s.set_cfg(|c| c.bytes_per_sec = None);

    let (tx, mut rx) = broadcast::channel(256);
    let (dl, added) = restore_with_events(&s, &final_path, dir.path(), tx).await;
    assert_eq!(added.path, final_path);
    assert_eq!(added.name, "file.bin");
    assert_eq!(added.total_bytes, Some(LEN));
    let st = wait_completed(&dl).await;
    assert_eq!(st.done_bytes, LEN);
    assert_file(&final_path, LEN, 0);
    assert_no_leftovers(&final_path);
    let mut completed = false;
    while let Ok(ev) = rx.try_recv() {
        completed |= matches!(ev, EngineEvent::Completed { id: 2 });
    }
    assert!(completed, "no Completed event");
}

#[tokio::test]
async fn restore_strong_etag_ok_completes() {
    let s = server(|_| {}).await;
    let dir = tempfile::tempdir().unwrap();
    let final_path = paused_download(&s, dir.path()).await;
    let (dl, _) = restore(&s, &final_path, dir.path()).await;
    wait_completed(&dl).await;
    assert_file(&final_path, LEN, 0);
    assert_no_leftovers(&final_path);
    // Validation used If-Range with the recorded ETag.
    assert!(
        s.requests_seen()
            .iter()
            .any(|h| h.get("if-range").is_some_and(|v| v == "\"v0\""))
    );
}

#[tokio::test]
async fn restore_strong_etag_changed() {
    let s = server(|_| {}).await;
    let dir = tempfile::tempdir().unwrap();
    let final_path = paused_download(&s, dir.path()).await;
    s.set_version(1);
    let (dl, _) = restore(&s, &final_path, dir.path()).await;
    assert!(matches!(attention(&dl), AttentionReason::RemoteChanged));
    assert!(!final_path.exists());
    assert!(part_path(&final_path).exists());
}

#[tokio::test]
async fn restore_no_validator_spot_check_ok() {
    let s = server(|c| {
        c.etag = false;
        c.last_modified = false;
    })
    .await;
    let dir = tempfile::tempdir().unwrap();
    let final_path = paused_download(&s, dir.path()).await;
    let (dl, _) = restore(&s, &final_path, dir.path()).await;
    wait_completed(&dl).await;
    assert_file(&final_path, LEN, 0);
    assert_no_leftovers(&final_path);
}

#[tokio::test]
async fn restore_no_validator_spot_check_detects_change() {
    let s = server(|c| {
        c.etag = false;
        c.last_modified = false;
    })
    .await;
    let dir = tempfile::tempdir().unwrap();
    let final_path = paused_download(&s, dir.path()).await;
    s.set_version(1);
    let (dl, _) = restore(&s, &final_path, dir.path()).await;
    assert!(matches!(attention(&dl), AttentionReason::RemoteChanged));
    assert!(part_path(&final_path).exists());
}

#[tokio::test]
async fn restore_weak_etag_uses_spot_check() {
    // A weak ETag is not a validator: the changed body must still be detected by the
    // spot check, without sending `If-Range`.
    let s = server(|c| c.last_modified = false).await;
    let dir = tempfile::tempdir().unwrap();
    let final_path = paused_download(&s, dir.path()).await;
    let mut state = HttpResumeState::load(&final_path).await.unwrap();
    state.etag = Some("W/\"v0\"".into());
    state.etag_strong = false;
    tokio::fs::write(
        sidecar_path(&final_path),
        serde_json::to_vec(&state).unwrap(),
    )
    .await
    .unwrap();
    s.set_version(1);
    let before = s.requests_seen().len();
    let (dl, _) = restore(&s, &final_path, dir.path()).await;
    assert!(matches!(attention(&dl), AttentionReason::RemoteChanged));
    assert!(
        s.requests_seen()[before..]
            .iter()
            .all(|h| h.get("if-range").is_none())
    );
}

#[tokio::test]
async fn restore_last_modified_only() {
    let s = server(|c| c.etag = false).await;
    let dir = tempfile::tempdir().unwrap();
    let final_path = paused_download(&s, dir.path()).await;
    let before = s.requests_seen().len();
    let (dl, _) = restore(&s, &final_path, dir.path()).await;
    // Last-Modified goes into If-Range, and the spot check still runs.
    let seen = &s.requests_seen()[before..];
    let spot = format!("bytes=0-{}", 64 * KIB - 1);
    assert!(seen.iter().any(|h| {
        h.get("if-range")
            .is_some_and(|v| v.to_str().unwrap().ends_with("GMT"))
            && h.get("range").is_some_and(|v| v == spot.as_str())
    }));
    wait_completed(&dl).await;
    assert_file(&final_path, LEN, 0);

    let dir = tempfile::tempdir().unwrap();
    s.set_cfg(|c| c.bytes_per_sec = Some(256 * KIB));
    let final_path = paused_download(&s, dir.path()).await;
    s.set_version(1);
    let (dl, _) = restore(&s, &final_path, dir.path()).await;
    assert!(matches!(attention(&dl), AttentionReason::RemoteChanged));
}

#[tokio::test]
async fn restore_last_modified_same_but_body_changed() {
    // Last-Modified has 1 s granularity: an unchanged date does not prove an unchanged
    // body, so the spot check still runs.
    let s = server(|c| c.etag = false).await;
    let dir = tempfile::tempdir().unwrap();
    let final_path = paused_download(&s, dir.path()).await;
    s.set_version(3600); // same Last-Modified as version 0, different body
    let (dl, _) = restore(&s, &final_path, dir.path()).await;
    assert!(matches!(attention(&dl), AttentionReason::RemoteChanged));
}

#[tokio::test]
async fn restore_no_validator_length_changed() {
    let s = server(|c| {
        c.etag = false;
        c.last_modified = false;
    })
    .await;
    let dir = tempfile::tempdir().unwrap();
    let final_path = paused_download(&s, dir.path()).await;
    s.set_cfg(|c| c.len = 5 * MIB);
    let (dl, _) = restore(&s, &final_path, dir.path()).await;
    assert!(matches!(attention(&dl), AttentionReason::RemoteChanged));
}

#[tokio::test]
async fn restore_unknown_length_not_resumable() {
    let s = server(|c| {
        c.ranges = false;
        c.content_length = false;
    })
    .await;
    let dir = tempfile::tempdir().unwrap();
    let final_path = paused_download(&s, dir.path()).await;
    let before = s.requests_seen().len();
    let (dl, added) = restore(&s, &final_path, dir.path()).await;
    assert!(matches!(attention(&dl), AttentionReason::NotResumable));
    assert_eq!(added.total_bytes, None);
    assert_eq!(s.requests_seen().len(), before, "no network validation");
    assert!(part_path(&final_path).exists(), ".part must be kept");
    assert!(sidecar_path(&final_path).exists());
}

#[tokio::test]
async fn restore_single_stream_not_resumable() {
    let s = server(|c| c.ranges = false).await;
    let dir = tempfile::tempdir().unwrap();
    let final_path = paused_download(&s, dir.path()).await;
    let (dl, added) = restore(&s, &final_path, dir.path()).await;
    assert!(matches!(attention(&dl), AttentionReason::NotResumable));
    assert_eq!(added.total_bytes, Some(LEN));
    assert!(part_path(&final_path).exists(), ".part must be kept");
}

fn assert_validation_failed(dl: &HttpDownload) {
    let reason = attention(dl);
    assert!(
        matches!(reason, AttentionReason::ValidationFailed { .. }),
        "{reason:?}"
    );
}

#[tokio::test]
async fn restore_missing_sidecar() {
    let s = server(|_| {}).await;
    let dir = tempfile::tempdir().unwrap();
    let final_path = paused_download(&s, dir.path()).await;
    std::fs::remove_file(sidecar_path(&final_path)).unwrap();
    let (dl, added) = restore(&s, &final_path, dir.path()).await;
    assert_validation_failed(&dl);
    assert_eq!(added.path, final_path);
    assert!(part_path(&final_path).exists());
}

#[tokio::test]
async fn restore_corrupt_sidecar() {
    let s = server(|_| {}).await;
    let dir = tempfile::tempdir().unwrap();
    let final_path = paused_download(&s, dir.path()).await;
    std::fs::write(sidecar_path(&final_path), b"{ not json").unwrap();
    let (dl, _) = restore(&s, &final_path, dir.path()).await;
    assert_validation_failed(&dl);
}

#[tokio::test]
async fn restore_missing_part() {
    let s = server(|_| {}).await;
    let dir = tempfile::tempdir().unwrap();
    let final_path = paused_download(&s, dir.path()).await;
    std::fs::remove_file(part_path(&final_path)).unwrap();
    let (dl, _) = restore(&s, &final_path, dir.path()).await;
    assert_validation_failed(&dl);
}

#[tokio::test]
async fn restore_part_length_mismatch() {
    let s = server(|_| {}).await;
    let dir = tempfile::tempdir().unwrap();
    let final_path = paused_download(&s, dir.path()).await;
    let f = std::fs::OpenOptions::new()
        .write(true)
        .open(part_path(&final_path))
        .unwrap();
    f.set_len(LEN - 1).unwrap();
    drop(f);
    let (dl, _) = restore(&s, &final_path, dir.path()).await;
    assert_validation_failed(&dl);
}

#[tokio::test]
async fn restore_segments_not_covering_total() {
    let s = server(|_| {}).await;
    let dir = tempfile::tempdir().unwrap();
    let final_path = paused_download(&s, dir.path()).await;
    let mut state = HttpResumeState::load(&final_path).await.unwrap();
    state.segments.pop();
    tokio::fs::write(
        sidecar_path(&final_path),
        serde_json::to_vec(&state).unwrap(),
    )
    .await
    .unwrap();
    let (dl, _) = restore(&s, &final_path, dir.path()).await;
    assert_validation_failed(&dl);
}

#[tokio::test]
async fn restore_after_rename_before_sidecar_delete_is_completed() {
    let s = server(|_| {}).await;
    let dir = tempfile::tempdir().unwrap();
    let final_path = paused_download(&s, dir.path()).await;
    let sidecar = std::fs::read(sidecar_path(&final_path)).unwrap();
    // Finish the download, then put back the sidecar as if the process died between
    // renaming `.part` and deleting the sidecar.
    let (dl, _) = restore(&s, &final_path, dir.path()).await;
    wait_completed(&dl).await;
    drop(dl);
    std::fs::write(sidecar_path(&final_path), sidecar).unwrap();
    let before = s.requests_seen().len();

    let (tx, mut rx) = broadcast::channel(256);
    let (dl, added) = restore_with_events(&s, &final_path, dir.path(), tx).await;
    let st = dl.status();
    assert!(matches!(st.state, DownloadState::Completed), "{st:?}");
    assert_eq!(st.done_bytes, LEN);
    assert_eq!(added.total_bytes, Some(LEN));
    assert_file(&final_path, LEN, 0);
    assert_no_leftovers(&final_path);
    assert_eq!(s.requests_seen().len(), before, "no network needed");
    let mut completed = false;
    while let Ok(ev) = rx.try_recv() {
        completed |= matches!(ev, EngineEvent::Completed { id: 2 });
    }
    assert!(completed, "no Completed event");
}

#[tokio::test]
async fn restore_network_error_fails_then_resume_completes() {
    let s = server(|_| {}).await;
    let dir = tempfile::tempdir().unwrap();
    let final_path = paused_download(&s, dir.path()).await;
    s.set_cfg(|c| c.get_503_once = true);
    let (dl, _) = restore(&s, &final_path, dir.path()).await;
    let st = dl.status();
    assert!(
        matches!(
            st.state,
            DownloadState::Failed {
                transient: true,
                ..
            }
        ),
        "{st:?}"
    );
    assert!(part_path(&final_path).exists());
    dl.resume().await.unwrap();
    wait_completed(&dl).await;
    assert_file(&final_path, LEN, 0);
    assert_no_leftovers(&final_path);
}

#[tokio::test]
async fn resume_after_pause_validates_remote() {
    let s = server(|_| {}).await;
    let dir = tempfile::tempdir().unwrap();
    let (dl, added) = start(request(&s, dir.path()), cfg(dir.path())).await;
    wait_status(&dl, |s| s.done_bytes >= MIB, TIMEOUT).await;
    dl.pause().await;
    s.set_version(1);
    dl.resume().await.unwrap();
    assert!(matches!(attention(&dl), AttentionReason::RemoteChanged));
    assert!(!added.path.exists());
    assert!(part_path(&added.path).exists());
}

#[tokio::test]
async fn resume_after_pause_no_validator_spot_check() {
    let s = server(|c| {
        c.etag = false;
        c.last_modified = false;
    })
    .await;
    let dir = tempfile::tempdir().unwrap();
    let (dl, added) = start(request(&s, dir.path()), cfg(dir.path())).await;
    wait_status(&dl, |s| s.done_bytes >= MIB, TIMEOUT).await;
    dl.pause().await;
    s.set_version(1);
    dl.resume().await.unwrap();
    assert!(matches!(attention(&dl), AttentionReason::RemoteChanged));
    assert!(part_path(&added.path).exists());
}

#[tokio::test]
async fn restore_final_length_mismatch_keeps_sidecar() {
    let s = server(|_| {}).await;
    let dir = tempfile::tempdir().unwrap();
    let final_path = paused_download(&s, dir.path()).await;
    let sidecar = std::fs::read(sidecar_path(&final_path)).unwrap();
    let (dl, _) = restore(&s, &final_path, dir.path()).await;
    wait_completed(&dl).await;
    drop(dl);
    std::fs::write(sidecar_path(&final_path), sidecar).unwrap();
    let f = std::fs::OpenOptions::new()
        .write(true)
        .open(&final_path)
        .unwrap();
    f.set_len(LEN - 1).unwrap();
    drop(f);
    let (dl, _) = restore(&s, &final_path, dir.path()).await;
    assert_validation_failed(&dl);
    assert!(sidecar_path(&final_path).exists(), "sidecar must be kept");
}

#[tokio::test]
async fn restore_stalled_server_times_out_failed() {
    let s = server(|_| {}).await;
    let dir = tempfile::tempdir().unwrap();
    let final_path = paused_download(&s, dir.path()).await;
    s.set_cfg(|c| c.stall_headers = true);
    let mut c = cfg(dir.path());
    c.validate_timeout_ms = 300;
    let (tx, _) = broadcast::channel(256);
    let t = Instant::now();
    let (dl, _) = restore_with(&s, &final_path, c, tx).await;
    assert!(t.elapsed() < Duration::from_secs(5), "{:?}", t.elapsed());
    let st = dl.status();
    match &st.state {
        DownloadState::Failed {
            reason,
            transient: true,
        } => assert!(reason.contains("timed out"), "{reason}"),
        other => panic!("expected Failed, got {other:?}"),
    }
    assert!(part_path(&final_path).exists());
    assert!(sidecar_path(&final_path).exists());
}

#[tokio::test]
async fn pause_during_validation_is_prompt() {
    let s = server(|_| {}).await;
    let dir = tempfile::tempdir().unwrap();
    let (dl, added) = start(request(&s, dir.path()), cfg(dir.path())).await;
    wait_status(&dl, |s| s.done_bytes >= MIB, TIMEOUT).await;
    dl.pause().await;
    s.set_cfg(|c| c.stall_headers = true);
    let before = s.requests_seen().len();
    let dl = std::sync::Arc::new(dl);
    let resuming = tokio::spawn({
        let dl = dl.clone();
        async move { dl.resume().await }
    });
    // Validation has sent its request and is waiting for the stalled server.
    let deadline = Instant::now() + TIMEOUT;
    while s.requests_seen().len() == before {
        assert!(Instant::now() < deadline, "validation never started");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let t = Instant::now();
    dl.pause().await;
    assert!(
        t.elapsed() < Duration::from_millis(500),
        "pause took {:?}",
        t.elapsed()
    );
    let st = dl.status();
    assert!(matches!(st.state, DownloadState::Paused), "{st:?}");
    tokio::time::timeout(Duration::from_secs(1), resuming)
        .await
        .expect("resume did not return")
        .unwrap()
        .unwrap();
    assert!(matches!(dl.status().state, DownloadState::Paused));
    assert!(part_path(&added.path).exists());

    // Once the server answers again, resuming validates and completes.
    s.set_cfg(|c| c.stall_headers = false);
    dl.resume().await.unwrap();
    wait_completed(&dl).await;
    assert_file(&added.path, LEN, 0);
}

#[tokio::test]
async fn cancel_during_validation_is_prompt() {
    let s = server(|_| {}).await;
    let dir = tempfile::tempdir().unwrap();
    let (dl, added) = start(request(&s, dir.path()), cfg(dir.path())).await;
    wait_status(&dl, |s| s.done_bytes >= MIB, TIMEOUT).await;
    dl.pause().await;
    s.set_cfg(|c| c.stall_headers = true);
    let before = s.requests_seen().len();
    let dl = std::sync::Arc::new(dl);
    let resuming = tokio::spawn({
        let dl = dl.clone();
        async move { dl.resume().await }
    });
    let deadline = Instant::now() + TIMEOUT;
    while s.requests_seen().len() == before {
        assert!(Instant::now() < deadline, "validation never started");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let t = Instant::now();
    dl.cancel(true).await;
    assert!(
        t.elapsed() < Duration::from_millis(500),
        "cancel took {:?}",
        t.elapsed()
    );
    tokio::time::timeout(Duration::from_secs(1), resuming)
        .await
        .expect("resume did not return")
        .unwrap()
        .unwrap();
    assert_no_leftovers(&added.path);
    assert!(dl.resume().await.is_err(), "cancelled download resumed");
}

#[tokio::test]
async fn sidecar_never_stores_request_context() {
    let s = server(|_| {}).await;
    let dir = tempfile::tempdir().unwrap();
    let mut req = request(&s, dir.path());
    req.context = RequestContext {
        cookies: Some("session=topsecret".into()),
        basic_auth: Some(("user".into(), Some("hunter2".into()))),
        headers: vec![("X-Token".into(), "tok-secret".into())],
        ..RequestContext::default()
    };
    let (dl, added) = start(req.clone(), cfg(dir.path())).await;
    wait_status(&dl, |s| s.done_bytes >= MIB, TIMEOUT).await;
    dl.pause().await;
    drop(dl);
    let raw = String::from_utf8(std::fs::read(sidecar_path(&added.path)).unwrap()).unwrap();
    for secret in ["topsecret", "hunter2", "tok-secret", "X-Token"] {
        assert!(!raw.contains(secret), "{secret} persisted: {raw}");
    }
    let state = HttpResumeState::load(&added.path).await.unwrap();
    assert_eq!(state.request.context, RequestContext::default());

    // The restore gets the context from the caller and uses it for validation.
    s.set_cfg(|c| c.bytes_per_sec = None);
    let before = s.requests_seen().len();
    let (tx, _) = broadcast::channel(256);
    let (dl, _) = restore_req(req, &added.path, cfg(dir.path()), AddOptions::default(), tx).await;
    wait_completed(&dl).await;
    let later = &s.requests_seen()[before..];
    assert!(!later.is_empty());
    for h in later {
        assert_eq!(h.get("cookie").unwrap(), "session=topsecret");
        assert_eq!(h.get("x-token").unwrap(), "tok-secret");
    }
    assert_file(&added.path, LEN, 0);
}

#[tokio::test]
async fn restore_source_mismatch_needs_attention() {
    let s = server(|_| {}).await;
    let dir = tempfile::tempdir().unwrap();
    let final_path = paused_download(&s, dir.path()).await;
    let mut other = request(&s, dir.path());
    other.source = Source::Http {
        url: s.url("/other.bin"),
    };
    let before = s.requests_seen().len();
    let (tx, _) = broadcast::channel(256);
    let (dl, added) = restore_req(
        other,
        &final_path,
        cfg(dir.path()),
        AddOptions::default(),
        tx,
    )
    .await;
    assert_validation_failed(&dl);
    assert_eq!(added.path, final_path);
    assert_eq!(s.requests_seen().len(), before);
    assert!(part_path(&final_path).exists());
    assert!(sidecar_path(&final_path).exists());
}

#[tokio::test]
async fn restore_reresolves_expired_redirect() {
    let s = server(|_| {}).await;
    let dir = tempfile::tempdir().unwrap();
    let mut req = request(&s, dir.path());
    req.source = Source::Http {
        url: s.url("/expiring"),
    };
    let (dl, added) = start(req.clone(), cfg(dir.path())).await;
    wait_status(&dl, |s| s.done_bytes >= MIB, TIMEOUT).await;
    dl.pause().await;
    drop(dl);
    s.set_cfg(|c| c.bytes_per_sec = None);
    s.rotate_signed();

    let (tx, _) = broadcast::channel(256);
    let (dl, _) = restore_req(req, &added.path, cfg(dir.path()), AddOptions::default(), tx).await;
    wait_completed(&dl).await;
    assert_file(&added.path, LEN, 0);
    assert_no_leftovers(&added.path);
    let paths = s.paths_seen();
    assert!(paths.iter().any(|p| p == "/signed/1"), "{paths:?}");
}

#[tokio::test]
async fn reresolve_to_different_file_needs_attention() {
    let s = server(|_| {}).await;
    let dir = tempfile::tempdir().unwrap();
    let mut req = request(&s, dir.path());
    req.source = Source::Http {
        url: s.url("/expiring"),
    };
    let (dl, added) = start(req.clone(), cfg(dir.path())).await;
    wait_status(&dl, |s| s.done_bytes >= MIB, TIMEOUT).await;
    dl.pause().await;
    drop(dl);
    s.set_cfg(|c| c.bytes_per_sec = None);
    s.rotate_signed();
    s.set_version(1);

    let (tx, _) = broadcast::channel(256);
    let (dl, _) = restore_req(req, &added.path, cfg(dir.path()), AddOptions::default(), tx).await;
    assert!(matches!(attention(&dl), AttentionReason::RemoteChanged));
    assert!(part_path(&added.path).exists());
}

#[tokio::test]
async fn restore_after_kill_mid_steal() {
    // Four 1 MiB segments; the first one's connection is slow, so the others finish
    // and steal from it. The process is "killed" right after a steal.
    let s = server(|c| {
        c.bytes_per_sec = Some(512 * KIB);
        c.slow_range_start = Some(0);
        c.slow_bytes_per_sec = Some(32 * KIB);
    })
    .await;
    let dir = tempfile::tempdir().unwrap();
    let req = request(&s, dir.path());
    let mut c = cfg(dir.path());
    c.connections = 4;
    c.min_segment_bytes = 64 * KIB;
    let killed = std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let (dl, added, segs) = rt.block_on(async {
            let (dl, added) = start(req, c).await;
            let deadline = Instant::now() + TIMEOUT;
            loop {
                let EngineDetail::Http { segments, .. } = dl.detail() else {
                    unreachable!()
                };
                if segments.len() > 4 {
                    break;
                }
                assert!(Instant::now() < deadline, "no steal: {segments:?}");
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            // Wait for a periodic flush to record the split.
            loop {
                if let Ok(st) = HttpResumeState::load(&added.path).await
                    && st.segments.len() > 4
                {
                    break;
                }
                assert!(Instant::now() < deadline, "split never flushed");
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            let EngineDetail::Http { segments, .. } = dl.detail() else {
                unreachable!()
            };
            (dl, added, segments)
        });
        let st = dl.status();
        rt.shutdown_background();
        drop(dl);
        (added.path, st, segs)
    });
    let (final_path, st, segs) = tokio::task::spawn_blocking(move || killed.join().unwrap())
        .await
        .unwrap();
    assert!(matches!(st.state, DownloadState::Active), "{st:?}");
    assert!(segs.len() > 4);

    let state = HttpResumeState::load(&final_path).await.unwrap();
    assert!(
        state.segments.len() > 4,
        "split not recorded: {:?}",
        state.segments
    );
    let mut cursor = 0;
    let part = std::fs::read(part_path(&final_path)).unwrap();
    for seg in &state.segments {
        assert_eq!(seg.start, cursor, "gap or overlap: {:?}", state.segments);
        assert!(seg.done <= seg.end - seg.start);
        for i in seg.start..seg.start + seg.done {
            assert_eq!(part[i as usize], body_byte(i, 0), "unverified byte {i}");
        }
        cursor = seg.end;
    }
    assert_eq!(cursor, LEN);

    s.set_cfg(|c| {
        c.bytes_per_sec = None;
        c.slow_range_start = None;
    });
    let (dl, _) = restore(&s, &final_path, dir.path()).await;
    wait_completed(&dl).await;
    assert_file(&final_path, LEN, 0);
    assert_no_leftovers(&final_path);
}

fn secret_context() -> RequestContext {
    RequestContext {
        cookies: Some("session=topsecret".into()),
        basic_auth: Some(("user".into(), Some("hunter2".into()))),
        headers: vec![
            ("X-Token".into(), "tok".into()),
            ("authorization".into(), "Bearer hdr-secret".into()),
            ("Proxy-Authorization".into(), "Basic proxy-secret".into()),
            ("COOKIE".into(), "hdr=cookie".into()),
        ],
        referer: Some("https://example.com/page".into()),
        user_agent: Some("TestAgent/1".into()),
    }
}

/// A redirect to another origin (host and port) must not carry the original host's
/// cookies or credentials to the target: not on the probe, not on segment requests, not
/// on the validation after a restart.
#[tokio::test]
async fn cross_origin_redirect_target_gets_no_credentials() {
    let target = server(|_| {}).await;
    let to = format!("http://localhost:{}/file.bin", target.port());
    let origin = server(|c| c.redirect_to = Some(to)).await;
    let dir = tempfile::tempdir().unwrap();
    let mut req = request(&origin, dir.path());
    req.source = Source::Http {
        url: origin.url("/away"),
    };
    req.context = secret_context();
    let (dl, added) = start(req.clone(), cfg(dir.path())).await;
    wait_status(&dl, |s| s.done_bytes >= MIB, TIMEOUT).await;
    dl.pause().await;
    drop(dl);
    let before_restore = target.requests_seen().len();
    target.set_cfg(|c| c.bytes_per_sec = None);
    let (tx, _) = broadcast::channel(256);
    let (dl, _) = restore_req(req, &added.path, cfg(dir.path()), AddOptions::default(), tx).await;
    wait_completed(&dl).await;
    assert_file(&added.path, LEN, 0);

    let seen = target.requests_seen();
    assert!(seen.len() > before_restore, "no request after the restore");
    assert!(seen.len() >= 3, "{} requests", seen.len());
    for h in &seen {
        assert!(h.get("cookie").is_none(), "cookie leaked: {h:?}");
        assert!(h.get("authorization").is_none(), "auth leaked: {h:?}");
        assert!(h.get("proxy-authorization").is_none(), "proxy auth leaked");
        assert_eq!(h.get("user-agent").unwrap(), "TestAgent/1");
        assert_eq!(h.get("x-token").unwrap(), "tok");
    }
    // Requests sent straight to the target keep the referer (on the redirect hop itself
    // reqwest substitutes the redirecting URL).
    let with_referer = seen
        .iter()
        .filter(|h| {
            h.get("referer")
                .is_some_and(|r| r == "https://example.com/page")
        })
        .count();
    assert!(
        with_referer >= 2,
        "{with_referer} requests kept the referer"
    );
    // The original host did get them.
    let first = &origin.requests_seen()[0];
    assert!(first.get("cookie").is_some());
    assert!(first.get("authorization").is_some());
}

#[tokio::test]
async fn same_origin_redirect_keeps_credentials() {
    let s = server(|_| {}).await;
    let dir = tempfile::tempdir().unwrap();
    let mut req = request(&s, dir.path());
    req.source = Source::Http {
        url: s.url("/redirect"),
    };
    req.context = RequestContext {
        cookies: Some("session=topsecret".into()),
        basic_auth: Some(("user".into(), Some("hunter2".into()))),
        ..RequestContext::default()
    };
    let (dl, added) = start(req.clone(), cfg(dir.path())).await;
    wait_status(&dl, |s| s.done_bytes >= MIB, TIMEOUT).await;
    dl.pause().await;
    drop(dl);
    s.set_cfg(|c| c.bytes_per_sec = None);
    let (tx, _) = broadcast::channel(256);
    let (dl, _) = restore_req(req, &added.path, cfg(dir.path()), AddOptions::default(), tx).await;
    wait_completed(&dl).await;
    let paths = s.paths_seen();
    let seen = s.requests_seen();
    let to_file: Vec<_> = paths
        .iter()
        .zip(&seen)
        .filter(|(p, _)| *p == "/file.bin")
        .collect();
    assert!(to_file.len() >= 3);
    for (_, h) in to_file {
        assert_eq!(h.get("cookie").unwrap(), "session=topsecret");
        assert!(h.get("authorization").is_some());
    }
}
