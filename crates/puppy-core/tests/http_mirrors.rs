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
        size: None,
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

/// With an expected size (a Metalink's), a primary serving another size is dropped
/// instead of outvoting the mirror that has the right one.
#[tokio::test]
async fn expected_size_drops_a_stale_primary() {
    let a = server(|c| c.len = LEN + 1).await;
    let b = server(|_| {}).await;
    let dir = tempfile::tempdir().unwrap();
    let req = AddRequest {
        size: Some(LEN),
        ..request(&a, &[&b], dir.path())
    };
    let (dl, added) = start(req, cfg(dir.path())).await;
    wait_completed(&dl).await;
    assert_file(&added.path, LEN, 0);
    let shown = mirrors(&dl);
    let why = dropped_reason(&shown[0]);
    assert!(
        why.contains(&(LEN + 1).to_string()) && why.contains("expected"),
        "{why}"
    );
    assert_eq!(a.range_requests(), 0);
    assert_eq!(shown[1].done_bytes, LEN);
}

/// A primary that reports no size (no Content-Length, no ranges) does not outvote a
/// mirror that reports the expected one: the mirror is the reference and serves the file.
#[tokio::test]
async fn expected_size_prefers_a_location_that_reports_it() {
    let a = server(|c| {
        c.len = LEN + 1;
        c.content_length = false;
        c.ranges = false;
    })
    .await;
    let b = server(|_| {}).await;
    let dir = tempfile::tempdir().unwrap();
    let req = AddRequest {
        size: Some(LEN),
        ..request(&a, &[&b], dir.path())
    };
    let (dl, added) = start(req, cfg(dir.path())).await;
    wait_completed(&dl).await;
    assert_file(&added.path, LEN, 0);
    let shown = mirrors(&dl);
    assert!(dropped_reason(&shown[0]).contains("range"), "{shown:?}");
    assert_eq!(shown[1].done_bytes, LEN);
}

/// No location reports a size: the one stream's file is checked against the expected
/// size at the end and fails, instead of completing with another length.
#[tokio::test]
async fn expected_size_checks_the_finished_file() {
    let a = server(|c| {
        c.len = LEN + 1;
        c.content_length = false;
        c.ranges = false;
    })
    .await;
    let dir = tempfile::tempdir().unwrap();
    let req = AddRequest {
        size: Some(LEN),
        ..request(&a, &[], dir.path())
    };
    let (dl, _) = start(req, cfg(dir.path())).await;
    let st = wait_status(
        &dl,
        |s| {
            matches!(
                s.state,
                DownloadState::Failed { .. } | DownloadState::Completed
            )
        },
        TIMEOUT,
    )
    .await;
    match st.state {
        DownloadState::Failed { reason, transient } => {
            assert!(!transient);
            assert!(
                reason.contains(&format!("{} bytes, not the expected {LEN}", LEN + 1)),
                "{reason}"
            );
        }
        other => panic!("completed with the wrong size: {other:?}"),
    }
}

/// No location serving the expected size fails the add, not transiently.
#[tokio::test]
async fn expected_size_nowhere_fails_the_add() {
    let dir = tempfile::tempdir().unwrap();
    let c = cfg(dir.path());
    let a = server(|_| {}).await;
    let b = server(|c| c.len = LEN + 2).await;
    for (req, want) in [
        (request(&a, &[], dir.path()), "not the expected"),
        (request(&a, &[&b], dir.path()), "no mirror works"),
    ] {
        let req = AddRequest {
            size: Some(LEN - 1),
            ..req
        };
        let (tx, _) = broadcast::channel(16);
        let e = HttpDownload::start_fresh(
            1,
            req,
            AddOptions::default(),
            build_client(&c).unwrap(),
            c.clone(),
            TokenBucket::new(None),
            tx,
        )
        .await
        .err()
        .expect("no location has the size");
        assert!(!e.is_transient(), "{e}");
        let msg = e.to_string();
        assert!(
            msg.contains(want) && msg.contains(&(LEN - 1).to_string()),
            "{msg}"
        );
    }
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
        ssh_agent: None,
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

// ---- review fixes ----

/// A mirror without validators (no ETag, no Last-Modified) cannot be checked by
/// `If-Range`: before each range it gets, a window of bytes already downloaded is
/// compared against it. One whose file changed is dropped instead of mixing another
/// version into the file.
#[tokio::test]
async fn validator_less_mirror_that_changes_is_never_mixed_in() {
    let a = server(|c| c.bytes_per_sec = Some(512 * KIB)).await;
    let b = server(|c| {
        c.bytes_per_sec = Some(512 * KIB);
        c.etag = false;
        c.last_modified = false;
    })
    .await;
    let dir = tempfile::tempdir().unwrap();
    let (dl, added) = start(request(&a, &[&b], dir.path()), cfg(dir.path())).await;
    wait_status(&dl, |s| s.done_bytes >= 512 * KIB, TIMEOUT).await;
    b.set_version(1);
    b.set_cfg(|c| c.bytes_per_sec = None);
    a.set_cfg(|c| c.fail_status = Some(503));
    let dropped = |m: &MirrorView| matches!(&m.state, MirrorState::Dropped { .. });
    wait_status(&dl, |_| dropped(&mirrors(&dl)[1]), TIMEOUT).await;
    assert!(
        dropped_reason(&mirrors(&dl)[1]).contains("changed"),
        "{:?}",
        mirrors(&dl)
    );
    a.set_cfg(|c| {
        c.fail_status = None;
        c.bytes_per_sec = None;
    });
    let st = wait_status(
        &dl,
        |s| {
            matches!(
                s.state,
                DownloadState::Completed | DownloadState::Failed { .. }
            )
        },
        TIMEOUT,
    )
    .await;
    if matches!(st.state, DownloadState::Failed { .. }) {
        dl.resume().await.unwrap();
        wait_completed(&dl).await;
    }
    assert_file(&added.path, LEN, 0);
}

/// Before any byte is downloaded from a location with validators, a validator-less
/// mirror has nothing to be compared against: it waits for the first bytes.
#[tokio::test]
async fn validator_less_mirror_waits_for_checked_bytes() {
    // The primary alone needs 4 s (8 connections at 128 KiB/s each): the mirror is
    // checked (and dropped) only once bytes exist, which must not race the end.
    let a = server(|c| c.bytes_per_sec = Some(128 * KIB)).await;
    let b = server(|c| {
        c.etag = false;
        c.last_modified = false;
    })
    .await;
    let dir = tempfile::tempdir().unwrap();
    // A mirror serving another version from the start.
    b.set_version(1);
    let (dl, added) = start(request(&a, &[&b], dir.path()), cfg(dir.path())).await;
    let dropped = |m: &MirrorView| matches!(&m.state, MirrorState::Dropped { .. });
    wait_status(&dl, |_| dropped(&mirrors(&dl)[1]), TIMEOUT).await;
    a.set_cfg(|c| c.bytes_per_sec = None);
    wait_completed(&dl).await;
    assert_file(&added.path, LEN, 0);
    assert_eq!(mirrors(&dl)[1].done_bytes, 0);
}

/// The primary's file changed while the download was stopped: it needs attention, as
/// without mirrors, even though a mirror still serves the old file.
#[tokio::test]
async fn primary_changed_on_resume_needs_attention() {
    let a = server(|c| c.bytes_per_sec = Some(256 * KIB)).await;
    let b = server(|c| c.bytes_per_sec = Some(256 * KIB)).await;
    let dir = tempfile::tempdir().unwrap();
    let req = request(&a, &[&b], dir.path());
    let (dl, added) = start(req.clone(), cfg(dir.path())).await;
    wait_status(&dl, |s| s.done_bytes >= MIB, TIMEOUT).await;
    dl.pause().await;
    drop(dl);
    a.set_version(1);
    let dl = restore(req, &added.path, cfg(dir.path())).await;
    assert_eq!(
        dl.status().state,
        DownloadState::NeedsAttention {
            reason: AttentionReason::RemoteChanged
        }
    );
}

/// The primary's file changing mid-download needs attention too.
#[tokio::test]
async fn primary_changing_mid_download_needs_attention() {
    let a = server(|c| c.bytes_per_sec = Some(512 * KIB)).await;
    let b = server(|c| c.bytes_per_sec = Some(512 * KIB)).await;
    let dir = tempfile::tempdir().unwrap();
    let (dl, _) = start(request(&a, &[&b], dir.path()), cfg(dir.path())).await;
    wait_status(&dl, |s| s.done_bytes >= 512 * KIB, TIMEOUT).await;
    a.set_version(1);
    // Its running responses end, so its next requests carry `If-Range`.
    a.set_cfg(|c| c.fail_status = Some(503));
    tokio::time::sleep(Duration::from_millis(50)).await;
    a.set_cfg(|c| c.fail_status = None);
    wait_status(
        &dl,
        |s| {
            s.state
                == DownloadState::NeedsAttention {
                    reason: AttentionReason::RemoteChanged,
                }
        },
        TIMEOUT,
    )
    .await;
}

/// A mirror's expired redirect target is re-resolved on its own, mid-download, and it
/// gets connections again at its new URL. (That the other location's connections are
/// not stopped meanwhile is checked by a unit test in `transfer.rs`.)
#[tokio::test]
async fn mirror_reresolves_without_stopping_the_others() {
    let a = server(|c| c.bytes_per_sec = Some(64 * KIB)).await;
    let b = server(|c| c.bytes_per_sec = Some(512 * KIB)).await;
    let dir = tempfile::tempdir().unwrap();
    let mut req = request(&a, &[], dir.path());
    req.mirrors = vec![Mirror::new(b.url("/expiring"))];
    // Small enough that the fast mirror steals ranges from the slow primary.
    let mut c = cfg(dir.path());
    c.min_segment_bytes = 64 * KIB;
    let (dl, added) = start(req, c).await;
    wait_status(
        &dl,
        |_| mirrors(&dl).iter().all(|m| m.connections > 0),
        TIMEOUT,
    )
    .await;
    // Its next request (once one of its ranges is done) is refused and re-resolved.
    b.rotate_signed();
    let deadline = Instant::now() + TIMEOUT;
    while !b.paths_seen().iter().any(|p| p == "/signed/1") {
        assert!(
            Instant::now() < deadline,
            "{:?} {:?} {:?}",
            b.paths_seen(),
            dl.status(),
            dl.detail()
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    wait_status(&dl, |_| mirrors(&dl)[1].connections > 0, TIMEOUT).await;
    a.set_cfg(|c| c.bytes_per_sec = None);
    b.set_cfg(|c| c.bytes_per_sec = None);
    wait_completed(&dl).await;
    assert_file(&added.path, LEN, 0);
}

/// A mirror that cannot be checked on resume (it is down for now) is benched, not
/// dropped: it is checked and used again once it answers.
#[tokio::test]
async fn mirror_down_on_resume_is_benched_and_comes_back() {
    let a = server(|c| c.bytes_per_sec = Some(256 * KIB)).await;
    let b = server(|c| c.bytes_per_sec = Some(256 * KIB)).await;
    let dir = tempfile::tempdir().unwrap();
    let req = request(&a, &[&b], dir.path());
    let (dl, added) = start(req.clone(), cfg(dir.path())).await;
    wait_status(&dl, |s| s.done_bytes >= MIB, TIMEOUT).await;
    dl.pause().await;
    drop(dl);
    b.set_cfg(|c| c.fail_status = Some(503));
    // The primary alone needs 3 s for the rest (8 connections at 128 KiB/s each), so
    // the mirror's bench runs out and it is checked again well before the end.
    a.set_cfg(|c| c.bytes_per_sec = Some(128 * KIB));
    let dl = restore(req, &added.path, cfg(dir.path())).await;
    let shown = mirrors(&dl);
    assert!(
        matches!(&shown[1].state, MirrorState::Benched { reason, .. } if reason.contains("503")),
        "{shown:?}"
    );
    let b_before = b.range_requests();
    b.set_cfg(|c| {
        c.fail_status = None;
        c.bytes_per_sec = None;
    });
    wait_completed(&dl).await;
    assert_file(&added.path, LEN, 0);
    assert!(b.range_requests() > b_before, "the mirror never came back");
}
