mod support;

use puppy_core::config::HttpConfig;
use puppy_core::http::probe::{apply_context, build_client, probe};
use puppy_core::types::RequestContext;
use support::{ServerCfg, TestServer};

const MIB4: u64 = 4 * 1024 * 1024;
const RT: std::time::Duration = std::time::Duration::from_secs(30);

fn client() -> reqwest::Client {
    build_client(&HttpConfig::default()).unwrap()
}

#[tokio::test]
async fn probe_basic() {
    let s = TestServer::start(ServerCfg::default()).await;
    let info = probe(
        &client(),
        &s.url("/file.bin"),
        &RequestContext::default(),
        RT,
    )
    .await
    .unwrap();
    assert_eq!(info.total, Some(MIB4));
    assert!(info.accepts_ranges);
    assert_eq!(info.etag.as_deref(), Some("\"v0\""));
    assert!(info.etag_strong);
    assert!(info.last_modified.is_some());
    assert_eq!(info.filename, "file.bin");
}

#[tokio::test]
async fn probe_follows_redirect() {
    let s = TestServer::start(ServerCfg::default()).await;
    let info = probe(
        &client(),
        &s.url("/redirect"),
        &RequestContext::default(),
        RT,
    )
    .await
    .unwrap();
    assert_eq!(info.final_url.path(), "/file.bin");
    assert_eq!(info.filename, "file.bin");
}

#[tokio::test]
async fn probe_head_not_allowed_falls_back() {
    let s = TestServer::start(ServerCfg {
        head_allowed: false,
        ..Default::default()
    })
    .await;
    let info = probe(
        &client(),
        &s.url("/file.bin"),
        &RequestContext::default(),
        RT,
    )
    .await
    .unwrap();
    assert_eq!(info.total, Some(MIB4));
    assert!(info.accepts_ranges);
    assert_eq!(s.range_requests(), 1);
}

#[tokio::test]
async fn probe_head_without_length_falls_back() {
    let s = TestServer::start(ServerCfg {
        content_length: false,
        ..Default::default()
    })
    .await;
    let info = probe(
        &client(),
        &s.url("/file.bin"),
        &RequestContext::default(),
        RT,
    )
    .await
    .unwrap();
    assert_eq!(info.total, Some(MIB4));
    assert!(info.accepts_ranges);
}

#[tokio::test]
async fn probe_no_ranges() {
    let s = TestServer::start(ServerCfg {
        ranges: false,
        ..Default::default()
    })
    .await;
    let info = probe(
        &client(),
        &s.url("/file.bin"),
        &RequestContext::default(),
        RT,
    )
    .await
    .unwrap();
    assert!(!info.accepts_ranges);
    assert_eq!(info.total, Some(MIB4));
}

#[tokio::test]
async fn probe_disposition_filename() {
    for (d, want) in [
        (
            "attachment; filename*=UTF-8''r%C3%A9sum%C3%A9.pdf",
            "résumé.pdf",
        ),
        ("attachment; filename=\"../../x.sh\"", "x.sh"),
    ] {
        let s = TestServer::start(ServerCfg {
            disposition: Some(d.to_string()),
            ..Default::default()
        })
        .await;
        let info = probe(
            &client(),
            &s.url("/file.bin"),
            &RequestContext::default(),
            RT,
        )
        .await
        .unwrap();
        assert_eq!(info.filename, want);
    }
}

#[tokio::test]
async fn probe_sends_context() {
    let s = TestServer::start(ServerCfg::default()).await;
    let ctx = RequestContext {
        headers: vec![("X-Test".into(), "1".into())],
        cookies: Some("a=b".into()),
        referer: Some("http://example.com/page".into()),
        ..Default::default()
    };
    probe(&client(), &s.url("/file.bin"), &ctx, RT)
        .await
        .unwrap();
    let seen = s.requests_seen();
    assert!(!seen.is_empty());
    let h = &seen[0];
    assert_eq!(h.get("x-test").unwrap(), "1");
    assert_eq!(h.get("cookie").unwrap(), "a=b");
    assert_eq!(h.get("referer").unwrap(), "http://example.com/page");
    assert_eq!(h.get("accept-encoding").unwrap(), "identity");
    assert!(
        h.get("user-agent")
            .unwrap()
            .to_str()
            .unwrap()
            .starts_with("puppy/")
    );
}

#[tokio::test]
async fn context_user_agent_overrides_default() {
    let s = TestServer::start(ServerCfg::default()).await;
    let ctx = RequestContext {
        user_agent: Some("custom/1".into()),
        ..Default::default()
    };
    apply_context(client().get(s.url("/file.bin")), &ctx)
        .send()
        .await
        .unwrap();
    assert_eq!(s.requests_seen()[0].get("user-agent").unwrap(), "custom/1");
}

#[tokio::test]
async fn server_range_and_if_range() {
    let s = TestServer::start(ServerCfg {
        len: 1000,
        ..Default::default()
    })
    .await;
    let c = client();
    let r = c
        .get(s.url("/file.bin"))
        .header("Range", "bytes=10-19")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 206);
    assert_eq!(r.headers()["content-range"], "bytes 10-19/1000");
    let b = r.bytes().await.unwrap();
    assert_eq!(b.len(), 10);
    assert_eq!(b[0], support::body_byte(10, 0));
    // Stale If-Range -> full body.
    let r = c
        .get(s.url("/file.bin"))
        .header("Range", "bytes=10-")
        .header("If-Range", "\"v9\"")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(r.bytes().await.unwrap().len(), 1000);
    // Matching If-Range honoured; open-ended range.
    let r = c
        .get(s.url("/file.bin"))
        .header("Range", "bytes=990-")
        .header("If-Range", "\"v0\"")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 206);
    assert_eq!(r.bytes().await.unwrap().len(), 10);
}

#[tokio::test]
async fn server_fails_once_mid_body() {
    let s = TestServer::start(ServerCfg {
        len: 100_000,
        fail_after_bytes_once: Some(40_000),
        ..Default::default()
    })
    .await;
    let c = client();
    let first = c
        .get(s.url("/file.bin"))
        .send()
        .await
        .unwrap()
        .bytes()
        .await;
    assert!(first.is_err());
    let c = client(); // fresh pool: the broken connection must not be reused
    let second = c
        .get(s.url("/file.bin"))
        .send()
        .await
        .unwrap()
        .bytes()
        .await;
    assert_eq!(second.unwrap().len(), 100_000);
}

#[tokio::test]
async fn probe_head_zero_length_falls_back() {
    let s = TestServer::start(ServerCfg {
        head_zero_length: true,
        ..Default::default()
    })
    .await;
    let info = probe(
        &client(),
        &s.url("/file.bin"),
        &RequestContext::default(),
        RT,
    )
    .await
    .unwrap();
    assert_eq!(info.total, Some(MIB4));
    assert!(info.accepts_ranges);
    assert_eq!(s.range_requests(), 1);
}

#[tokio::test]
async fn probe_empty_file_keeps_head_info() {
    let s = TestServer::start(ServerCfg {
        len: 0,
        ..Default::default()
    })
    .await;
    let info = probe(
        &client(),
        &s.url("/file.bin"),
        &RequestContext::default(),
        RT,
    )
    .await
    .unwrap();
    // The range fallback gets 416 for an empty file; HEAD's length 0 stands.
    assert_eq!(info.total, Some(0));
    assert_eq!(s.range_requests(), 1);
}

#[tokio::test]
async fn probe_head_zero_then_get_fails_is_unknown_length() {
    // HEAD's 0 is not confirmed by the fallback GET, so the size must be unknown.
    let s = TestServer::start(ServerCfg {
        head_zero_length: true,
        get_503_once: true,
        ..Default::default()
    })
    .await;
    let info = probe(
        &client(),
        &s.url("/file.bin"),
        &RequestContext::default(),
        RT,
    )
    .await
    .unwrap();
    assert_eq!(info.total, None);
}
