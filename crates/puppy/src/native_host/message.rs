//! The messages between the browser extension and the host (host protocol 1), and
//! how an `add` becomes the daemon's [`AddRequest`].

use percent_encoding::percent_decode_str;
use puppy_client::ClientError;
use puppy_core::types::{AddRequest, DownloadId, FileChoice, RequestContext, Source};
use puppy_proto::ErrorCode;
use serde::{Deserialize, Serialize};

/// The host↔extension protocol version (separate from the daemon protocol).
pub const HOST_PROTOCOL: u32 = 1;
/// Error messages are cut to this many characters.
pub const MAX_MESSAGE_CHARS: usize = 1000;
/// Longest request id (in bytes) the host echoes; a longer one is a `bad_message`.
pub const MAX_ID_BYTES: usize = 256;

/// A request from the extension: its id (echoed in the reply), the protocol it
/// speaks and what it asks. `protocol` may be missing only from a `hello` (so an
/// extension of any protocol learns the host's versions); [`parse`] refuses any other
/// request without it.
#[derive(Deserialize)]
pub struct Envelope {
    pub id: String,
    #[serde(default)]
    pub protocol: Option<u32>,
    #[serde(flatten)]
    pub body: Incoming,
}

/// The body's cookies and credentials are never printed (see [`AddMsg`]).
impl std::fmt::Debug for Envelope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Envelope")
            .field("id", &self.id)
            .field("protocol", &self.protocol)
            .field("body", &self.body)
            .finish()
    }
}

#[derive(Deserialize, Debug)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Incoming {
    Hello {
        /// Part of the schema; the host has no use for it.
        #[allow(dead_code)]
        extension_version: String,
    },
    Add(AddMsg),
    Remove {
        download_id: DownloadId,
        /// Delete the files puppy created for it. Absent (older extensions): false.
        #[serde(default)]
        delete_files: bool,
    },
}

/// A download the browser hands over. `final_url` (after redirects) is diagnostic
/// only: puppy fetches `url` and follows redirects itself.
#[derive(Deserialize, Default)]
pub struct AddMsg {
    pub url: String,
    #[allow(dead_code)]
    #[serde(default)]
    pub final_url: Option<String>,
    #[serde(default)]
    pub filename: Option<String>,
    #[serde(default)]
    pub referrer: Option<String>,
    #[serde(default)]
    pub cookies: Option<String>,
    #[serde(default)]
    pub user_agent: Option<String>,
}

/// Cookies and the passwords in URLs are never printed: a stray `{:?}` must not leak
/// them.
impl std::fmt::Debug for AddMsg {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        const HIDDEN: &str = "<redacted>";
        f.debug_struct("AddMsg")
            .field("url", &redact_password(&self.url))
            .field("final_url", &self.final_url.as_deref().map(redact_password))
            .field("filename", &self.filename)
            .field("referrer", &self.referrer.as_deref().map(redact_password))
            .field("cookies", &self.cookies.as_ref().map(|_| HIDDEN))
            .field("user_agent", &self.user_agent)
            .finish()
    }
}

/// `url` with any password replaced; an unparsable URL is hidden as a whole, since it
/// may still hold one.
fn redact_password(url: &str) -> String {
    match url::Url::parse(url) {
        Ok(mut u) => {
            if u.password().is_some() {
                let _ = u.set_password(Some("redacted"));
            }
            u.to_string()
        }
        Err(_) => "<unparsable URL>".to_string(),
    }
}

/// A reply to the extension; `id` is the request's, `None` when it could not be read.
#[derive(Serialize, Debug, PartialEq)]
pub struct Reply {
    pub id: Option<String>,
    #[serde(flatten)]
    pub body: Outgoing,
}

#[derive(Serialize, Debug, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Outgoing {
    Hello {
        protocol: u32,
        puppy_version: String,
        daemon_version: Option<String>,
        daemon_error: Option<String>,
    },
    Accepted {
        download_id: DownloadId,
    },
    Removed,
    Error {
        code: ErrorKind,
        message: String,
    },
}

#[derive(Serialize, Debug, PartialEq, Clone, Copy)]
#[serde(rename_all = "snake_case")]
pub enum ErrorKind {
    /// Puppy will not take this download (the browser keeps it).
    Refused,
    /// The daemon could not be reached or talked to.
    Unreachable,
    /// The request speaks another host protocol.
    ProtocolMismatch,
    /// The request could not be read.
    BadMessage,
}

/// Reads a request. `Err`: the request's id when it is readable, and why it failed.
/// An id over [`MAX_ID_BYTES`] is never echoed (`None`).
pub fn parse(bytes: &[u8]) -> Result<Envelope, (Option<String>, String)> {
    let env: Envelope = serde_json::from_slice(bytes).map_err(|e| {
        let id = serde_json::from_slice::<serde_json::Value>(bytes)
            .ok()
            .and_then(|v| v.get("id")?.as_str().map(str::to_owned))
            .filter(|id| id.len() <= MAX_ID_BYTES);
        (id, format!("unreadable message: {e}"))
    })?;
    if env.id.len() > MAX_ID_BYTES {
        return Err((
            None,
            format!("the request id is longer than {MAX_ID_BYTES} bytes"),
        ));
    }
    if env.protocol.is_none() && !matches!(env.body, Incoming::Hello { .. }) {
        return Err((
            Some(env.id),
            "unreadable message: missing field `protocol`".into(),
        ));
    }
    Ok(env)
}

/// The daemon request for a browser download: puppy's own download folder, the
/// browser's cookies, referrer and user agent, and `user:pass@` from the URL as basic
/// auth (removed from the URL so it is never shown). Only http(s) URLs are taken.
pub fn to_add_request(m: &AddMsg) -> Result<AddRequest, String> {
    let mut url = url::Url::parse(&m.url).map_err(|e| format!("invalid URL: {e}"))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(format!("unsupported URL scheme \"{}\"", url.scheme()));
    }
    let basic_auth = if !url.username().is_empty() || url.password().is_some() {
        let user = decode(url.username())?;
        let password = url.password().map(decode).transpose()?;
        // Cannot fail: an http(s) URL has a host.
        let _ = url.set_username("");
        let _ = url.set_password(None);
        Some((user, password))
    } else {
        None
    };
    Ok(AddRequest {
        source: Source::Http { url },
        output_dir: None,
        filename: m.filename.as_deref().and_then(basename),
        context: RequestContext {
            headers: Vec::new(),
            cookies: non_empty(&m.cookies),
            referer: non_empty(&m.referrer),
            user_agent: non_empty(&m.user_agent),
            basic_auth,
        },
        files: FileChoice::All,
        seed: None,
        checksum: None,
    })
}

fn decode(s: &str) -> Result<String, String> {
    percent_decode_str(s)
        .decode_utf8()
        .map(|s| s.into_owned())
        .map_err(|_| "the credentials in the URL are not UTF-8".to_string())
}

fn non_empty(s: &Option<String>) -> Option<String> {
    s.as_ref().filter(|s| !s.trim().is_empty()).cloned()
}

/// The file name at the end of a path the browser suggests (`/` or `\` separated),
/// trimmed; `None` when there is none (empty, `.`, `..`, a trailing separator).
pub fn basename(name: &str) -> Option<String> {
    let last = name.rsplit(['/', '\\']).next()?.trim();
    (!last.is_empty() && last != "." && last != "..").then(|| last.to_string())
}

/// How a daemon error is reported: the daemon refusing the request is `Refused`;
/// anything else (no daemon, a broken connection, an incompatible daemon) is
/// `Unreachable`.
pub fn error_kind(e: &ClientError) -> ErrorKind {
    match e {
        ClientError::Daemon { code, .. } if *code != ErrorCode::ProtocolMismatch => {
            ErrorKind::Refused
        }
        _ => ErrorKind::Unreachable,
    }
}

/// `message` cut to [`MAX_MESSAGE_CHARS`] characters.
pub fn cut(mut message: String) -> String {
    if let Some((i, _)) = message.char_indices().nth(MAX_MESSAGE_CHARS) {
        message.truncate(i);
    }
    message
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn msg(url: &str) -> AddMsg {
        AddMsg {
            url: url.into(),
            ..AddMsg::default()
        }
    }

    #[test]
    fn add_maps_to_add_request() {
        let m = AddMsg {
            url: "https://dl.example.com/a/b.iso?x=1".into(),
            final_url: Some("https://cdn.example.net/b.iso".into()),
            filename: Some("b.iso".into()),
            referrer: Some("https://example.com/page".into()),
            cookies: Some("sid=SECRET; t=2".into()),
            user_agent: Some("Mozilla/5.0 X".into()),
        };
        assert_eq!(
            to_add_request(&m).unwrap(),
            AddRequest {
                source: Source::Http {
                    url: "https://dl.example.com/a/b.iso?x=1".parse().unwrap()
                },
                output_dir: None,
                filename: Some("b.iso".into()),
                context: RequestContext {
                    headers: vec![],
                    cookies: Some("sid=SECRET; t=2".into()),
                    referer: Some("https://example.com/page".into()),
                    user_agent: Some("Mozilla/5.0 X".into()),
                    basic_auth: None,
                },
                files: FileChoice::All,
                seed: None,
                checksum: None,
            }
        );
    }

    #[test]
    fn userinfo_moves_to_basic_auth() {
        let cases = [
            (
                "https://us%40er:p%3Ass@h.example/f",
                ("us@er", Some("p:ss")),
            ),
            ("https://user@h.example/f", ("user", None)),
            ("https://:pw@h.example/f", ("", Some("pw"))),
        ];
        for (url, (user, pass)) in cases {
            let r = to_add_request(&msg(url)).unwrap();
            assert_eq!(
                r.source,
                Source::Http {
                    url: "https://h.example/f".parse().unwrap()
                },
                "{url}"
            );
            assert_eq!(
                r.context.basic_auth,
                Some((user.to_string(), pass.map(str::to_string))),
                "{url}"
            );
        }
    }

    #[test]
    fn non_http_is_refused() {
        for url in [
            "ftp://x/f",
            "blob:https://x/1",
            "data:,hi",
            "file:///etc/passwd",
        ] {
            let e = to_add_request(&msg(url)).unwrap_err();
            assert!(e.contains("unsupported"), "{url}: {e}");
        }
        let e = to_add_request(&msg("not a url")).unwrap_err();
        assert!(e.contains("invalid URL"), "{e}");
    }

    #[test]
    fn filename_is_reduced_to_a_name() {
        assert_eq!(basename("dir/sub/a.zip").as_deref(), Some("a.zip"));
        assert_eq!(
            basename(r"C:\Users\me\Downloads\a.zip").as_deref(),
            Some("a.zip")
        );
        assert_eq!(
            basename("отчёт 2024.pdf").as_deref(),
            Some("отчёт 2024.pdf")
        );
        for bad in ["", ".", "..", "a/", "   "] {
            assert_eq!(basename(bad), None, "{bad:?}");
        }
        let m = AddMsg {
            filename: Some("../../a.zip".into()),
            ..msg("https://h.example/f")
        };
        assert_eq!(
            to_add_request(&m).unwrap().filename.as_deref(),
            Some("a.zip")
        );
    }

    #[test]
    fn empty_strings_are_none() {
        let m = AddMsg {
            referrer: Some(String::new()),
            cookies: Some(String::new()),
            user_agent: Some(String::new()),
            filename: Some(String::new()),
            ..msg("https://h.example/f")
        };
        let r = to_add_request(&m).unwrap();
        assert_eq!(r.context, RequestContext::default());
        assert_eq!(r.filename, None);
    }

    #[test]
    fn debug_redacts_cookies_and_passwords() {
        let e = parse(
            br#"{"id":"r1","protocol":1,"type":"add","url":"https://u:PWSECRET@h/f","final_url":"https://u:PW2SECRET@c/f","referrer":"https://r:PW3SECRET@r/","cookies":"sid=COOKIESECRET"}"#,
        )
        .unwrap();
        let s = format!("{e:?}");
        for secret in ["PWSECRET", "PW2SECRET", "PW3SECRET", "COOKIESECRET"] {
            assert!(!s.contains(secret), "{s}");
        }
        assert!(s.contains("r1") && s.contains("h/f"), "{s}");
    }

    #[test]
    fn ids_are_capped() {
        let long = "x".repeat(MAX_ID_BYTES + 1);
        let ok = "x".repeat(MAX_ID_BYTES);
        let msg = |id: &str, ty: &str| {
            format!(r#"{{"id":"{id}","protocol":1,"type":"{ty}","download_id":1}}"#).into_bytes()
        };
        assert_eq!(parse(&msg(&ok, "remove")).unwrap().id, ok);
        let (id, why) = parse(&msg(&long, "remove")).unwrap_err();
        assert_eq!(id, None);
        assert!(why.contains("256"), "{why}");
        // Also when the message is unreadable for another reason.
        let (id, _) = parse(&msg(&long, "nope")).unwrap_err();
        assert_eq!(id, None);
    }

    #[test]
    fn protocol_may_be_missing_only_from_hello() {
        let e = parse(br#"{"id":"h1","type":"hello","extension_version":"9"}"#).unwrap();
        assert_eq!(e.protocol, None);
        let (id, why) = parse(br#"{"id":"r1","type":"remove","download_id":1}"#).unwrap_err();
        assert_eq!(id.as_deref(), Some("r1"));
        assert!(why.contains("protocol"), "{why}");
    }

    #[test]
    fn envelopes_and_replies() {
        let e = parse(br#"{"id":"r1","protocol":1,"type":"remove","download_id":7}"#).unwrap();
        assert_eq!(e.id, "r1");
        assert_eq!(e.protocol, Some(1));
        assert!(
            matches!(
                e.body,
                Incoming::Remove {
                    download_id: 7,
                    delete_files: false
                }
            ),
            "{e:?}"
        );
        let e = parse(
            br#"{"id":"r1","protocol":1,"type":"remove","download_id":7,"delete_files":true}"#,
        )
        .unwrap();
        assert!(
            matches!(
                e.body,
                Incoming::Remove {
                    download_id: 7,
                    delete_files: true
                }
            ),
            "{e:?}"
        );

        let e =
            parse(br#"{"id":"r2","protocol":1,"type":"add","url":"https://h/f","cookies":"a=b"}"#)
                .unwrap();
        assert!(
            matches!(&e.body, Incoming::Add(m) if m.url == "https://h/f" && m.cookies.as_deref() == Some("a=b")),
            "{e:?}"
        );

        let e = parse(br#"{"id":"r3","protocol":1,"type":"hello","extension_version":"0.1.0"}"#)
            .unwrap();
        assert!(
            matches!(&e.body, Incoming::Hello { extension_version } if extension_version == "0.1.0")
        );

        let (id, _) = parse(br#"{"id":"r1","protocol":1,"type":"nope"}"#).unwrap_err();
        assert_eq!(id.as_deref(), Some("r1"));
        let (id, _) = parse(b"not json").unwrap_err();
        assert_eq!(id, None);

        let reply = |body| {
            serde_json::to_value(Reply {
                id: Some("r1".into()),
                body,
            })
            .unwrap()
        };
        assert_eq!(
            reply(Outgoing::Accepted { download_id: 7 }),
            json!({"id": "r1", "type": "accepted", "download_id": 7})
        );
        assert_eq!(
            reply(Outgoing::Error {
                code: ErrorKind::ProtocolMismatch,
                message: "m".into()
            }),
            json!({"id": "r1", "type": "error", "code": "protocol_mismatch", "message": "m"})
        );
        assert_eq!(
            reply(Outgoing::Removed),
            json!({"id": "r1", "type": "removed"})
        );
        assert_eq!(
            serde_json::to_value(Reply {
                id: None,
                body: Outgoing::Error {
                    code: ErrorKind::BadMessage,
                    message: "m".into()
                }
            })
            .unwrap(),
            json!({"id": null, "type": "error", "code": "bad_message", "message": "m"})
        );
    }

    #[test]
    fn client_errors_map_to_kinds() {
        let daemon = |code| ClientError::Daemon {
            code,
            message: "m".into(),
        };
        assert_eq!(
            error_kind(&daemon(ErrorCode::BadRequest)),
            ErrorKind::Refused
        );
        assert_eq!(
            error_kind(&daemon(ErrorCode::ProtocolMismatch)),
            ErrorKind::Unreachable
        );
        for e in [
            ClientError::Io(std::io::ErrorKind::ConnectionRefused.into()),
            ClientError::Timeout,
            ClientError::Disconnected,
        ] {
            assert_eq!(error_kind(&e), ErrorKind::Unreachable, "{e:?}");
        }
    }

    #[test]
    fn messages_are_cut() {
        assert_eq!(cut("ab".into()), "ab");
        let long = "é".repeat(MAX_MESSAGE_CHARS + 5);
        assert_eq!(cut(long).chars().count(), MAX_MESSAGE_CHARS);
    }
}
