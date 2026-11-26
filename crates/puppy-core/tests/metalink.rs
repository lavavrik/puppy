//! Fetching metalinks over HTTP: the Accept header, the content type, the size cap.

use puppy_core::config::HttpConfig;
use puppy_core::metalink::{ACCEPT, Location, MAX_DOCUMENT_BYTES, load};
use puppy_core::types::RequestContext;
use puppy_testserver::{ServerCfg, TestServer};

const DOC: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<metalink xmlns="urn:ietf:params:xml:ns:metalink">
  <file name="f.bin"><size>3</size><url>http://127.0.0.1:9/f.bin</url></file>
</metalink>"#;

#[tokio::test]
async fn fetched_metalinks() {
    let s = TestServer::start(ServerCfg {
        len: 2 * MAX_DOCUMENT_BYTES as u64,
        ..ServerCfg::default()
    })
    .await;
    let cfg = HttpConfig::default();
    let ctx = RequestContext {
        user_agent: Some("tester".into()),
        ..RequestContext::default()
    };
    s.set_doc("/a.meta4", "application/metalink4+xml", DOC);
    let got = load(&Location::Url(s.url("/a.meta4")), &cfg, &ctx)
        .await
        .unwrap();
    assert_eq!(got.metalink.files[0].path.to_string(), "f.bin");
    let doc = got.document.unwrap();
    assert_eq!(doc.requested, s.url("/a.meta4"));
    assert_eq!(doc.final_url, s.url("/a.meta4"));
    let req = s.requests_seen().pop().unwrap();
    assert_eq!(req["accept"], ACCEPT);
    assert_eq!(req["user-agent"], "tester");

    // The content type does not matter when the body is a metalink.
    s.set_doc("/b", "application/octet-stream", DOC);
    assert!(load(&Location::Url(s.url("/b")), &cfg, &ctx).await.is_ok());

    // An HTML error page says what it is.
    s.set_doc("/c.meta4", "text/html", "<html><body>gone</body></html>");
    let e = load(&Location::Url(s.url("/c.meta4")), &cfg, &ctx)
        .await
        .unwrap_err();
    assert_eq!(e, "not a metalink: the server sent text/html");

    // A metalink label on something else: the parser's error.
    s.set_doc("/d.meta4", "application/metalink4+xml", "<metalink");
    let e = load(&Location::Url(s.url("/d.meta4")), &cfg, &ctx)
        .await
        .unwrap_err();
    assert!(e.starts_with("not valid XML"), "{e}");

    let e = load(&Location::Url(s.url("/missing.meta4")), &cfg, &ctx)
        .await
        .unwrap_err();
    assert!(e.contains("HTTP 404"), "{e}");

    // Too big: refused from its Content-Length, before reading it.
    s.set_doc(
        "/big.meta4",
        "application/metalink4+xml",
        vec![b' '; MAX_DOCUMENT_BYTES + 1],
    );
    let e = load(&Location::Url(s.url("/big.meta4")), &cfg, &ctx)
        .await
        .unwrap_err();
    assert!(e.contains("larger than"), "{e}");
    // A server that ignores Accept and sends the (large, unlabelled) file itself.
    let e = load(&Location::Url(s.url("/file.bin")), &cfg, &ctx)
        .await
        .unwrap_err();
    assert!(e.contains("larger than"), "{e}");

    // Without a Content-Length: cut off while reading.
    s.set_cfg(|c| c.content_length = false);
    let e = load(&Location::Url(s.url("/file.bin")), &cfg, &ctx)
        .await
        .unwrap_err();
    assert!(e.contains("larger than"), "{e}");
}

/// A redirect to another site: the user's cookies, credentials, referer and headers go
/// to the URL they gave only, and the files of the list that site serves get none of
/// them, whatever their hosts.
#[tokio::test]
async fn redirected_fetches_keep_credentials_home() {
    let b = TestServer::start(ServerCfg::default()).await;
    let a = TestServer::start(ServerCfg {
        redirect_to: Some(b.url("/f.meta4").to_string()),
        ..ServerCfg::default()
    })
    .await;
    let listed = format!(
        r#"<metalink xmlns="urn:ietf:params:xml:ns:metalink">
             <file name="f.iso"><url>{}</url></file>
             <file name="g.iso"><url>{}</url></file></metalink>"#,
        b.url("/f.iso"),
        a.url("/g.iso")
    );
    b.set_doc("/f.meta4", "application/metalink4+xml", listed);
    let ctx = RequestContext {
        cookies: Some("sid=X".into()),
        headers: vec![
            ("Authorization".into(), "Bearer T".into()),
            ("X-Custom".into(), "c".into()),
        ],
        referer: Some("https://a.example/".into()),
        user_agent: Some("tester".into()),
        ..RequestContext::default()
    };
    let got = load(&Location::Url(a.url("/away")), &HttpConfig::default(), &ctx)
        .await
        .unwrap();
    let to_a = a.requests_seen().pop().unwrap();
    assert_eq!(to_a["cookie"], "sid=X");
    assert_eq!(to_a["authorization"], "Bearer T");
    let to_b = b.requests_seen().pop().unwrap();
    for h in ["cookie", "authorization", "x-custom", "referer"] {
        assert!(to_b.get(h).is_none(), "{h} reached the redirect target");
    }
    assert_eq!(to_b["user-agent"], "tester");
    let doc = got.document.clone().unwrap();
    assert_eq!(doc.requested, a.url("/away"));
    assert_eq!(doc.final_url, b.url("/f.meta4"));
    for f in &got.metalink.files {
        let c = got.request(f, None, &ctx).context;
        assert_eq!(c.cookies, None, "{}", f.path);
        assert!(c.headers.is_empty(), "{}", f.path);
        assert_eq!(c.referer, None);
        assert_eq!(c.user_agent.as_deref(), Some("tester"));
    }
}
