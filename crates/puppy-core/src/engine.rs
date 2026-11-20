//! The engine abstraction implemented by the HTTP (and later torrent) engines.

use crate::types::{
    AddRequest, DownloadId, DownloadState, DownloadStatus, EngineDetail, SeedPolicy, Selection,
    Source, TorrentFile,
};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

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
    /// The engine cannot work right now (e.g. its session failed to start); the message
    /// is shown as is. Transient: a later attempt may find it working.
    #[error("{0}")]
    Unavailable(String),
}

impl EngineError {
    /// Whether the error may go away by itself (network trouble, a timeout, or an HTTP
    /// status saying "try again later": 5xx, 429, 408), so retrying later can succeed.
    pub fn is_transient(&self) -> bool {
        match self {
            EngineError::Network(_) | EngineError::Unavailable(_) => true,
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
    /// Global upload limit in bytes/s (`None`: unlimited), passed as set (not divided).
    /// Engines that do not upload ignore it.
    fn set_global_upload_budget(&self, _bps: Option<u64>) {}
    fn subscribe(&self) -> tokio::sync::broadcast::Receiver<EngineEvent>;

    /// Drops a completed download from the engine (it no longer holds it afterwards),
    /// keeping its files and whatever the engine needs for [`Engine::delete_completed`].
    /// Default: `remove(id, false)`.
    async fn drop_completed(&self, id: DownloadId) -> Result<(), EngineError> {
        self.remove(id, false).await
    }

    /// Deletes the files of completed download `id` (no longer held by the engine), which
    /// were recorded at `path`. Default: removes `path` if it is a regular file (a symlink
    /// or directory there is left alone); nothing there is fine.
    async fn delete_completed(&self, _id: DownloadId, path: &Path) -> Result<(), EngineError> {
        match tokio::fs::symlink_metadata(path).await {
            Ok(m) if m.is_file() => match tokio::fs::remove_file(path).await {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.into()),
                _ => Ok(()),
            },
            Ok(_) => {
                tracing::warn!(path = %path.display(), "not deleting: not a regular file");
                Ok(())
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    /// Forgets anything the engine still keeps for download `id` once it has been
    /// removed for good (not held by the engine anymore). Default: nothing.
    async fn forget(&self, _id: DownloadId) {}

    /// The files of a torrent. Default: unsupported.
    async fn list_files(&self, _id: DownloadId) -> Result<Vec<TorrentFile>, EngineError> {
        Err(EngineError::Unsupported("file listing"))
    }

    /// Chooses which files of a torrent to download. Default: unsupported.
    async fn select_files(
        &self,
        _id: DownloadId,
        _selection: Selection,
    ) -> Result<(), EngineError> {
        Err(EngineError::Unsupported("file selection"))
    }

    /// Sets the seeding policy of a torrent. Default: unsupported.
    async fn set_seed(&self, _id: DownloadId, _policy: SeedPolicy) -> Result<(), EngineError> {
        Err(EngineError::Unsupported("seeding policy"))
    }
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
        let e = EngineError::Unavailable("torrent engine unavailable (see daemon log)".into());
        assert!(e.is_transient());
        assert_eq!(e.to_string(), "torrent engine unavailable (see daemon log)");
    }

    struct Nop;

    #[async_trait::async_trait]
    impl Engine for Nop {
        fn kind(&self) -> EngineKind {
            EngineKind::Http
        }
        fn accepts(&self, _: &Source) -> bool {
            true
        }
        async fn add(
            &self,
            _: DownloadId,
            _: AddRequest,
            _: StartMode,
            _: AddOptions,
        ) -> Result<Added, EngineError> {
            unimplemented!()
        }
        async fn pause(&self, _: DownloadId) -> Result<(), EngineError> {
            Ok(())
        }
        async fn resume(&self, _: DownloadId) -> Result<(), EngineError> {
            Ok(())
        }
        async fn remove(&self, _: DownloadId, _: bool) -> Result<(), EngineError> {
            Ok(())
        }
        fn status(&self, _: DownloadId) -> Option<DownloadStatus> {
            None
        }
        fn detail(&self, _: DownloadId) -> Option<EngineDetail> {
            None
        }
        fn set_download_limit(&self, _: DownloadId, _: Option<u64>) -> Result<(), EngineError> {
            Ok(())
        }
        fn set_global_download_budget(&self, _: Option<u64>) {}
        fn subscribe(&self) -> tokio::sync::broadcast::Receiver<EngineEvent> {
            tokio::sync::broadcast::channel(1).1
        }
    }

    #[tokio::test]
    async fn new_methods_default_to_unsupported() {
        assert!(matches!(
            Nop.list_files(1).await,
            Err(EngineError::Unsupported(_))
        ));
        assert!(matches!(
            Nop.select_files(1, Selection::default()).await,
            Err(EngineError::Unsupported(_))
        ));
        assert!(matches!(
            Nop.set_seed(1, SeedPolicy::default()).await,
            Err(EngineError::Unsupported(_))
        ));
    }

    #[tokio::test]
    async fn default_delete_completed_removes_only_a_regular_file() {
        let d = tempfile::tempdir().unwrap();
        let file = d.path().join("a.bin");
        std::fs::write(&file, b"x").unwrap();
        Nop.delete_completed(1, &file).await.unwrap();
        assert!(!file.exists());
        Nop.delete_completed(1, &file)
            .await
            .expect("already gone is fine");
        let dir = d.path().join("folder");
        std::fs::create_dir(&dir).unwrap();
        std::fs::write(dir.join("keep"), b"x").unwrap();
        Nop.delete_completed(1, &dir).await.unwrap();
        assert!(dir.join("keep").exists(), "a directory is left alone");
    }
}
