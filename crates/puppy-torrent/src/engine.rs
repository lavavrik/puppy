//! `TorrentEngine`: one librqbit [`Session`] configured for puppy behind the `Engine` trait.

use std::net::{Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;

use librqbit::dht::DhtPersistenceConfig;
use librqbit::{DhtSessionConfig, ListenerOptions, Session, SessionOptions};
use puppy_core::engine::{
    AddOptions, Added, Engine, EngineError, EngineEvent, EngineKind, StartMode,
};
use puppy_core::types::{AddRequest, DownloadId, DownloadStatus, EngineDetail, Source};
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

/// How a [`TorrentEngine`] is set up (the daemon fills it from its `[torrent]` config).
#[derive(Debug, Clone)]
pub struct TorrentEngineOptions {
    /// Use the DHT to find peers (ignored in `test_mode`: always off).
    pub dht: bool,
    /// Ask the router to forward the listen port via UPnP (ignored in `test_mode`).
    pub upnp: bool,
    /// Port for incoming peer connections on `0.0.0.0`; an OS-assigned port is used if it
    /// cannot be bound. 0: always OS-assigned.
    pub listen_port: u16,
    /// Reserved: seeding stops at completion (ratio 0) in this version.
    pub seed_ratio: f32,
    /// puppy's data dir; the engine keeps its own files under `<data_dir>/torrent/`.
    pub data_dir: PathBuf,
    /// Where torrents go when the request names no output dir.
    pub default_dir: PathBuf,
    /// Tests: DHT, trackers, LSD and UPnP off; listen on `127.0.0.1:0`.
    #[doc(hidden)]
    pub test_mode: bool,
    /// Tests: peers given to every added torrent (the local seeder).
    #[doc(hidden)]
    pub test_initial_peers: Vec<SocketAddr>,
}

/// Torrent engine: owns one librqbit session (its own persistence off: puppy's store is the
/// source of truth, torrents are re-added on restore).
pub struct TorrentEngine {
    session: Arc<Session>,
    /// The session's cancellation token: cancelling it stops every session task.
    cancel: CancellationToken,
    events: broadcast::Sender<EngineEvent>,
    #[allow(dead_code)] // used from Task 3 on (add, restore, test peers)
    opts: TorrentEngineOptions,
}

impl TorrentEngine {
    /// Starts the librqbit session (listener, DHT unless disabled).
    pub async fn new(opts: TorrentEngineOptions) -> Result<Self, EngineError> {
        std::fs::create_dir_all(opts.data_dir.join("torrent"))?;
        let cancel = CancellationToken::new();
        let session = if opts.test_mode || opts.listen_port == 0 {
            start_session(&opts, opts.listen_port, cancel.clone()).await
        } else {
            match start_session(&opts, opts.listen_port, cancel.clone()).await {
                Ok(s) => Ok(s),
                Err(e) => {
                    // librqbit reports a busy port only as a session start error ("error
                    // starting listeners"); retrying with port 0 covers it, and any other
                    // cause fails again and is reported then.
                    tracing::warn!(
                        port = opts.listen_port,
                        "torrent session failed to start ({e:#}); retrying with an OS-assigned port"
                    );
                    start_session(&opts, 0, cancel.clone()).await
                }
            }
        }
        .map_err(|e| EngineError::Network(format!("starting the torrent session: {e:#}")))?;
        tracing::info!(listen = ?session.listen_addr(), "torrent session started");
        let (events, _) = broadcast::channel(256);
        Ok(Self {
            session,
            cancel,
            events,
            opts,
        })
    }

    /// Where the session accepts incoming peer connections.
    pub fn listen_addr(&self) -> Option<SocketAddr> {
        self.session.listen_addr()
    }

    /// Pauses every torrent and stops the session's tasks (listener, DHT, peers). Takes
    /// about 1 s (librqbit gives its tasks a moment to wind down).
    pub async fn shutdown(&self) {
        self.session.stop().await;
    }
}

impl Drop for TorrentEngine {
    fn drop(&mut self) {
        // Unlike the HTTP engine, a dropped torrent engine stops its session: nothing else
        // could reach it, and a new engine (restore) must be able to take the files over.
        self.cancel.cancel();
    }
}

async fn start_session(
    opts: &TorrentEngineOptions,
    port: u16,
    cancel: CancellationToken,
) -> anyhow::Result<Arc<Session>> {
    let test = opts.test_mode;
    let listen_addr: SocketAddr = if test {
        (Ipv4Addr::LOCALHOST, 0).into()
    } else {
        (Ipv4Addr::UNSPECIFIED, port).into()
    };
    let dht = (opts.dht && !test).then(|| DhtSessionConfig {
        bootstrap_addrs: None,
        port: None,
        // The DHT routing table (not torrents) is kept between runs for a quick bootstrap.
        persistence: Some(DhtPersistenceConfig {
            dump_interval: None,
            config_filename: Some(opts.data_dir.join("torrent").join("dht.json")),
        }),
    });
    let session_opts = SessionOptions {
        dht,
        disable_trackers: test,
        disable_local_service_discovery: test,
        persistence: None,
        fastresume: false,
        listen: Some(ListenerOptions {
            listen_addr,
            enable_upnp_port_forwarding: opts.upnp && !test,
            ..Default::default()
        }),
        cancellation_token: Some(cancel),
        ..Default::default()
    };
    Session::new_with_opts(opts.default_dir.clone(), session_opts).await
}

const NOT_YET: &str = "not implemented yet";

#[async_trait::async_trait]
impl Engine for TorrentEngine {
    fn kind(&self) -> EngineKind {
        EngineKind::Torrent
    }

    fn accepts(&self, source: &Source) -> bool {
        matches!(source, Source::Magnet { .. } | Source::TorrentFile { .. })
    }

    async fn add(
        &self,
        _id: DownloadId,
        _req: AddRequest,
        _mode: StartMode,
        _opts: AddOptions,
    ) -> Result<Added, EngineError> {
        Err(EngineError::Unsupported(NOT_YET))
    }

    async fn pause(&self, _id: DownloadId) -> Result<(), EngineError> {
        Err(EngineError::Unsupported(NOT_YET))
    }

    async fn resume(&self, _id: DownloadId) -> Result<(), EngineError> {
        Err(EngineError::Unsupported(NOT_YET))
    }

    async fn remove(&self, _id: DownloadId, _delete_files: bool) -> Result<(), EngineError> {
        Err(EngineError::Unsupported(NOT_YET))
    }

    fn status(&self, _id: DownloadId) -> Option<DownloadStatus> {
        None
    }

    fn detail(&self, _id: DownloadId) -> Option<EngineDetail> {
        None
    }

    fn set_download_limit(&self, _id: DownloadId, _bps: Option<u64>) -> Result<(), EngineError> {
        Err(EngineError::Unsupported(NOT_YET))
    }

    fn set_global_download_budget(&self, _bps: Option<u64>) {}

    fn subscribe(&self) -> broadcast::Receiver<EngineEvent> {
        self.events.subscribe()
    }
}
