use super::*;
use sha2::Digest;

const OPENSUSE: &str = include_str!("testdata/opensuse.meta4");
const FEDORA: &str = include_str!("testdata/fedora.metalink");

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn urls(f: &MetaFile) -> Vec<String> {
    f.urls.iter().map(|m| m.url.to_string()).collect()
}

fn v4(files: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<metalink xmlns="urn:ietf:params:xml:ns:metalink">{files}</metalink>"#
    )
}

fn file_error(doc: &str) -> String {
    match parse(doc.as_bytes()) {
        Err(MetalinkError::File { why, .. }) => why,
        other => panic!("expected a file error, got {other:?}"),
    }
}

/// A MirrorBrain (openSUSE) `.meta4`: priorities, locations, the strongest hash,
/// rsync and the torrent skipped, pieces and the signature ignored.
#[test]
fn metalink4_mirrorbrain() {
    let m = parse(OPENSUSE.as_bytes()).unwrap();
    assert_eq!(m.version, Version::V4);
    assert_eq!(m.files.len(), 1);
    let f = &m.files[0];
    let iso = "openSUSE-Leap-15.6-NET-x86_64-Media.iso";
    assert_eq!(f.path.to_string(), iso);
    assert_eq!(f.size, Some(270532608));
    let c = f.checksum.as_ref().unwrap();
    assert_eq!(c.algo, HashAlgo::Sha256);
    assert_eq!(c.hex, hex(&sha2::Sha256::digest(iso)));
    let u = urls(f);
    assert_eq!(u.len(), 4, "{u:?}");
    assert!(
        u[0].starts_with("http://ftp.halifax.rwth-aachen.de/"),
        "{u:?}"
    );
    assert!(u[1].starts_with("https://ftp.fau.de/"), "{u:?}");
    assert!(u[2].starts_with("ftp://mirror.karneval.cz/"), "{u:?}");
    assert!(u[3].starts_with("http://mirror.nl.leaseweb.net/"), "{u:?}");
    assert_eq!(f.urls[0].priority, Some(1));
    assert_eq!(f.urls[3].priority, Some(5));
    assert_eq!(f.urls[2].location.as_deref(), Some("cz"));

    let req = f.request(Some(PathBuf::from("/dl")), &RequestContext::default(), None);
    assert_eq!(
        req.source,
        Source::Http {
            url: f.urls[0].url.clone()
        }
    );
    assert_eq!(req.mirrors, f.urls[1..]);
    assert_eq!(req.filename.as_deref(), Some(iso));
    assert_eq!(req.output_dir, Some(PathBuf::from("/dl")));
    assert_eq!(req.checksum, f.checksum);
    assert_eq!(req.size, Some(270532608));
}

/// A Fedora MirrorManager Metalink 3: `preference` (higher first) becomes priority,
/// `type="bittorrent"` and rsync are skipped, the hashes of `mm0:alternates` (older
/// versions of the file) are not the file's.
#[test]
fn metalink3_mirrormanager() {
    let m = parse(FEDORA.as_bytes()).unwrap();
    assert_eq!(m.version, Version::V3);
    let f = &m.files[0];
    assert_eq!(f.path.to_string(), "repomd.xml");
    assert_eq!(f.size, Some(7045));
    let c = f.checksum.as_ref().unwrap();
    assert_eq!(c.algo, HashAlgo::Sha512);
    assert_eq!(c.hex, hex(&sha2::Sha512::digest("current")));
    let u = urls(f);
    assert_eq!(u.len(), 5, "{u:?}");
    assert!(u[0].starts_with("https://ftp.fau.de/"), "{u:?}");
    assert!(u[1].starts_with("http://ftp.fau.de/"), "{u:?}");
    assert!(u[2].starts_with("https://mirror.karneval.cz/"), "{u:?}");
    assert!(u[3].starts_with("ftp://mirrors.kernel.org/"), "{u:?}");
    assert!(u[4].starts_with("https://dl.fedoraproject.org/"), "{u:?}");
    assert_eq!(f.urls[0].priority, Some(1));
    assert_eq!(f.urls[4].priority, Some(91));
    assert_eq!(f.urls[0].location.as_deref(), Some("de"));
}

/// Several files: each its own entry, subdirectories kept; priorities sort, ties
/// and unnumbered URLs keep document order, duplicates are dropped.
#[test]
fn multi_file_and_priorities() {
    let doc = v4(r#"
      <file name="a.bin">
        <url>http://h/none-1</url>
        <url priority="2">http://h/two</url>
        <url priority="1">http://h/one</url>
        <url priority="2">http://h/two-b</url>
        <url>http://h/none-2</url>
        <url priority="1">http://h/one</url>
        <url priority="0">http://h/zero-is-invalid</url>
      </file>
      <file name="sub/dir/b.bin"><size>5</size><url>ftpes://h/b</url></file>"#);
    let m = parse(doc.as_bytes()).unwrap();
    assert_eq!(m.files.len(), 2);
    assert_eq!(
        urls(&m.files[0]),
        [
            "http://h/one",
            "http://h/two",
            "http://h/two-b",
            "http://h/none-1",
            "http://h/none-2",
            "http://h/zero-is-invalid",
        ]
    );
    let b = &m.files[1];
    assert_eq!(b.path.to_string(), "sub/dir/b.bin");
    assert_eq!(b.path.dirs(), ["sub", "dir"]);
    assert_eq!(b.checksum, None);
    let req = b.request(
        Some(b.path.dir_in(Path::new("/dl"))),
        &RequestContext::default(),
        None,
    );
    assert_eq!(req.filename.as_deref(), Some("b.bin"));
    assert_eq!(
        req.output_dir,
        Some(Path::new("/dl").join("sub").join("dir"))
    );
    assert!(req.mirrors.is_empty());
}

/// At most MAX_LOCATIONS locations, the best ones.
#[test]
fn locations_are_capped_by_priority() {
    let many: String = (0..40)
        .rev()
        .map(|i| format!(r#"<url priority="{}">http://h/{i}</url>"#, i + 1))
        .collect();
    let m = parse(v4(&format!(r#"<file name="f">{many}</file>"#)).as_bytes()).unwrap();
    let u = urls(&m.files[0]);
    assert_eq!(u.len(), MAX_LOCATIONS);
    assert_eq!(u[0], "http://h/0");
    assert_eq!(
        u[MAX_LOCATIONS - 1],
        format!("http://h/{}", MAX_LOCATIONS - 1)
    );
}

/// Unknown hash types are skipped; a known type with a bad digest is an error.
#[test]
fn hash_types() {
    let sha1 = hex(&sha1::Sha1::digest("x"));
    let doc = v4(&format!(
        r#"<file name="f">
            <hash type="sha3-256">{}</hash>
            <hash type="ed2k">00</hash>
            <hash type="SHA-1">{sha1}</hash>
            <url>http://h/f</url></file>"#,
        "ab".repeat(32)
    ));
    let m = parse(doc.as_bytes()).unwrap();
    let c = m.files[0].checksum.as_ref().unwrap();
    assert_eq!((c.algo, c.hex.as_str()), (HashAlgo::Sha1, sha1.as_str()));

    let only_unknown =
        v4(r#"<file name="f"><hash type="tiger">00</hash><url>http://h/f</url></file>"#);
    assert_eq!(
        parse(only_unknown.as_bytes()).unwrap().files[0].checksum,
        None
    );

    let short =
        v4(r#"<file name="f"><hash type="sha-256">abcd</hash><url>http://h/f</url></file>"#);
    assert!(file_error(&short).contains("bad <hash>"));
}

/// Schemes no transport serves, torrents and hostless URLs are skipped; a file
/// with nothing left says why.
#[test]
fn unusable_urls() {
    let doc = v4(r#"<file name="f">
        <url>rsync://h/f</url><url>gopher://h/f</url><url>not a url</url><url>http://</url>
        <url>https://h/f</url></file>"#);
    assert_eq!(
        urls(&parse(doc.as_bytes()).unwrap().files[0]),
        ["https://h/f"]
    );

    // Whatever the transports serve is kept: FTP(S) and SFTP too.
    let doc = v4(
        r#"<file name="f"><url>ftps://h/f</url><url>ftpes://h/f</url>
        <url>sftp://u@h:2222/srv/f</url><url>scp://h/f</url></file>"#,
    );
    assert_eq!(
        urls(&parse(doc.as_bytes()).unwrap().files[0]),
        ["ftps://h/f", "ftpes://h/f", "sftp://u@h:2222/srv/f"]
    );

    let torrent =
        v4(r#"<file name="f"><metaurl mediatype="torrent">http://h/f.torrent</metaurl></file>"#);
    let why = file_error(&torrent);
    assert!(
        why.contains("no usable URL") && why.contains("torrent"),
        "{why}"
    );

    let rsync = v4(r#"<file name="f"><url>rsync://h/f</url><url>gopher://h/f</url></file>"#);
    let why = file_error(&rsync);
    assert!(why.contains("unsupported: gopher, rsync"), "{why}");

    let none = v4(r#"<file name="f"><size>1</size></file>"#);
    assert!(file_error(&none).contains("no usable URL"));
}

/// Names that could leave the output folder refuse the whole document; the error
/// names the file.
#[test]
fn malicious_names() {
    for name in [
        "../../.bashrc",
        "/etc/cron.d/x",
        "C:\\Windows\\x.dll",
        "\\\\host\\share\\x",
        "a/../../x",
        "a&#x0;b",
        "a&#10;b",
        "x/..",
        "",
    ] {
        let doc = v4(&format!(
            r#"<file name="ok.bin"><url>http://h/ok</url></file>
               <file name="{name}"><url>http://h/f</url></file>"#
        ));
        match parse(doc.as_bytes()) {
            Err(MetalinkError::File { file, why }) => {
                assert!(!why.is_empty(), "{name:?}");
                assert!(!file.contains('\n') && !file.contains('\0'), "{file:?}");
            }
            // `&#x0;` is not even XML.
            Err(MetalinkError::Xml(_)) => assert!(name.contains("&#x0;"), "{name:?}"),
            other => panic!("{name:?}: {other:?}"),
        }
    }
    // Repaired, not refused: Windows device names and reserved characters.
    let doc = v4(r#"<file name="aux/CON.txt"><url>http://h/f</url></file>
                   <file name="ab:c?.txt"><url>http://h/g</url></file>"#);
    let m = parse(doc.as_bytes()).unwrap();
    assert_eq!(m.files[0].path.to_string(), "_aux/_CON.txt");
    assert_eq!(m.files[1].path.to_string(), "ab_c_.txt");
    // Two names for one path (here only by case) would collide.
    let dup = v4(r#"<file name="d/A.bin"><url>http://h/1</url></file>
                   <file name="./d/a.bin"><url>http://h/2</url></file>"#);
    assert!(file_error(&dup).contains("same name"));
}

#[test]
fn doctype_and_entities_are_refused() {
    let laughs = r#"<?xml version="1.0"?>
<!DOCTYPE metalink [
  <!ENTITY lol "lol">
  <!ENTITY lol1 "&lol;&lol;&lol;&lol;&lol;&lol;&lol;&lol;&lol;&lol;">
  <!ENTITY lol2 "&lol1;&lol1;&lol1;&lol1;&lol1;&lol1;&lol1;&lol1;&lol1;&lol1;">
]>
<metalink xmlns="urn:ietf:params:xml:ns:metalink">
  <file name="&lol2;"><url>http://h/f</url></file>
</metalink>"#;
    assert_eq!(
        parse(laughs.as_bytes()).unwrap_err(),
        MetalinkError::Doctype
    );
    let xxe = r#"<?xml version="1.0"?>
<!DOCTYPE m [<!ENTITY xxe SYSTEM "file:///etc/passwd">]>
<metalink xmlns="urn:ietf:params:xml:ns:metalink"><file name="&xxe;"/></metalink>"#;
    assert_eq!(parse(xxe.as_bytes()).unwrap_err(), MetalinkError::Doctype);
    // Without the DOCTYPE the reference is just unknown.
    let bare = v4(r#"<file name="&xxe;"><url>http://h/f</url></file>"#);
    assert!(matches!(parse(bare.as_bytes()), Err(MetalinkError::Xml(m)) if m.contains("xxe")));
}

#[test]
fn size_and_count_limits() {
    let mut big = v4("").into_bytes();
    big.resize(MAX_DOCUMENT_BYTES + 1, b' ');
    assert_eq!(parse(&big).unwrap_err(), MetalinkError::TooBig);

    let files: String = (0..=MAX_FILES)
        .map(|i| format!(r#"<file name="f{i}"><url>http://h/{i}</url></file>"#))
        .collect();
    assert_eq!(
        parse(v4(&files).as_bytes()).unwrap_err(),
        MetalinkError::TooManyFiles
    );

    let urls: String = (0..=MAX_URLS)
        .map(|i| format!("<url>http://h/{i}</url>"))
        .collect();
    let why = file_error(&v4(&format!(r#"<file name="f">{urls}</file>"#)));
    assert!(why.contains("more than"), "{why}");

    assert!(
        file_error(&v4(
            r#"<file name="f"><size>-1</size><url>http://h/f</url></file>"#
        ))
        .contains("bad <size>")
    );
}

#[test]
fn not_a_metalink() {
    assert!(matches!(
        parse(b"<html><body>404</body></html>"),
        Err(MetalinkError::NotMetalink(_))
    ));
    assert!(matches!(
        parse(br#"<metalink xmlns="urn:example"/>"#),
        Err(MetalinkError::NotMetalink(_))
    ));
    assert_eq!(
        parse(v4("").as_bytes()).unwrap_err(),
        MetalinkError::NoFiles
    );
    assert!(matches!(parse(b"\xff\xfe<\0"), Err(MetalinkError::Xml(_))));
    assert!(matches!(
        parse(b"<metalink>\xff</metalink>"),
        Err(MetalinkError::Xml(_))
    ));
    // Elements of other namespaces are not Metalink's.
    let foreign = r#"<metalink xmlns="urn:ietf:params:xml:ns:metalink" xmlns:x="urn:x">
        <x:file name="f"><url>http://h/f</url></x:file></metalink>"#;
    assert_eq!(
        parse(foreign.as_bytes()).unwrap_err(),
        MetalinkError::NoFiles
    );
}

/// Without a namespace, the structure decides; a BOM is fine.
#[test]
fn unnamespaced_documents() {
    let v3 = "\u{feff}<metalink version=\"3.0\"><files><file name=\"f\"><resources>\
              <url type=\"http\" preference=\"50\">http://h/f</url></resources></file></files>\
              </metalink>";
    let m = parse(v3.as_bytes()).unwrap();
    assert_eq!(m.version, Version::V3);
    assert_eq!(m.files[0].urls[0].priority, Some(51));
    let v4 = r#"<metalink><file name="f"><url>http://h/f</url></file></metalink>"#;
    assert_eq!(parse(v4.as_bytes()).unwrap().version, Version::V4);
}

#[test]
fn detection() {
    assert!(has_metalink_extension("/a/b.META4"));
    assert!(has_metalink_extension("x.metalink"));
    assert!(!has_metalink_extension("x.meta4.iso"));
    assert!(is_metalink_media_type(
        "application/metalink4+xml; charset=utf-8"
    ));
    assert!(is_metalink_media_type("Application/Metalink+XML"));
    assert!(!is_metalink_media_type("application/xml"));
    assert!(looks_like_metalink(OPENSUSE.as_bytes()));
    assert!(looks_like_metalink(FEDORA.as_bytes()));
    assert!(looks_like_metalink(
        b"\xEF\xBB\xBF <!-- x --><m:metalink xmlns:m='x'/>"
    ));
    assert!(!looks_like_metalink(b"<?xml version='1.0'?><rss/>"));
    assert!(!looks_like_metalink(b"d8:announce"));
    assert!(!looks_like_metalink(b"<!-- unterminated"));

    let url = |s: &str| detect(s, false).unwrap();
    assert!(matches!(
        url("https://h/f.iso.meta4"),
        Some(Location::Url(_))
    ));
    assert!(matches!(
        url("http://h/f.metalink?x=1"),
        Some(Location::Url(_))
    ));
    assert_eq!(url("https://h/f.iso"), None);
    assert_eq!(url("ftp://h/f.meta4"), None);
    assert!(matches!(
        detect("https://h/f.iso", true),
        Ok(Some(Location::Url(_)))
    ));
    assert!(detect("ftp://h/f.meta4", true).is_err());
    assert!(detect("magnet:?xt=urn:btih:x", true).is_err());
    assert!(detect("/nonexistent/x.meta4", true).is_err());
    assert_eq!(url("/nonexistent/x.meta4"), None);

    let dir = tempfile::tempdir().unwrap();
    let write = |name: &str, body: &str| {
        let p = dir.path().join(name);
        std::fs::write(&p, body).unwrap();
        p.to_str().unwrap().to_string()
    };
    let named = write("a.meta4", "anything");
    assert_eq!(
        url(&named),
        Some(Location::Path(PathBuf::from(&named))),
        "by extension"
    );
    let sniffed = write("list", OPENSUSE);
    assert!(
        matches!(url(&sniffed), Some(Location::Path(_))),
        "by content"
    );
    let plain = write("notes.txt", "hello");
    assert_eq!(url(&plain), None);
    assert!(matches!(detect(&plain, true), Ok(Some(Location::Path(_)))));
    let torrent = write("x.torrent", OPENSUSE);
    assert_eq!(url(&torrent), None);
}

/// A context for the document's site keeps its cookies only for a primary there.
#[test]
fn request_context_follows_the_document_origin() {
    let doc = v4(r#"<file name="f"><url>https://mirror.example/f</url></file>
                   <file name="g"><url>https://dl.example/g</url></file>"#);
    let m = parse(doc.as_bytes()).unwrap();
    let ctx = RequestContext {
        cookies: Some("s=1".into()),
        user_agent: Some("ua".into()),
        ..RequestContext::default()
    };
    let origin = url::Url::parse("https://dl.example/list.meta4").unwrap();
    let other = m.files[0].request(None, &ctx, Some(&origin));
    assert_eq!(other.context.cookies, None);
    assert_eq!(other.context.user_agent.as_deref(), Some("ua"));
    let same = m.files[1].request(None, &ctx, Some(&origin));
    assert_eq!(same.context, ctx);
    let local = m.files[0].request(None, &ctx, None);
    assert_eq!(local.context, ctx);
}

#[test]
fn local_documents_are_read_capped() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("big.meta4");
    std::fs::write(&p, vec![b' '; MAX_DOCUMENT_BYTES + 10]).unwrap();
    let e = read_capped(&p).unwrap_err();
    assert!(e.to_string().contains("larger than"), "{e}");
}
