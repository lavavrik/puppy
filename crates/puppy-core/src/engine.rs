//! The engine abstraction implemented by the HTTP (and later torrent) engines.

use crate::types::{AddRequest, DownloadId, DownloadState, DownloadStatus, EngineDetail, Source};
use serde::{Deserialize, Serialize};
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

/// Options for [`Engine::add`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AddOptions {
    /// Prepare the download (fresh: probe and create its files; restore: check the local
    /// files only, no network) but leave it `Paused` instead of transferring. `resume`
    /// starts it (validating the remote file as usual).
    pub start_paused: bool,
    /// Per-download rate limit in bytes/s, in force before the first byte is transferred.
    pub limit: Option<u64>,
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

impl EngineError {
    /// Whether the error may go away by itself (network trouble, a timeout, or an HTTP
    /// status saying "try again later": 5xx, 429, 408), so retrying later can succeed.
    pub fn is_transient(&self) -> bool {
        match self {
            EngineError::Network(_) => true,
            EngineError::Http { status } => is_transient_status(*status),
            _ => false,
        }
    }
}

/// HTTP statuses that mean "try again later".
pub fn is_transient_status(status: u16) -> bool {
    status >= 500 || status == 429 || status == 408
}

/// A download engine managing many downloads by caller-chosen id.
///
/// - A download in `NeedsAttention` is not resumable as is. To restart it from scratch,
///   `remove(id, true)` it and `add` it again (`StartMode::Fresh`) with the request the
///   caller stored.
/// - Dropping an engine does not stop its downloads (their tasks are detached): `pause`
///   or `remove` them first.
#[async_trait::async_trait]
pub trait Engine: Send + Sync + 'static {
    fn kind(&self) -> EngineKind;
    fn accepts(&self, source: &Source) -> bool;
    /// Registers download `id` and returns once its name and path are known.
    ///
    /// - Blocks for the remote probe (`Fresh`) or local checks and remote validation
    ///   (`Restore`), so it can take as long as the network does. Dropping the future
    ///   cancels the add: the id is released and no transfer is started.
    /// - Unless `opts.start_paused`, transferring then continues in the background.
    /// - `opts.limit` is applied before any transfer begins.
    /// - `Restore` continues from the files at `path`; `req` must be the request the
    ///   download was originally added with (it supplies the request context, which is
    ///   never persisted next to the download).
    /// - Events for `id` (e.g. `StateChanged`) may be broadcast before `add` returns.
    async fn add(
        &self,
        id: DownloadId,
        req: AddRequest,
        mode: StartMode,
        opts: AddOptions,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transient_errors() {
        assert!(EngineError::Network("reset".into()).is_transient());
        for status in [500, 502, 503, 429, 408] {
            assert!(EngineError::Http { status }.is_transient(), "{status}");
        }
        for status in [400, 401, 403, 404, 410, 416] {
            assert!(!EngineError::Http { status }.is_transient(), "{status}");
        }
        assert!(!EngineError::Io(std::io::Error::other("disk full")).is_transient());
        assert!(!EngineError::Invalid("x".into()).is_transient());
        assert!(!EngineError::NotFound(1).is_transient());
    }
}
