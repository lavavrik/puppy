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
    assert_eq!(got.document, Some(s.url("/a.meta4")));
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
