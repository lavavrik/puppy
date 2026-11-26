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
use puppy_core::checksum::Checksum;
use puppy_core::types::{
    AddRequest, AttentionReason, DownloadId, DownloadState, EngineDetail, SeedPolicy, Selection,
    TorrentFile,
};
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
        /// The client's ssh-agent socket (`SSH_AUTH_SOCK`): the daemon's SFTP logins try
        /// the agents recent clients offered (the most recent first) before its own.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        ssh_auth_sock: Option<String>,
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
    /// Lists a torrent's files.
    ListFiles {
        id: DownloadId,
    },
    /// Chooses the files of a torrent to download.
    SelectFiles {
        id: DownloadId,
        selection: Selection,
    },
    /// Sets a torrent's seeding policy.
    SetSeed {
        id: DownloadId,
        policy: SeedPolicy,
    },
    Subscribe {
        interval_ms: u64,
    },
    Unsubscribe,
    Shutdown,
    /// The current update status, from memory; no network.
    GetUpdate,
    /// Checks for a newer release now and, unless `check_only`, installs it.
    Update {
        check_only: bool,
    },
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
#[allow(clippy::large_enum_variant)] // wire type; boxing would change the public API
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
    Files {
        files: Vec<TorrentFile>,
    },
    /// The daemon's update status.
    Update {
        status: UpdateStatus,
    },
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
    /// The download's engine cannot do this (e.g. change a running torrent's limit).
    Unsupported,
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
        /// Retried automatically (the snapshot's `next_retry_unix` says when). Absent
        /// from older daemons: treated as permanent.
        #[serde(default)]
        transient: bool,
    },
    Removed {
        id: DownloadId,
    },
    NeedsAttention {
        id: DownloadId,
        reason: WireAttention,
    },
    SeedingFinished {
        id: DownloadId,
    },
    /// A torrent waits for its files to be chosen.
    ChoosingFiles {
        id: DownloadId,
        name: String,
    },
    /// The update status changed.
    Update {
        status: UpdateStatus,
    },
    /// A kind this client does not know (newer daemon).
    #[serde(other)]
    Unknown,
}

/// Where the daemon stands on self-updating.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum UpdateStatus {
    /// No check has run yet (or updates are off).
    NotChecked,
    /// The running version is the latest release.
    UpToDate,
    /// A newer release exists.
    Available {
        version: String,
        /// Whether this daemon can install it itself.
        can_install: bool,
        /// Why not, when `can_install` is false.
        reason: Option<String>,
    },
    /// Downloading and swapping in `version`.
    Installing { version: String },
    /// Swapped in; the daemon is restarting into it.
    Installed { version: String },
    /// The last check or install failed.
    Failed {
        version: Option<String>,
        error: String,
    },
    /// A state this client does not know (newer daemon).
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
    /// Why the download is in its state, when the engine says (e.g. why a torrent waits
    /// for a file choice although one was given).
    #[serde(default)]
    pub note: Option<String>,
    /// A torrent's own seeding policy as set (`add --seed-*`, `seed`, the dashboard):
    /// a `None` field (or no policy) inherits the daemon's default.
    #[serde(default)]
    pub seed: Option<SeedPolicy>,
    /// The checksum the file is verified against once complete (`add --checksum`).
    #[serde(default)]
    pub checksum: Option<Checksum>,
    /// The mirrors of an HTTP download (`add --mirror`), display form (a password
    /// masked), in the order given; how each is used is in the engine detail.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mirrors: Vec<String>,
}

/// Where a download with a checksum stands with it, derived from its state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verification {
    /// Not checked yet (still downloading, queued, paused, ...).
    Pending,
    Verifying,
    /// Completed, so the checksum matched.
    Verified,
    /// Failed with a checksum mismatch (the reason names both digests).
    Mismatch,
}

impl DownloadSnapshot {
    /// `None` without a checksum.
    pub fn verification(&self) -> Option<Verification> {
        self.checksum.as_ref()?;
        Some(match &self.state {
            WireState::Verifying => Verification::Verifying,
            WireState::Completed => Verification::Verified,
            WireState::Failed { reason, .. } if puppy_core::checksum::is_mismatch(reason) => {
                Verification::Mismatch
            }
            _ => Verification::Pending,
        })
    }
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
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WireState {
    Queued,
    Starting,
    Active,
    /// The file is complete and being checked against its checksum (`done_bytes`:
    /// bytes hashed so far). Older clients see `Unknown`.
    Verifying,
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
    ChoosingFiles,
    Seeding {
        uploaded: u64,
        ratio: f32,
        seeded_secs: u64,
        #[serde(default)]
        ratio_target: Option<f32>,
        #[serde(default)]
        time_target_secs: Option<u64>,
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

impl WireAttention {
    /// A seeding torrent's data failed its check (see
    /// [`puppy_core::types::RECHECK_FAILED`]): `resume` downloads it again.
    pub fn is_resumable(&self) -> bool {
        matches!(self, WireAttention::ValidationFailed { detail }
            if detail == puppy_core::types::RECHECK_FAILED)
    }
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

/// JSON has no NaN/infinity (serde_json would write `null`, which a reader rejects as an
/// `f32`): a non-finite ratio goes on the wire as `0.0`.
fn finite(x: f32) -> f32 {
    if x.is_finite() { x } else { 0.0 }
}

impl From<&DownloadState> for WireState {
    fn from(s: &DownloadState) -> Self {
        match s {
            DownloadState::Queued => WireState::Queued,
            DownloadState::Starting => WireState::Starting,
            DownloadState::Active => WireState::Active,
            DownloadState::Verifying => WireState::Verifying,
            DownloadState::Paused => WireState::Paused,
            DownloadState::Completed => WireState::Completed,
            DownloadState::Failed { reason, transient } => WireState::Failed {
                reason: reason.clone(),
                transient: *transient,
            },
            DownloadState::NeedsAttention { reason } => WireState::NeedsAttention {
                reason: reason.into(),
            },
            DownloadState::ChoosingFiles => WireState::ChoosingFiles,
            DownloadState::Seeding {
                uploaded,
                ratio,
                seeded_secs,
                ratio_target,
                time_target_secs,
            } => WireState::Seeding {
                uploaded: *uploaded,
                ratio: finite(*ratio),
                seeded_secs: *seeded_secs,
                ratio_target: ratio_target.map(finite),
                time_target_secs: *time_target_secs,
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
    use puppy_core::checksum::HashAlgo;
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
            note: None,
            seed: None,
            checksum: None,
            mirrors: Vec::new(),
        }
    }

    #[test]
    fn update_status_round_trips() {
        let all = [
            UpdateStatus::NotChecked,
            UpdateStatus::UpToDate,
            UpdateStatus::Available {
                version: "1.2.3".into(),
                can_install: false,
                reason: Some("why".into()),
            },
            UpdateStatus::Available {
                version: "1.2.3".into(),
                can_install: true,
                reason: None,
            },
            UpdateStatus::Installing {
                version: "1.2.3".into(),
            },
            UpdateStatus::Installed {
                version: "1.2.3".into(),
            },
            UpdateStatus::Failed {
                version: Some("1.2.3".into()),
                error: "boom".into(),
            },
            UpdateStatus::Failed {
                version: None,
                error: "boom".into(),
            },
        ];
        for status in all {
            let s = serde_json::to_string(&status).unwrap();
            assert_eq!(serde_json::from_str::<UpdateStatus>(&s).unwrap(), status);
            let ev = Event::Update {
                status: status.clone(),
            };
            let s = serde_json::to_string(&ev).unwrap();
            assert_eq!(serde_json::from_str::<Event>(&s).unwrap(), ev);
            let reply = ReplyData::Update { status };
            let s = serde_json::to_string(&reply).unwrap();
            assert_eq!(serde_json::from_str::<ReplyData>(&s).unwrap(), reply);
        }
    }

    #[test]
    fn unknown_update_state_is_unknown() {
        let s: UpdateStatus = serde_json::from_str(r#"{"state":"from_the_future"}"#).unwrap();
        assert_eq!(s, UpdateStatus::Unknown);
    }

    #[test]
    fn update_request_wire_form() {
        let e = Envelope {
            req_id: 4,
            body: Request::Update { check_only: true },
        };
        let s = serde_json::to_string(&e).unwrap();
        assert_eq!(s, r#"{"req_id":4,"method":"update","check_only":true}"#);
        assert_eq!(serde_json::from_str::<Envelope<Request>>(&s).unwrap(), e);
        let get: Envelope<Request> =
            serde_json::from_str(r#"{"req_id":5,"method":"get_update"}"#).unwrap();
        assert_eq!(get.body, Request::GetUpdate);
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
                    files: Default::default(),
                    seed: None,
                    checksum: None,
                    mirrors: Vec::new(),
                    size: None,
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
    fn unsupported_error_code_wire_format() {
        let json = serde_json::to_string(&ErrorCode::Unsupported).unwrap();
        assert_eq!(json, r#""unsupported""#);
        let back: ErrorCode = serde_json::from_str(r#""unsupported""#).unwrap();
        assert_eq!(back, ErrorCode::Unsupported);
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
                transient: true,
            },
            Event::Removed { id: 1 },
            Event::NeedsAttention {
                id: 1,
                reason: WireAttention::ValidationFailed { detail: "d".into() },
            },
            Event::ChoosingFiles {
                id: 4,
                name: "Big.Show.S01".into(),
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
            DownloadState::ChoosingFiles,
            DownloadState::Seeding {
                uploaded: 7,
                ratio: 0.5,
                seeded_secs: 3,
                ratio_target: Some(1.0),
                time_target_secs: None,
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
            "state": {"kind": "deduplicating", "progress": 3},
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
    fn torrent_feature_messages_roundtrip() {
        let reqs = [
            Request::ListFiles { id: 1 },
            Request::SelectFiles {
                id: 1,
                selection: Selection {
                    indices: vec![0],
                    globs: vec!["*.txt".into()],
                },
            },
            Request::SetSeed {
                id: 1,
                policy: SeedPolicy {
                    ratio: Some(2.0),
                    time_secs: None,
                },
            },
        ];
        for r in reqs {
            let e = Envelope { req_id: 1, body: r };
            let s = serde_json::to_string(&e).unwrap();
            assert_eq!(serde_json::from_str::<Envelope<Request>>(&s).unwrap(), e);
        }
        let m = ServerMsg::Response {
            req_id: 1,
            result: Reply::Ok {
                data: ReplyData::Files {
                    files: vec![TorrentFile {
                        index: 0,
                        path: "a".into(),
                        size: 1,
                    }],
                },
            },
        };
        let s = serde_json::to_string(&m).unwrap();
        assert!(s.contains(r#""kind":"files""#), "{s}");
        assert_eq!(serde_json::from_str::<ServerMsg>(&s).unwrap(), m);
        let m = ServerMsg::Event {
            event: Event::SeedingFinished { id: 4 },
        };
        let s = serde_json::to_string(&m).unwrap();
        assert!(s.contains(r#""kind":"seeding_finished""#), "{s}");
        assert_eq!(serde_json::from_str::<ServerMsg>(&s).unwrap(), m);
    }

    /// An older client (modelled by an unknown kind) falls back to `Unknown`; and the
    /// new states decode on this build.
    #[test]
    fn new_states_and_older_decoding() {
        let w: WireState = serde_json::from_str(r#"{"kind":"choosing_files"}"#).unwrap();
        assert_eq!(w, WireState::ChoosingFiles);
        let w: WireState = serde_json::from_str(
            r#"{"kind":"seeding","uploaded":1,"ratio":0.1,"seeded_secs":2,"ratio_target":null,"time_target_secs":null}"#,
        )
        .unwrap();
        assert!(matches!(w, WireState::Seeding { uploaded: 1, .. }));
        let w: WireState = serde_json::from_str(r#"{"kind":"future_state","x":1}"#).unwrap();
        assert_eq!(w, WireState::Unknown);
        // Targets may be omitted.
        let w: WireState =
            serde_json::from_str(r#"{"kind":"seeding","uploaded":1,"ratio":0.1,"seeded_secs":2}"#)
                .unwrap();
        assert!(matches!(
            w,
            WireState::Seeding {
                ratio_target: None,
                time_target_secs: None,
                ..
            }
        ));
        let c: DownloadState =
            serde_json::from_str(r#"{"kind":"seeding","uploaded":1,"ratio":0.1,"seeded_secs":2}"#)
                .unwrap();
        assert!(matches!(
            c,
            DownloadState::Seeding {
                ratio_target: None,
                ..
            }
        ));
    }

    #[test]
    fn snapshot_note_is_optional() {
        let mut s = snap(1);
        s.note = Some("pattern \"*.iso\" matches no file".into());
        let json = serde_json::to_value(&s).unwrap();
        assert_eq!(json["note"], "pattern \"*.iso\" matches no file");
        assert_eq!(
            serde_json::from_value::<DownloadSnapshot>(json.clone()).unwrap(),
            s
        );
        // From an older daemon: no `note`.
        let mut old = json;
        old.as_object_mut().unwrap().remove("note");
        assert_eq!(
            serde_json::from_value::<DownloadSnapshot>(old)
                .unwrap()
                .note,
            None
        );
    }

    #[test]
    fn checksum_and_verifying_on_the_wire() {
        let sum: Checksum = format!("sha-256={}", "AB".repeat(32)).parse().unwrap();
        let mut s = snap(1);
        s.checksum = Some(sum.clone());
        s.state = (&DownloadState::Verifying).into();
        let json = serde_json::to_value(&s).unwrap();
        assert_eq!(json["state"], serde_json::json!({"kind": "verifying"}));
        assert_eq!(
            json["checksum"],
            serde_json::json!({"algo": "sha-256", "hex": "ab".repeat(32)})
        );
        assert_eq!(
            serde_json::from_value::<DownloadSnapshot>(json.clone()).unwrap(),
            s
        );
        assert_eq!(s.verification(), Some(Verification::Verifying));
        // From an older daemon: no `checksum`.
        let mut old = json;
        old.as_object_mut().unwrap().remove("checksum");
        let old: DownloadSnapshot = serde_json::from_value(old).unwrap();
        assert_eq!((old.checksum.clone(), old.verification()), (None, None));
        for (state, v) in [
            (WireState::Completed, Verification::Verified),
            (WireState::Active, Verification::Pending),
            (
                WireState::Failed {
                    reason: sum.mismatch("00"),
                    transient: false,
                },
                Verification::Mismatch,
            ),
            (
                WireState::Failed {
                    reason: "HTTP 404".into(),
                    transient: false,
                },
                Verification::Pending,
            ),
        ] {
            s.state = state;
            assert_eq!(s.verification(), Some(v));
        }
        // An add with a checksum; a malformed one is a bad request.
        let req = serde_json::json!({"req_id": 1, "method": "add", "request": {
            "source": {"kind": "http", "url": "http://x/f"}, "output_dir": null,
            "filename": null, "context": {"headers": [], "cookies": null, "referer": null,
            "user_agent": null, "basic_auth": null},
            "checksum": {"algo": "md5", "hex": "0".repeat(32)}}});
        let Request::Add { request, .. } = parse_request(req.clone()).unwrap().body else {
            panic!()
        };
        assert_eq!(request.checksum.unwrap().algo, HashAlgo::Md5);
        let mut bad = req;
        bad["request"]["checksum"]["hex"] = "xyz".into();
        assert_eq!(parse_request(bad).unwrap_err().0, Some(1));
    }

    #[test]
    fn non_finite_ratio_goes_on_wire_as_zero() {
        for bad in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            let s = DownloadState::Seeding {
                uploaded: 1,
                ratio: bad,
                seeded_secs: 0,
                ratio_target: Some(bad),
                time_target_secs: None,
            };
            let w = WireState::from(&s);
            let json = serde_json::to_string(&w).unwrap();
            assert!(
                json.contains(r#""ratio":0.0"#) && json.contains(r#""ratio_target":0.0"#),
                "{json}"
            );
            let back: WireState = serde_json::from_str(&json).unwrap();
            assert_eq!(
                back,
                WireState::Seeding {
                    uploaded: 1,
                    ratio: 0.0,
                    seeded_secs: 0,
                    ratio_target: Some(0.0),
                    time_target_secs: None,
                }
            );
        }
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
