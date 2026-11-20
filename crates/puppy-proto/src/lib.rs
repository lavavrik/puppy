//! Versioned newline-delimited-JSON protocol spoken by the daemon and its clients.

pub mod codec;

pub use codec::{CodecError, read_msg, write_msg};
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

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
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
        reason: AttentionReason,
    },
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct DownloadSnapshot {
    pub id: DownloadId,
    pub kind: EngineKindTag,
    /// Display form: URL, magnet name or .torrent path.
    pub source: String,
    pub name: String,
    pub path: Option<PathBuf>,
    pub state: DownloadState,
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
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EngineKindTag {
    Http,
    Torrent,
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
            state: DownloadState::Failed { reason: "r".into() },
            total_bytes: Some(10),
            done_bytes: 5,
            down_bps: 1,
            up_bps: 0,
            eta_secs: None,
            limit: None,
            queue_position: Some(0),
            connections: Some(4),
            peers: None,
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
                reason: AttentionReason::ValidationFailed { detail: "d".into() },
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
}
