//! The engine abstraction implemented by the HTTP (and later torrent) engines.

use crate::types::{AddRequest, DownloadId, DownloadState, DownloadStatus, EngineDetail, Source};
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EngineKind {
    Http,
    Torrent,
}

#[derive(Debug, Clone)]
pub enum StartMode {
    Fresh,
    Restore { path: PathBuf },
}

#[derive(Debug, Clone)]
pub struct Added {
    pub name: String,
    pub path: PathBuf,
    pub total_bytes: Option<u64>,
}

#[derive(Debug, Clone)]
pub enum EngineEvent {
    StateChanged {
        id: DownloadId,
        state: DownloadState,
    },
    Completed {
        id: DownloadId,
    },
}

#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error("download {0} not found")]
    NotFound(DownloadId),
    #[error("unsupported: {0}")]
    Unsupported(&'static str),
    #[error("network error: {0}")]
    Network(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("http status {status}")]
    Http { status: u16 },
    #[error("invalid: {0}")]
    Invalid(String),
}

#[async_trait::async_trait]
pub trait Engine: Send + Sync + 'static {
    fn kind(&self) -> EngineKind;
    fn accepts(&self, source: &Source) -> bool;
    /// Probes/prepares, starts transferring in the background, returns once name/path are known.
    async fn add(
        &self,
        id: DownloadId,
        req: AddRequest,
        mode: StartMode,
    ) -> Result<Added, EngineError>;
    async fn pause(&self, id: DownloadId) -> Result<(), EngineError>;
    async fn resume(&self, id: DownloadId) -> Result<(), EngineError>;
    async fn remove(&self, id: DownloadId, delete_files: bool) -> Result<(), EngineError>;
    fn status(&self, id: DownloadId) -> Option<DownloadStatus>;
    fn detail(&self, id: DownloadId) -> Option<EngineDetail>;
    fn set_download_limit(&self, id: DownloadId, bps: Option<u64>) -> Result<(), EngineError>;
    /// Engine-wide download budget assigned by the daemon's global limiter.
    fn set_global_download_budget(&self, bps: Option<u64>);
    fn subscribe(&self) -> tokio::sync::broadcast::Receiver<EngineEvent>;
}
