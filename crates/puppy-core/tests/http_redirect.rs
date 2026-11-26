//! What each redirect hop of a download gets of the user's request context: everything on
//! the origin of the URL the user gave, and on any other origin only `Accept`,
//! `Accept-Language`, the user agent and the referer's origin. Each server is its own
//! origin (another port).

use puppy_core::config::HttpConfig;
use puppy_core::engine::{AddOptions, Added};
use puppy_core::http::download::HttpDownload;
use puppy_core::http::probe::{build_client, probe};
use puppy_core::limiter::TokenBucket;
use puppy_core::metalink::{Location, load};
use puppy_core::types::{AddRequest, DownloadState, DownloadStatus, RequestContext, Source};
use puppy_testserver::{ServerCfg, TestServer, body_byte};
use std::path::Path;
use std::time::{Duration, Instant};
use tokio::sync::broadcast;

const KIB: u64 = 1024;
const MIB: u64 = 1024 * 1024;
const LEN: u64 = 4 * MIB;
const TIMEOUT: Duration = Duration::from_secs(20);
const RT: Duration = Duration::from_secs(30);

fn cfg(dir: &Path) -> HttpConfig {
    HttpConfig {
        backoff_base_ms: 10,
        backoff_max_ms: 100,
        flush_interval_ms: 100,
        default_dir: dir.to_path_buf(),
        ..HttpConfig::default()
    }
}

/// Everything a user (or the browser extension) can give for a download. The referer is
/// a plain-HTTP page: the test servers are plain HTTP, where an HTTPS page's referer
/// never goes on another origin (a unit test of `RequestContext::for_target` checks that).
fn user_context() -> RequestContext {
    RequestContext {
        headers: vec![
            ("X-Api-Key".into(), "secret".into()),
            ("X-Other".into(), "other".into()),
            ("Authorization".into(), "Bearer hdr".into()),
            ("Proxy-Authorization".into(), "Basic proxy".into()),
            ("Cookie".into(), "hdr=cookie".into()),
            ("Accept".into(), "application/octet-stream".into()),
            ("Accept-Language".into(), "uk".into()),
        ],
        cookies: Some("session=topsecret".into()),
        referer: Some("http://example.com/page?token=t".into()),
        user_agent: Some("TestAgent/1".into()),
        basic_auth: Some(("user".into(), Some("hunter2".into()))),
        ssh_agent: None,
    }
}

/// A request on the origin the user gave: it carries their whole context.
fn assert_full(path: &str, h: &http::HeaderMap) {
    assert_eq!(h.get("x-api-key").unwrap(), "secret", "{path}: {h:?}");
    assert_eq!(h.get("x-other").unwrap(), "other", "{path}");
    let cookies: Vec<_> = h.get_all("cookie").iter().collect();
    assert_eq!(cookies, ["hdr=cookie", "session=topsecret"], "{path}");
    assert_eq!(h.get_all("authorization").iter().count(), 2, "{path}");
    assert!(h.get("proxy-authorization").is_some(), "{path}");
    assert_eq!(
        h.get("referer").unwrap(),
        "http://example.com/page?token=t",
        "{path}"
    );
    assert_eq!(h.get("user-agent").unwrap(), "TestAgent/1", "{path}");
}

/// A request on another origin: only the harmless headers and the referer's origin.
fn assert_stripped(path: &str, h: &http::HeaderMap) {
    for name in [
        "x-api-key",
        "x-other",
        "cookie",
        "authorization",
        "proxy-authorization",
    ] {
        assert!(h.get(name).is_none(), "{path}: {name} leaked: {h:?}");
    }
    assert_eq!(
        h.get("accept").unwrap(),
        "application/octet-stream",
        "{path}"
    );
    assert_eq!(h.get("accept-language").unwrap(), "uk", "{path}");
    assert_eq!(h.get("user-agent").unwrap(), "TestAgent/1", "{path}");
    assert_eq!(h.get("referer").unwrap(), "http://example.com/", "{path}");
}

async fn server(f: impl FnOnce(&mut ServerCfg)) -> TestServer {
    let mut c = ServerCfg {
        len: LEN,
        bytes_per_sec: Some(256 * KIB),
        ..ServerCfg::default()
    };
    f(&mut c);
    TestServer::start(c).await
}

fn request(url: url::Url, dir: &Path) -> AddRequest {
    AddRequest {
        source: Source::Http { url },
        output_dir: Some(dir.to_path_buf()),
        filename: None,
        context: user_context(),
        files: Default::default(),
        seed: None,
        checksum: None,
        mirrors: Vec::new(),
        size: None,
    }
}

async fn start(req: AddRequest, cfg: HttpConfig) -> (HttpDownload, Added) {
    let (tx, _) = broadcast::channel(256);
    HttpDownload::start_fresh(
        1,
        req,
        AddOptions::default(),
        build_client(&cfg).unwrap(),
        cfg,
        TokenBucket::new(None),
        tx,
    )
    .await
    .unwrap()
}

async fn restore(req: AddRequest, final_path: &Path, cfg: HttpConfig) -> HttpDownload {
    let (tx, _) = broadcast::channel(256);
    HttpDownload::restore(
        2,
        req,
        final_path,
        AddOptions::default(),
        build_client(&cfg).unwrap(),
        cfg,
        TokenBucket::new(None),
        tx,
    )
    .await
    .unwrap()
    .0
}

async fn wait_status(dl: &HttpDownload, pred: impl Fn(&DownloadStatus) -> bool) -> DownloadStatus {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        let st = dl.status();
        if pred(&st) {
            return st;
        }
        assert!(Instant::now() < deadline, "timed out; last status {st:?}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

async fn wait_completed(dl: &HttpDownload) {
    wait_status(dl, |s| matches!(s.state, DownloadState::Completed)).await;
}

fn assert_file(path: &Path, len: u64) {
    let data = std::fs::read(path).unwrap();
    assert_eq!(data.len() as u64, len);
    assert!(
        data.iter()
            .enumerate()
            .all(|(i, b)| *b == body_byte(i as u64, 0))
    );
}

/// Checks every request `s` received with `check`, and that there were some.
fn each_request(s: &TestServer, check: fn(&str, &http::HeaderMap)) {
    let seen = s.seen();
    assert!(!seen.is_empty(), "no request reached the server");
    for (path, h) in &seen {
        check(path, h);
    }
}

/// a → b: the probe's hops to b get none of the user's secrets.
#[tokio::test]
async fn cross_origin_hop_gets_only_harmless_headers() {
    let a = TestServer::start(ServerCfg::default()).await;
    let b = TestServer::start(ServerCfg::default()).await;
    a.set_redirect("/away", b.url("/file.bin").to_string());
    let info = probe(
        &build_client(&HttpConfig::default()).unwrap(),
        &a.url("/away"),
        &user_context(),
        RT,
    )
    .await
    .unwrap();
    assert_eq!(info.final_url, b.url("/file.bin"));
    each_request(&a, assert_full);
    each_request(&b, assert_stripped);
}

/// a → b → a: back on the origin the user gave, the whole context again.
#[tokio::test]
async fn returning_to_the_origin_restores_the_context() {
    let a = TestServer::start(ServerCfg::default()).await;
    let b = TestServer::start(ServerCfg::default()).await;
    a.set_redirect("/away", b.url("/back").to_string());
    b.set_redirect("/back", a.url("/file.bin").to_string());
    let info = probe(
        &build_client(&HttpConfig::default()).unwrap(),
        &a.url("/away"),
        &user_context(),
        RT,
    )
    .await
    .unwrap();
    assert_eq!(info.final_url, a.url("/file.bin"));
    let paths = a.paths_seen();
    assert!(paths.iter().any(|p| p == "/file.bin"), "{paths:?}");
    each_request(&a, assert_full);
    each_request(&b, assert_stripped);
}

/// a → b → c: every hop off the user's origin is stripped, judged against a (not the
/// previous hop).
#[tokio::test]
async fn every_hop_off_the_origin_is_stripped() {
    let a = TestServer::start(ServerCfg::default()).await;
    let b = TestServer::start(ServerCfg::default()).await;
    let c = TestServer::start(ServerCfg::default()).await;
    a.set_redirect("/away", b.url("/on").to_string());
    b.set_redirect("/on", c.url("/file.bin").to_string());
    let info = probe(
        &build_client(&HttpConfig::default()).unwrap(),
        &a.url("/away"),
        &user_context(),
        RT,
    )
    .await
    .unwrap();
    assert_eq!(info.final_url, c.url("/file.bin"));
    each_request(&a, assert_full);
    each_request(&b, assert_stripped);
    each_request(&c, assert_stripped);
}

/// Same-origin hops keep everything, on the probe and on every segment request.
#[tokio::test]
async fn same_origin_download_keeps_everything() {
    let a = server(|c| c.bytes_per_sec = None).await;
    a.set_redirect("/hop", a.url("/redirect").to_string());
    let dir = tempfile::tempdir().unwrap();
    let (dl, added) = start(request(a.url("/hop"), dir.path()), cfg(dir.path())).await;
    wait_completed(&dl).await;
    assert_file(&added.path, LEN);
    assert!(a.range_requests() >= 1);
    each_request(&a, assert_full);
}

/// A download whose segments go to another origin, whose final URL expires mid-download:
/// the re-resolution starts over at the user's URL with the whole context, and every
/// request to the other origin (probe hops, segments) is stripped.
#[tokio::test]
async fn reresolving_an_expired_cross_origin_redirect_mid_download() {
    let a = server(|_| {}).await;
    // One connection, dropped once at 768 KiB, after the signed URL it was redirected
    // to has expired: the reconnect gets 403.
    let b = server(|c| {
        c.len = 2 * MIB;
        c.bytes_per_sec = Some(MIB);
        c.fail_after_bytes_once = Some(768 * KIB);
    })
    .await;
    a.set_redirect("/away", b.url("/expiring").to_string());
    let dir = tempfile::tempdir().unwrap();
    let mut c = cfg(dir.path());
    c.connections = 1;
    let (dl, added) = start(request(a.url("/away"), dir.path()), c).await;
    let st = wait_status(&dl, |s| s.done_bytes >= 128 * KIB).await;
    assert!(st.done_bytes < 768 * KIB, "too late to expire: {st:?}");
    b.rotate_signed();
    wait_completed(&dl).await;
    assert_file(&added.path, 2 * MIB);
    let paths = b.paths_seen();
    assert!(paths.iter().any(|p| p == "/signed/1"), "{paths:?}");
    let probes = a.paths_seen().iter().filter(|p| *p == "/away").count();
    assert!(probes >= 2, "{:?}", a.paths_seen());
    each_request(&a, assert_full);
    each_request(&b, assert_stripped);
}

/// The same on a restore: validating the stored final URL is refused, so it is
/// re-resolved from the user's URL.
#[tokio::test]
async fn reresolving_an_expired_cross_origin_redirect_on_restore() {
    let a = server(|_| {}).await;
    let b = server(|_| {}).await;
    a.set_redirect("/away", b.url("/expiring").to_string());
    let dir = tempfile::tempdir().unwrap();
    let req = request(a.url("/away"), dir.path());
    let (dl, added) = start(req.clone(), cfg(dir.path())).await;
    wait_status(&dl, |s| s.done_bytes >= MIB).await;
    dl.pause().await;
    drop(dl);
    b.rotate_signed();
    b.set_cfg(|c| c.bytes_per_sec = None);
    let dl = restore(req, &added.path, cfg(dir.path())).await;
    wait_completed(&dl).await;
    assert_file(&added.path, LEN);
    let paths = b.paths_seen();
    assert!(paths.iter().any(|p| p == "/signed/1"), "{paths:?}");
    let probes = a.paths_seen().iter().filter(|p| *p == "/away").count();
    assert!(probes >= 2, "{:?}", a.paths_seen());
    each_request(&a, assert_full);
    each_request(&b, assert_stripped);
}

/// A metalink fetch: a hop off the user's origin gets the user agent alone (as a mirror
/// on another origin does), and the origin gets everything again when the document is
/// redirected back to it.
#[tokio::test]
async fn metalink_fetch_through_another_origin() {
    let a = TestServer::start(ServerCfg::default()).await;
    let b = TestServer::start(ServerCfg::default()).await;
    a.set_redirect("/away", b.url("/back").to_string());
    b.set_redirect("/back", a.url("/f.meta4").to_string());
    a.set_doc(
        "/f.meta4",
        "application/metalink4+xml",
        format!(
            r#"<metalink xmlns="urn:ietf:params:xml:ns:metalink">
                 <file name="f.iso"><url>{}</url></file></metalink>"#,
            a.url("/f.iso")
        ),
    );
    let ctx = user_context();
    let got = load(&Location::Url(a.url("/away")), &HttpConfig::default(), &ctx)
        .await
        .unwrap();
    assert_eq!(got.document.as_ref().unwrap().final_url, a.url("/f.meta4"));
    each_request(&a, assert_full);
    for (path, h) in b.seen() {
        for name in [
            "x-api-key",
            "x-other",
            "cookie",
            "authorization",
            "proxy-authorization",
            "referer",
            "accept-language",
        ] {
            assert!(h.get(name).is_none(), "{path}: {name} leaked: {h:?}");
        }
        assert_eq!(h["user-agent"], "TestAgent/1");
        assert!(h["accept"].to_str().unwrap().contains("metalink"), "{h:?}");
    }
    assert_eq!(b.paths_seen(), ["/back"]);
}
