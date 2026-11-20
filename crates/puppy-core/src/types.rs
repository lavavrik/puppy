//! Shared data types exchanged between engines, the daemon and clients.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

pub type DownloadId = u64;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Source {
    Http { url: url::Url },
    Magnet { uri: String },
    TorrentFile { path: PathBuf },
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct RequestContext {
    pub headers: Vec<(String, String)>,
    pub cookies: Option<String>,
    pub referer: Option<String>,
    pub user_agent: Option<String>,
    pub basic_auth: Option<(String, Option<String>)>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AddRequest {
    pub source: Source,
    pub output_dir: Option<PathBuf>,
    pub filename: Option<String>,
    pub context: RequestContext,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AttentionReason {
    RemoteChanged,
    NotResumable,
    ValidationFailed { detail: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DownloadState {
    Queued,
    Starting,
    Active,
    Paused,
    Completed,
    Failed {
        reason: String,
        /// The cause may go away by itself (network error, timeout, HTTP 5xx/429/408):
        /// retrying later can succeed. Otherwise retrying as is would fail the same way.
        #[serde(default)]
        transient: bool,
    },
    NeedsAttention {
        reason: AttentionReason,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DownloadStatus {
    pub state: DownloadState,
    pub name: String,
    /// Where the finished file is (or will be). Can differ from the path `add` returned:
    /// if a file appeared there meanwhile, the download is finalized under a free name.
    pub path: PathBuf,
    pub total_bytes: Option<u64>,
    pub done_bytes: u64,
    pub down_bps: u64,
    pub up_bps: u64,
    pub eta_secs: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SegmentView {
    pub start: u64,
    /// Exclusive.
    pub end: u64,
    pub done: u64,
    pub bps: u64,
    pub active: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EngineDetail {
    Http {
        segments: Vec<SegmentView>,
        connections: u16,
        resumable: bool,
    },
    Torrent {
        piece_count: u32,
        /// Bitfield, MSB first.
        have: Vec<u8>,
        peers: u32,
        seeds: u32,
        leechers: u32,
        ratio: f32,
    },
    /// A kind this build does not know (sent by a newer daemon): no detail to show.
    #[serde(other)]
    Unknown,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn download_state_serde_roundtrip() {
        let s = DownloadState::NeedsAttention {
            reason: AttentionReason::RemoteChanged,
        };
        let json = serde_json::to_string(&s).unwrap();
        assert_eq!(
            json,
            r#"{"kind":"needs_attention","reason":{"kind":"remote_changed"}}"#
        );
        let back: DownloadState = serde_json::from_str(&json).unwrap();
        assert!(matches!(
            back,
            DownloadState::NeedsAttention {
                reason: AttentionReason::RemoteChanged
            }
        ));
    }

    /// A detail kind from a newer daemon decodes as `Unknown` instead of failing.
    #[test]
    fn unknown_engine_detail_kind_decodes() {
        let d: EngineDetail =
            serde_json::from_str(r#"{"kind":"ftp","sessions":3,"extra":[1,2]}"#).unwrap();
        assert_eq!(d, EngineDetail::Unknown);
        let back: EngineDetail = serde_json::from_str(&serde_json::to_string(&d).unwrap()).unwrap();
        assert_eq!(back, EngineDetail::Unknown);
        let http = EngineDetail::Http {
            segments: Vec::new(),
            connections: 2,
            resumable: true,
        };
        let back: EngineDetail =
            serde_json::from_str(&serde_json::to_string(&http).unwrap()).unwrap();
        assert_eq!(back, http);
    }
}
