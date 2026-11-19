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

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AttentionReason {
    RemoteChanged,
    NotResumable,
    ValidationFailed { detail: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DownloadState {
    Queued,
    Starting,
    Active,
    Paused,
    Completed,
    Failed { reason: String },
    NeedsAttention { reason: AttentionReason },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DownloadStatus {
    pub state: DownloadState,
    pub name: String,
    pub total_bytes: Option<u64>,
    pub done_bytes: u64,
    pub down_bps: u64,
    pub up_bps: u64,
    pub eta_secs: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SegmentView {
    pub start: u64,
    /// Exclusive.
    pub end: u64,
    pub done: u64,
    pub bps: u64,
    pub active: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
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
}
