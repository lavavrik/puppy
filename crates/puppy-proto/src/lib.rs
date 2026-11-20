//! Versioned newline-delimited-JSON protocol spoken by the daemon and its clients.
//!
//! Evolution rules:
//! - New `Request` methods are additive. Servers must parse the envelope leniently (as
//!   `serde_json::Value` first, then [`parse_request`]) so an unknown method yields a
//!   `bad_request` reply carrying the right `req_id`.
//! - Every enum the daemon sends ([`Event`], [`ReplyData`], [`ErrorCode`],
//!   [`EngineKindTag`], [`WireState`], [`WireAttention`]) has an `Unknown` catch-all, so
//!   adding a variant to any of them is backward compatible: older clients decode it as
//!   `Unknown` instead of failing the whole message. A new wire enum must get one too.
//! - New *optional* fields (`#[serde(default)]`) are additive. A new *required* field, a
//!   removed field or a changed meaning of an existing value requires bumping
//!   [`PROTOCOL_VERSION`].
//! - The download state on the wire is [`WireState`], not puppy-core's `DownloadState`,
//!   which stays exhaustive for the engines and the daemon.
//! - A client that gets `protocol_mismatch` for its `hello` may send one `shutdown`
//!   (any `req_id`) on the same connection within 5 s to stop the incompatible daemon.

pub mod codec;

pub use codec::{CodecError, MsgReader, read_msg, write_msg};
use puppy_core::types::{AddRequest, AttentionReason, DownloadId, DownloadState, EngineDetail};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

pub const PROTOCOL_VERSION: u32 = 1;
pub const MAX_LINE: usize = 1 << 20;

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(tag = "method", rename_all = "snake_case")]
#[allow(clippy::large_enum_variant)] // wire type; boxing would change the public API
pub enum Request {
    Hello {
        client: String,
        protocol: u32,
    },
    Add {
        request: AddRequest,
        #[serde(default)]
        start_paused: bool,
        #[serde(default)]
        limit: Option<u64>,
    },
    List,
    Get {
        id: DownloadId,
    },
    Pause {
        id: DownloadId,
    },
    Resume {
        id: DownloadId,
    },
    Remove {
        id: DownloadId,
        delete_files: bool,
    },
    /// NeedsAttention/Failed: remove(true) + fresh add, same id.
    Restart {
        id: DownloadId,
    },
    /// Queue reorder.
    Move {
        id: DownloadId,
        position: usize,
    },
    SetLimit {
        id: DownloadId,
        bps: Option<u64>,
    },
    SetGlobalLimits {
        down: Option<u64>,
        up: Option<u64>,
    },
    SetMaxActive {
        n: u16,
    },
    GetSettings,
    Subscribe {
        interval_ms: u64,
    },
    Unsubscribe,
    Shutdown,
}

/// Client -> server: `Envelope<Request>`.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct Envelope<T> {
    pub req_id: u64,
    #[serde(flatten)]
    pub body: T,
}

/// Leniently parses a client line already decoded to a `Value`. On failure
/// returns the `req_id` (if one could be extracted) and a message, so the
/// server can send a `bad_request` reply.
pub fn parse_request(
    line_value: serde_json::Value,
) -> Result<Envelope<Request>, (Option<u64>, String)> {
    let req_id = line_value.get("req_id").and_then(serde_json::Value::as_u64);
    serde_json::from_value(line_value).map_err(|e| (req_id, e.to_string()))
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
#[allow(clippy::large_enum_variant)] // wire type; boxing would change the public API
pub enum ServerMsg {
    Response {
        req_id: u64,
        #[serde(flatten)]
        result: Reply,
    },
    Event {
        event: Event,
    },
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(tag = "status", rename_all = "snake_case")]
#[allow(clippy::large_enum_variant)] // wire type; boxing would change the public API
pub enum Reply {
    Ok { data: ReplyData },
    Err { code: ErrorCode, message: String },
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ReplyData {
    Hello {
        daemon_version: String,
        protocol: u32,
    },
    Added {
        id: DownloadId,
    },
    Downloads {
        downloads: Vec<DownloadSnapshot>,
    },
    Download {
        download: DownloadSnapshot,
        detail: Option<EngineDetail>,
    },
    Settings {
        settings: Settings,
    },
    Done,
    /// A kind this client does not know (newer daemon).
    #[serde(other)]
    Unknown,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    ProtocolMismatch,
    BadRequest,
    NotFound,
    InvalidState,
    Engine,
    Internal,
    /// A code this client does not know (newer daemon).
    #[serde(other)]
    Unknown,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Event {
    Snapshot {
        downloads: Vec<DownloadSnapshot>,
        totals: Totals,
    },
    Added {
        id: DownloadId,
    },
    Completed {
        id: DownloadId,
        name: String,
    },
    Failed {
        id: DownloadId,
        reason: String,
    },
    Removed {
        id: DownloadId,
    },
    NeedsAttention {
        id: DownloadId,
        reason: WireAttention,
    },
    /// A kind this client does not know (newer daemon).
    #[serde(other)]
    Unknown,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct DownloadSnapshot {
    pub id: DownloadId,
    pub kind: EngineKindTag,
    /// Display form: URL, magnet name or .torrent path.
    pub source: String,
    pub name: String,
    pub path: Option<PathBuf>,
    pub state: WireState,
    pub total_bytes: Option<u64>,
    pub done_bytes: u64,
    pub down_bps: u64,
    pub up_bps: u64,
    pub eta_secs: Option<u64>,
    pub limit: Option<u64>,
    pub queue_position: Option<usize>,
    /// Active segment count (http).
    pub connections: Option<u16>,
    pub peers: Option<u32>,
    /// When a transiently failed download is retried automatically (Unix seconds).
    #[serde(default)]
    pub next_retry_unix: Option<u64>,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EngineKindTag {
    Http,
    Torrent,
    /// A kind this client does not know (newer daemon).
    #[serde(other)]
    Unknown,
}

/// A download's state on the wire: puppy-core's `DownloadState` (same JSON) plus a
/// catch-all for states a newer daemon may add.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WireState {
    Queued,
    Starting,
    Active,
    Paused,
    Completed,
    Failed {
        reason: String,
        /// Retried automatically (see `DownloadSnapshot::next_retry_unix`).
        #[serde(default)]
        transient: bool,
    },
    NeedsAttention {
        reason: WireAttention,
    },
    /// A state this client does not know (newer daemon).
    #[serde(other)]
    Unknown,
}

/// Why a download needs attention, on the wire: puppy-core's `AttentionReason` (same
/// JSON) plus a catch-all.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WireAttention {
    RemoteChanged,
    NotResumable,
    ValidationFailed {
        detail: String,
    },
    /// A reason this client does not know (newer daemon).
    #[serde(other)]
    Unknown,
}

impl From<&AttentionReason> for WireAttention {
    fn from(r: &AttentionReason) -> Self {
        match r {
            AttentionReason::RemoteChanged => WireAttention::RemoteChanged,
            AttentionReason::NotResumable => WireAttention::NotResumable,
            AttentionReason::ValidationFailed { detail } => WireAttention::ValidationFailed {
                detail: detail.clone(),
            },
        }
    }
}

impl From<&DownloadState> for WireState {
    fn from(s: &DownloadState) -> Self {
        match s {
            DownloadState::Queued => WireState::Queued,
            DownloadState::Starting => WireState::Starting,
            DownloadState::Active => WireState::Active,
            DownloadState::Paused => WireState::Paused,
            DownloadState::Completed => WireState::Completed,
            DownloadState::Failed { reason, transient } => WireState::Failed {
                reason: reason.clone(),
                transient: *transient,
            },
            DownloadState::NeedsAttention { reason } => WireState::NeedsAttention {
                reason: reason.into(),
            },
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Default)]
pub struct Totals {
    pub down_bps: u64,
    pub up_bps: u64,
    pub active: usize,
    pub queued: usize,
    pub paused: usize,
    pub completed: usize,
    pub failed: usize,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct Settings {
    pub max_active: u16,
    pub global_down_limit: Option<u64>,
    pub global_up_limit: Option<u64>,
    pub download_dir: PathBuf,
    pub connections: u16,
    pub theme: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use puppy_core::types::{RequestContext, Source};
    use tokio::io::{AsyncWriteExt, BufReader, duplex};

    fn snap(id: u64) -> DownloadSnapshot {
        DownloadSnapshot {
            id,
            kind: EngineKindTag::Http,
            source: "http://x/f".into(),
            name: "f".into(),
            path: Some("/tmp/f".into()),
            state: WireState::Failed {
                reason: "r".into(),
                transient: true,
            },
            total_bytes: Some(10),
            done_bytes: 5,
            down_bps: 1,
            up_bps: 0,
            eta_secs: None,
            limit: None,
            queue_position: Some(0),
            connections: Some(4),
            peers: None,
            next_retry_unix: Some(99),
        }
    }

    #[test]
    fn request_wire_format() {
        let e = Envelope {
            req_id: 7,
            body: Request::Pause { id: 3 },
        };
        let s = serde_json::to_string(&e).unwrap();
        assert_eq!(s, r#"{"req_id":7,"method":"pause","id":3}"#);
        assert_eq!(serde_json::from_str::<Envelope<Request>>(&s).unwrap(), e);

        let add = Envelope {
            req_id: 1,
            body: Request::Add {
                request: AddRequest {
                    source: Source::Http {
                        url: "http://x/".parse().unwrap(),
                    },
                    output_dir: None,
                    filename: None,
                    context: RequestContext::default(),
                },
                start_paused: true,
                limit: Some(9),
            },
        };
        let s = serde_json::to_string(&add).unwrap();
        assert_eq!(serde_json::from_str::<Envelope<Request>>(&s).unwrap(), add);
        let unit: Envelope<Request> =
            serde_json::from_str(r#"{"req_id":2,"method":"list"}"#).unwrap();
        assert_eq!(unit.body, Request::List);
    }

    #[test]
    fn response_ok_wire_format() {
        let m = ServerMsg::Response {
            req_id: 1,
            result: Reply::Ok {
                data: ReplyData::Added { id: 5 },
            },
        };
        let s = serde_json::to_string(&m).unwrap();
        assert_eq!(
            s,
            r#"{"type":"response","req_id":1,"status":"ok","data":{"kind":"added","id":5}}"#
        );
        assert_eq!(serde_json::from_str::<ServerMsg>(&s).unwrap(), m);
        let d = ServerMsg::Response {
            req_id: 2,
            result: Reply::Ok {
                data: ReplyData::Download {
                    download: snap(1),
                    detail: None,
                },
            },
        };
        let s = serde_json::to_string(&d).unwrap();
        assert_eq!(serde_json::from_str::<ServerMsg>(&s).unwrap(), d);
    }

    #[test]
    fn response_err_wire_format() {
        let m = ServerMsg::Response {
            req_id: 4,
            result: Reply::Err {
                code: ErrorCode::NotFound,
                message: "no such id".into(),
            },
        };
        let s = serde_json::to_string(&m).unwrap();
        assert_eq!(
            s,
            r#"{"type":"response","req_id":4,"status":"err","code":"not_found","message":"no such id"}"#
        );
        assert_eq!(serde_json::from_str::<ServerMsg>(&s).unwrap(), m);
    }

    #[test]
    fn event_roundtrip() {
        let events = vec![
            Event::Snapshot {
                downloads: vec![snap(1), snap(2)],
                totals: Totals::default(),
            },
            Event::Added { id: 1 },
            Event::Completed {
                id: 1,
                name: "a".into(),
            },
            Event::Failed {
                id: 1,
                reason: "boom".into(),
            },
            Event::Removed { id: 1 },
            Event::NeedsAttention {
                id: 1,
                reason: WireAttention::ValidationFailed { detail: "d".into() },
            },
        ];
        for ev in events {
            let m = ServerMsg::Event { event: ev };
            let s = serde_json::to_string(&m).unwrap();
            assert_eq!(serde_json::from_str::<ServerMsg>(&s).unwrap(), m, "{s}");
        }
    }

    #[tokio::test]
    async fn codec_roundtrip_multiple_msgs() {
        let (a, b) = duplex(64);
        let msgs = vec![
            Envelope {
                req_id: 1,
                body: Request::List,
            },
            Envelope {
                req_id: 2,
                body: Request::Pause { id: 9 },
            },
            Envelope {
                req_id: 3,
                body: Request::Subscribe { interval_ms: 250 },
            },
        ];
        let w = msgs.clone();
        let t = tokio::spawn(async move {
            let mut a = a;
            for m in &w {
                write_msg(&mut a, m).await.unwrap();
            }
        });
        let mut r = BufReader::new(b);
        for m in &msgs {
            assert_eq!(
                &read_msg::<_, Envelope<Request>>(&mut r)
                    .await
                    .unwrap()
                    .unwrap(),
                m
            );
        }
        t.await.unwrap();
        assert!(
            read_msg::<_, Envelope<Request>>(&mut r)
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn codec_rejects_too_long() {
        let total = 2 << 20;
        let (mut a, b) = duplex(64 * 1024);
        // Writer would block forever past what the reader consumes, proving the
        // reader errored before reaching EOF of the 2 MiB stream.
        let w = tokio::spawn(async move {
            let chunk = vec![b'a'; 4096];
            let mut sent = 0;
            while sent < total {
                if a.write_all(&chunk).await.is_err() {
                    break;
                }
                sent += chunk.len();
            }
            sent
        });
        let mut r = BufReader::new(b);
        let err = read_msg::<_, Request>(&mut r).await.unwrap_err();
        assert!(matches!(err, CodecError::TooLong), "{err:?}");
        drop(r);
        let sent = w.await.unwrap();
        assert!(sent < total, "reader consumed whole stream");
    }

    #[tokio::test]
    async fn codec_eof_returns_none() {
        let mut r = BufReader::new(&b""[..]);
        assert!(read_msg::<_, Request>(&mut r).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn codec_bad_json_keeps_stream_usable() {
        let mut r = BufReader::new(&b"nope\n{\"req_id\":1,\"method\":\"list\"}\n"[..]);
        assert!(matches!(
            read_msg::<_, Envelope<Request>>(&mut r).await,
            Err(CodecError::Json(_))
        ));
        assert!(
            read_msg::<_, Envelope<Request>>(&mut r)
                .await
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn unknown_kinds_deserialize_to_unknown() {
        let m: ServerMsg =
            serde_json::from_str(r#"{"type":"event","event":{"kind":"brand_new","x":1}}"#).unwrap();
        assert_eq!(
            m,
            ServerMsg::Event {
                event: Event::Unknown
            }
        );
        let m: ServerMsg = serde_json::from_str(
            r#"{"type":"response","req_id":1,"status":"ok","data":{"kind":"brand_new"}}"#,
        )
        .unwrap();
        assert_eq!(
            m,
            ServerMsg::Response {
                req_id: 1,
                result: Reply::Ok {
                    data: ReplyData::Unknown
                }
            }
        );
    }

    /// `WireState` is the same JSON as core's `DownloadState`.
    #[test]
    fn wire_state_matches_core_json() {
        let states = [
            DownloadState::Queued,
            DownloadState::Starting,
            DownloadState::Active,
            DownloadState::Paused,
            DownloadState::Completed,
            DownloadState::Failed {
                reason: "r".into(),
                transient: false,
            },
            DownloadState::Failed {
                reason: "r".into(),
                transient: true,
            },
            DownloadState::NeedsAttention {
                reason: AttentionReason::RemoteChanged,
            },
            DownloadState::NeedsAttention {
                reason: AttentionReason::NotResumable,
            },
            DownloadState::NeedsAttention {
                reason: AttentionReason::ValidationFailed { detail: "d".into() },
            },
        ];
        for s in &states {
            let core = serde_json::to_value(s).unwrap();
            let wire = WireState::from(s);
            assert_eq!(serde_json::to_value(&wire).unwrap(), core);
            assert_eq!(serde_json::from_value::<WireState>(core).unwrap(), wire);
        }
    }

    /// Values a newer daemon may add inside known messages must not make the whole
    /// message undecodable.
    #[test]
    fn unknown_nested_values_deserialize() {
        let snap = serde_json::json!({
            "id": 1, "kind": "ipfs", "source": "s", "name": "n", "path": null,
            "state": {"kind": "verifying", "progress": 3},
            "total_bytes": null, "done_bytes": 0, "down_bps": 0, "up_bps": 0,
            "eta_secs": null, "limit": null, "queue_position": null,
            "connections": null, "peers": null
        });
        let m = serde_json::json!({"type": "event", "event": {"kind": "snapshot",
            "downloads": [snap], "totals": Totals::default()}});
        let m: ServerMsg = serde_json::from_value(m).expect("snapshot with unknown values");
        let ServerMsg::Event {
            event: Event::Snapshot { downloads, .. },
        } = m
        else {
            panic!("{m:?}")
        };
        assert_eq!(downloads[0].kind, EngineKindTag::Unknown);
        assert_eq!(downloads[0].state, WireState::Unknown);

        let m: ServerMsg = serde_json::from_str(
            r#"{"type":"response","req_id":3,"status":"err","code":"rate_limited","message":"m"}"#,
        )
        .expect("unknown error code");
        assert!(matches!(
            m,
            ServerMsg::Response {
                result: Reply::Err {
                    code: ErrorCode::Unknown,
                    ..
                },
                ..
            }
        ));

        let m: ServerMsg = serde_json::from_str(
            r#"{"type":"event","event":{"kind":"needs_attention","id":1,"reason":{"kind":"disk_full"}}}"#,
        )
        .expect("unknown attention reason");
        assert_eq!(
            m,
            ServerMsg::Event {
                event: Event::NeedsAttention {
                    id: 1,
                    reason: WireAttention::Unknown
                }
            }
        );
    }

    #[test]
    fn parse_request_lenient() {
        let v = serde_json::json!({"req_id": 5, "method": "teleport"});
        let (id, msg) = parse_request(v).unwrap_err();
        assert_eq!(id, Some(5));
        assert!(!msg.is_empty());
        let (id, _) = parse_request(serde_json::json!({"method": "list"})).unwrap_err();
        assert_eq!(id, None);
        let ok = parse_request(serde_json::json!({"req_id": 1, "method": "list"})).unwrap();
        assert_eq!(ok.body, Request::List);
    }

    fn line_of(n: usize) -> Vec<u8> {
        // A JSON string of exactly n bytes total.
        let mut v = vec![b'"'];
        v.extend(std::iter::repeat_n(b'a', n - 2));
        v.push(b'"');
        v
    }

    #[tokio::test]
    async fn codec_max_line_boundary() {
        let mut ok = line_of(MAX_LINE);
        ok.push(b'\n');
        let mut r = BufReader::new(&ok[..]);
        assert!(read_msg::<_, String>(&mut r).await.unwrap().is_some());
        let mut bad = line_of(MAX_LINE + 1);
        bad.push(b'\n');
        let mut r = BufReader::new(&bad[..]);
        assert!(matches!(
            read_msg::<_, String>(&mut r).await,
            Err(CodecError::TooLong)
        ));
    }

    #[tokio::test]
    async fn write_msg_refuses_oversize() {
        let mut out = Vec::new();
        let big = "a".repeat(MAX_LINE);
        assert!(matches!(
            write_msg(&mut out, &big).await,
            Err(CodecError::TooLong)
        ));
        assert!(out.is_empty());
    }

    #[tokio::test]
    async fn codec_crlf_blank_lines_and_mid_line_eof() {
        let mut r = BufReader::new(&b"\r\n\n{\"req_id\":1,\"method\":\"list\"}\r\n"[..]);
        let m: Envelope<Request> = read_msg(&mut r).await.unwrap().unwrap();
        assert_eq!(m.body, Request::List);
        assert!(read_msg::<_, Request>(&mut r).await.unwrap().is_none());
        let mut r = BufReader::new(&b"{\"req_id\":1,"[..]);
        match read_msg::<_, Request>(&mut r).await {
            Err(CodecError::Io(e)) => assert_eq!(e.kind(), std::io::ErrorKind::UnexpectedEof),
            other => panic!("{other:?}"),
        }
    }

    #[tokio::test]
    async fn msg_reader_cancel_safe() {
        let (mut a, b) = duplex(1024);
        let mut r = MsgReader::new(BufReader::new(b));
        a.write_all(b"{\"req_id\":1,\"met").await.unwrap();
        let first = tokio::time::timeout(
            std::time::Duration::from_millis(50),
            r.next::<Envelope<Request>>(),
        )
        .await;
        assert!(first.is_err(), "should have been cancelled");
        a.write_all(b"hod\":\"list\"}\n").await.unwrap();
        let m = r.next::<Envelope<Request>>().await.unwrap().unwrap();
        assert_eq!(
            m,
            Envelope {
                req_id: 1,
                body: Request::List
            }
        );
    }
}
