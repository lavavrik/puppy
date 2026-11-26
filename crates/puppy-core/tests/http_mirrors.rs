//! One HTTP download from several locations: the primary URL and its mirrors, each its
//! own test server (another port is another origin).

use puppy_core::config::HttpConfig;
use puppy_core::engine::{AddOptions, Added};
use puppy_core::http::download::HttpDownload;
use puppy_core::http::probe::build_client;
use puppy_core::http::resume::{HttpResumeState, sidecar_path};
use puppy_core::limiter::TokenBucket;
use puppy_core::types::{
    AddRequest, AttentionReason, DownloadState, DownloadStatus, EngineDetail, Mirror, MirrorState,
    MirrorView, RequestContext, Source,
};
use puppy_testserver::{ServerCfg, TestServer, body_byte};
use std::path::Path;
use std::time::{Duration, Instant};
use tokio::sync::broadcast;

const KIB: u64 = 1024;
const MIB: u64 = 1024 * 1024;
const LEN: u64 = 4 * MIB;
const TIMEOUT: Duration = Duration::from_secs(30);

fn cfg(dir: &Path) -> HttpConfig {
    HttpConfig {
        backoff_base_ms: 10,
        backoff_max_ms: 100,
        flush_interval_ms: 100,
        min_segment_bytes: 256 * KIB,
        default_dir: dir.to_path_buf(),
        ..HttpConfig::default()
    }
}

async fn server(f: impl FnOnce(&mut ServerCfg)) -> TestServer {
    let mut c = ServerCfg {
        len: LEN,
        ..ServerCfg::default()
    };
    f(&mut c);
    TestServer::start(c).await
}

/// A download of `primary`'s `/file.bin` with the others' as mirrors.
fn request(primary: &TestServer, mirrors: &[&TestServer], dir: &Path) -> AddRequest {
    AddRequest {
        source: Source::Http {
            url: primary.url("/file.bin"),
        },
        output_dir: Some(dir.to_path_buf()),
        filename: None,
        context: RequestContext::default(),
        files: Default::default(),
        seed: None,
        checksum: None,
        mirrors: mirrors
            .iter()
            .map(|s| Mirror::new(s.url("/file.bin")))
            .collect(),
    }
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

async fn restore(req: AddRequest, final_path: &Path, cfg: HttpConfig) -> HttpDownload {
    let (tx, _) = broadcast::channel(256);
    let client = build_client(&cfg).unwrap();
    HttpDownload::restore(
        2,
        req,
        final_path,
        AddOptions::default(),
        client,
        cfg,
        TokenBucket::new(None),
        tx,
    )
    .await
    .unwrap()
    .0
}

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
        assert!(
            Instant::now() < deadline,
            "timed out; last status {st:?}, mirrors {:?}",
            mirrors(dl)
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

async fn wait_completed(dl: &HttpDownload) {
    wait_status(dl, |s| matches!(s.state, DownloadState::Completed), TIMEOUT).await;
}

fn mirrors(dl: &HttpDownload) -> Vec<MirrorView> {
    match dl.detail() {
        EngineDetail::Http { mirrors, .. } => mirrors,
        other => panic!("unexpected detail {other:?}"),
    }
}

fn dropped_reason(m: &MirrorView) -> &str {
    match &m.state {
        MirrorState::Dropped { reason } => reason,
        other => panic!("{} not dropped: {other:?}", m.url),
    }
}

fn assert_file(path: &Path, len: u64, version: u32) {
    let data = std::fs::read(path).unwrap();
    assert_eq!(data.len() as u64, len, "length of {}", path.display());
    for (i, b) in data.iter().enumerate() {
        assert_eq!(*b, body_byte(i as u64, version), "byte {i}");
    }
}

/// Connections are spread over both locations: each gets range requests and serves
/// part of the file.
#[tokio::test]
async fn segments_spread_over_two_mirrors() {
    let a = server(|c| c.bytes_per_sec = Some(MIB)).await;
    let b = server(|c| c.bytes_per_sec = Some(MIB)).await;
    let dir = tempfile::tempdir().unwrap();
    let (dl, added) = start(request(&a, &[&b], dir.path()), cfg(dir.path())).await;
    assert_eq!(added.name, "file.bin");
    let busy = wait_status(
        &dl,
        |_| mirrors(&dl).iter().all(|m| m.connections > 0),
        TIMEOUT,
    );
    busy.await;
    let shown = mirrors(&dl);
    assert_eq!(shown.len(), 2);
    assert!(shown[0].primary && !shown[1].primary);
    // 8 connections in all, shared evenly.
    let conns: Vec<u16> = shown.iter().map(|m| m.connections).collect();
    assert_eq!(conns.iter().sum::<u16>(), 8, "{conns:?}");
    assert!(conns.iter().all(|&c| c == 4), "{conns:?}");
    wait_completed(&dl).await;
    assert_file(&added.path, LEN, 0);
    assert!(a.range_requests() > 0 && b.range_requests() > 0);
    let got: Vec<u64> = mirrors(&dl).iter().map(|m| m.done_bytes).collect();
    assert_eq!(got.iter().sum::<u64>(), LEN, "{got:?}");
    assert!(got.iter().all(|&g| g > 0), "{got:?}");
}

/// Without mirrors the detail lists none (the JSON has no `mirrors` key).
#[tokio::test]
async fn no_mirrors_no_mirror_detail() {
    let a = server(|_| {}).await;
    let dir = tempfile::tempdir().unwrap();
    let (dl, _) = start(request(&a, &[], dir.path()), cfg(dir.path())).await;
    wait_completed(&dl).await;
    assert!(mirrors(&dl).is_empty());
    let json = serde_json::to_string(&dl.detail()).unwrap();
    assert!(!json.contains("mirrors"), "{json}");
}

/// A mirror serving a file of another size is dropped with its reason and never asked
/// for a range.
#[tokio::test]
async fn mirror_with_wrong_size_is_dropped() {
    let a = server(|_| {}).await;
    let b = server(|c| c.len = LEN + 1).await;
    let dir = tempfile::tempdir().unwrap();
    let (dl, added) = start(request(&a, &[&b], dir.path()), cfg(dir.path())).await;
    wait_completed(&dl).await;
    assert_file(&added.path, LEN, 0);
    let shown = mirrors(&dl);
    assert_eq!(shown[0].state, MirrorState::InUse);
    let why = dropped_reason(&shown[1]);
    assert!(
        why.contains("size") && why.contains(&(LEN + 1).to_string()),
        "{why}"
    );
    assert_eq!(b.range_requests(), 0);
    assert_eq!(shown[1].done_bytes, 0);
}

/// A mirror without range support is dropped while the ranged locations are used.
#[tokio::test]
async fn mirror_without_ranges_is_dropped() {
    let a = server(|_| {}).await;
    let b = server(|c| c.ranges = false).await;
    let dir = tempfile::tempdir().unwrap();
    let (dl, added) = start(request(&a, &[&b], dir.path()), cfg(dir.path())).await;
    wait_completed(&dl).await;
    assert_file(&added.path, LEN, 0);
    assert!(dropped_reason(&mirrors(&dl)[1]).contains("range"));
}

/// The primary does not answer, a mirror does: the download proceeds from the mirror.
#[tokio::test]
async fn primary_down_mirror_up() {
    let a = server(|c| c.fail_status = Some(404)).await;
    let b = server(|_| {}).await;
    let dir = tempfile::tempdir().unwrap();
    let (dl, added) = start(request(&a, &[&b], dir.path()), cfg(dir.path())).await;
    wait_completed(&dl).await;
    assert_file(&added.path, LEN, 0);
    assert_eq!(added.name, "file.bin");
    let shown = mirrors(&dl);
    assert!(dropped_reason(&shown[0]).contains("404"), "{shown:?}");
    assert_eq!(shown[1].done_bytes, LEN);
    assert_eq!(a.range_requests(), 0);
}

/// Every location failing its probe fails the add; transiently if one may come back.
#[tokio::test]
async fn every_location_down_fails_the_add() {
    let a = server(|c| c.fail_status = Some(404)).await;
    let b = server(|c| c.fail_status = Some(503)).await;
    let dir = tempfile::tempdir().unwrap();
    let c = cfg(dir.path());
    let client = build_client(&c).unwrap();
    let (tx, _) = broadcast::channel(16);
    let req = request(&a, &[&b], dir.path());
    let e = HttpDownload::start_fresh(
        1,
        req,
        AddOptions::default(),
        client,
        c,
        TokenBucket::new(None),
        tx,
    )
    .await
    .err()
    .expect("no location works");
    assert!(e.is_transient(), "{e}");
    let msg = e.to_string();
    assert!(
        msg.contains("no mirror works") && msg.contains("404") && msg.contains("503"),
        "{msg}"
    );
}

/// A mirror that goes down mid-download: its ranges move to the other location, which
/// finishes the file; the dead one ends up benched or dropped.
#[tokio::test]
async fn mirror_dying_mid_download_fails_over() {
    let a = server(|c| c.bytes_per_sec = Some(512 * KIB)).await;
    let b = server(|c| c.bytes_per_sec = Some(512 * KIB)).await;
    let dir = tempfile::tempdir().unwrap();
    let (dl, added) = start(request(&a, &[&b], dir.path()), cfg(dir.path())).await;
    wait_status(&dl, |s| s.done_bytes >= MIB, TIMEOUT).await;
    b.set_cfg(|c| c.fail_status = Some(503));
    // Its failure shows (benched, with the reason) while its ranges move to the
    // primary, which may then finish them all.
    let benched = |m: &MirrorView| matches!(&m.state, MirrorState::Benched { reason, .. } if !reason.is_empty());
    wait_status(&dl, |_| benched(&mirrors(&dl)[1]), TIMEOUT).await;
    a.set_cfg(|c| c.bytes_per_sec = None);
    wait_completed(&dl).await;
    assert_file(&added.path, LEN, 0);
    let shown = mirrors(&dl);
    assert!(shown[0].done_bytes > shown[1].done_bytes, "{shown:?}");
    assert_eq!(shown[0].done_bytes + shown[1].done_bytes, LEN, "{shown:?}");
}

/// When every location keeps failing the download fails (transiently), naming each.
#[tokio::test]
async fn every_location_dying_fails_transiently() {
    let a = server(|c| c.bytes_per_sec = Some(256 * KIB)).await;
    let b = server(|c| c.bytes_per_sec = Some(256 * KIB)).await;
    let dir = tempfile::tempdir().unwrap();
    let mut c = cfg(dir.path());
    c.max_retries = 2;
    let (dl, _) = start(request(&a, &[&b], dir.path()), c).await;
    wait_status(&dl, |s| s.done_bytes >= 256 * KIB, TIMEOUT).await;
    a.set_cfg(|c| c.fail_status = Some(503));
    b.set_cfg(|c| c.fail_status = Some(503));
    let st = wait_status(
        &dl,
        |s| matches!(s.state, DownloadState::Failed { .. }),
        TIMEOUT,
    )
    .await;
    let DownloadState::Failed { reason, transient } = st.state else {
        unreachable!()
    };
    assert!(transient, "{reason}");
    assert!(reason.contains("no mirror works"), "{reason}");
    // A resume tries them all again.
    a.set_cfg(|c| {
        c.fail_status = None;
        c.bytes_per_sec = None;
    });
    b.set_cfg(|c| {
        c.fail_status = None;
        c.bytes_per_sec = None;
    });
    dl.resume().await.unwrap();
    wait_completed(&dl).await;
}

/// After a restart the mirror list and each location's validators come back from the
/// sidecar, both are validated on their own, and both serve the rest.
#[tokio::test]
async fn resume_after_restart_with_mirrors() {
    let a = server(|c| c.bytes_per_sec = Some(256 * KIB)).await;
    let b = server(|c| {
        c.bytes_per_sec = Some(256 * KIB);
        c.etag = false; // Last-Modified only: validated with a byte comparison too
    })
    .await;
    let dir = tempfile::tempdir().unwrap();
    let req = request(&a, &[&b], dir.path());
    let (dl, added) = start(req.clone(), cfg(dir.path())).await;
    wait_status(&dl, |s| s.done_bytes >= MIB, TIMEOUT).await;
    dl.pause().await;
    drop(dl);
    let state = HttpResumeState::load(&added.path).await.unwrap();
    assert_eq!(state.request.mirrors, req.mirrors);
    assert_eq!(state.mirrors.len(), 1);
    assert_eq!(state.mirrors[0].url, b.url("/file.bin"));
    assert!(state.mirrors[0].etag.is_none());
    assert!(state.mirrors[0].last_modified.is_some());
    assert!(state.etag.is_some(), "the primary's own validator");

    for s in [&a, &b] {
        s.set_cfg(|c| c.bytes_per_sec = Some(MIB));
    }
    let (a_before, b_before) = (a.range_requests(), b.range_requests());
    let dl = restore(req, &added.path, cfg(dir.path())).await;
    wait_status(
        &dl,
        |_| mirrors(&dl).iter().all(|m| m.connections > 0),
        TIMEOUT,
    )
    .await;
    wait_completed(&dl).await;
    assert_file(&added.path, LEN, 0);
    assert!(a.range_requests() > a_before && b.range_requests() > b_before);
    assert!(!sidecar_path(&added.path).exists());
}

/// A mirror whose file changed while the download was stopped is dropped on resume;
/// the primary, unchanged, finishes the file. The changed bytes are never used.
#[tokio::test]
async fn changed_mirror_is_dropped_on_resume() {
    let a = server(|c| c.bytes_per_sec = Some(256 * KIB)).await;
    let b = server(|c| c.bytes_per_sec = Some(256 * KIB)).await;
    let dir = tempfile::tempdir().unwrap();
    let req = request(&a, &[&b], dir.path());
    let (dl, added) = start(req.clone(), cfg(dir.path())).await;
    wait_status(&dl, |s| s.done_bytes >= MIB, TIMEOUT).await;
    dl.pause().await;
    drop(dl);
    a.set_cfg(|c| c.bytes_per_sec = None);
    b.set_cfg(|c| c.bytes_per_sec = None);
    b.set_version(1);
    let dl = restore(req, &added.path, cfg(dir.path())).await;
    wait_completed(&dl).await;
    assert_file(&added.path, LEN, 0);
    let shown = mirrors(&dl);
    assert!(dropped_reason(&shown[1]).contains("changed"), "{shown:?}");
    assert_eq!(shown[1].done_bytes, 0);
}

/// Every location changed: the download needs attention, as without mirrors.
#[tokio::test]
async fn every_location_changed_needs_attention() {
    let a = server(|c| c.bytes_per_sec = Some(256 * KIB)).await;
    let b = server(|c| c.bytes_per_sec = Some(256 * KIB)).await;
    let dir = tempfile::tempdir().unwrap();
    let req = request(&a, &[&b], dir.path());
    let (dl, added) = start(req.clone(), cfg(dir.path())).await;
    wait_status(&dl, |s| s.done_bytes >= MIB, TIMEOUT).await;
    dl.pause().await;
    drop(dl);
    a.set_version(1);
    b.set_version(1);
    let dl = restore(req, &added.path, cfg(dir.path())).await;
    assert_eq!(
        dl.status().state,
        DownloadState::NeedsAttention {
            reason: AttentionReason::RemoteChanged
        }
    );
}

/// A mirror whose file changes mid-download (its `If-Range` no longer matches) is
/// dropped; the primary carries on.
#[tokio::test]
async fn mirror_changing_mid_download_is_dropped() {
    let a = server(|c| c.bytes_per_sec = Some(512 * KIB)).await;
    let b = server(|c| c.bytes_per_sec = Some(512 * KIB)).await;
    let dir = tempfile::tempdir().unwrap();
    let (dl, added) = start(request(&a, &[&b], dir.path()), cfg(dir.path())).await;
    wait_status(&dl, |s| s.done_bytes >= 512 * KIB, TIMEOUT).await;
    b.set_version(1);
    // Its running responses end, so its next requests carry `If-Range`.
    b.set_cfg(|c| c.fail_status = Some(503));
    tokio::time::sleep(Duration::from_millis(50)).await;
    b.set_cfg(|c| {
        c.fail_status = None;
        c.bytes_per_sec = None;
    });
    a.set_cfg(|c| c.bytes_per_sec = None);
    wait_completed(&dl).await;
    // Bytes from before the change are the old file's: all of it matches version 0.
    assert_file(&added.path, LEN, 0);
}

fn secret_context() -> RequestContext {
    RequestContext {
        cookies: Some("session=topsecret".into()),
        basic_auth: Some(("user".into(), Some("hunter2".into()))),
        headers: vec![
            ("X-Token".into(), "tok".into()),
            ("Authorization".into(), "Bearer hdr-secret".into()),
        ],
        referer: Some("https://example.com/page".into()),
        user_agent: Some("TestAgent/1".into()),
    }
}

/// Cookies, credentials and headers given for the download go to the primary's origin
/// only: a mirror on another origin gets none of them (only the user agent), neither on
/// the probe nor on segment requests nor on validation after a restart.
#[tokio::test]
async fn cookies_and_headers_are_not_sent_to_mirror_hosts() {
    let a = server(|c| c.bytes_per_sec = Some(512 * KIB)).await;
    let b = server(|c| c.bytes_per_sec = Some(512 * KIB)).await;
    let dir = tempfile::tempdir().unwrap();
    let mut req = request(&a, &[&b], dir.path());
    req.context = secret_context();
    let (dl, added) = start(req.clone(), cfg(dir.path())).await;
    wait_status(&dl, |s| s.done_bytes >= MIB, TIMEOUT).await;
    dl.pause().await;
    drop(dl);
    for s in [&a, &b] {
        s.set_cfg(|c| c.bytes_per_sec = None);
    }
    let dl = restore(req, &added.path, cfg(dir.path())).await;
    wait_completed(&dl).await;
    assert_file(&added.path, LEN, 0);

    let seen = b.requests_seen();
    assert!(
        b.range_requests() > 0 && seen.len() >= 3,
        "{} requests",
        seen.len()
    );
    for h in &seen {
        for name in ["cookie", "authorization", "x-token", "referer"] {
            assert!(h.get(name).is_none(), "{name} sent to the mirror: {h:?}");
        }
        assert_eq!(h.get("user-agent").unwrap(), "TestAgent/1");
    }
    for h in &a.requests_seen() {
        assert!(
            h.get("cookie").is_some() && h.get("x-token").is_some(),
            "{h:?}"
        );
    }
}

/// A mirror on the primary's own origin is the same site: it gets the cookies.
#[tokio::test]
async fn same_origin_mirror_keeps_cookies() {
    let a = server(|_| {}).await;
    let dir = tempfile::tempdir().unwrap();
    let mut req = request(&a, &[], dir.path());
    req.mirrors = vec![Mirror::new(a.url("/redirect"))];
    req.context = secret_context();
    let (dl, added) = start(req, cfg(dir.path())).await;
    wait_completed(&dl).await;
    assert_file(&added.path, LEN, 0);
    for h in &a.requests_seen() {
        assert!(h.get("cookie").is_some(), "{h:?}");
    }
}

/// Mirrors are tried by priority (lower first), then as given; the order shows.
#[tokio::test]
async fn mirrors_are_ordered_by_priority() {
    let a = server(|_| {}).await;
    let b = server(|_| {}).await;
    let c = server(|_| {}).await;
    let dir = tempfile::tempdir().unwrap();
    let mut req = request(&a, &[&b, &c], dir.path());
    req.mirrors[1].priority = Some(1);
    let (dl, _) = start(req, cfg(dir.path())).await;
    wait_completed(&dl).await;
    let urls: Vec<String> = mirrors(&dl).into_iter().map(|m| m.url).collect();
    let want: Vec<String> = [&a, &c, &b]
        .iter()
        .map(|s| s.url("/file.bin").to_string())
        .collect();
    assert_eq!(urls, want);
}
