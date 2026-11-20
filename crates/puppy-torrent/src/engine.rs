//! `TorrentEngine`: one librqbit [`Session`] configured for puppy behind the `Engine` trait.

use std::net::{Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
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
        create_private_dir(&opts.data_dir.join("torrent"))?;
        let plan = SessionPlan::from_options(&opts);
        let (session, cancel) = plan
            .start_with_fallback()
            .await
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

/// `<data_dir>/torrent/`: owner-only on Unix (it holds the DHT state and, later, the
/// stored `.torrent` copies).
fn create_private_dir(p: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(p)?;
        // Tighten the leaf even if it pre-existed with looser permissions.
        std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o700))
    }
    #[cfg(not(unix))]
    {
        std::fs::create_dir_all(p)
    }
}

/// What the session is started with, derived from [`TorrentEngineOptions`].
#[derive(Debug, Clone)]
struct SessionPlan {
    default_dir: PathBuf,
    listen_addr: SocketAddr,
    /// DHT state file; `None`: DHT off.
    dht_state: Option<PathBuf>,
    trackers: bool,
    lsd: bool,
    upnp: bool,
}

impl SessionPlan {
    fn from_options(opts: &TorrentEngineOptions) -> Self {
        let test = opts.test_mode;
        Self {
            default_dir: opts.default_dir.clone(),
            listen_addr: if test {
                (Ipv4Addr::LOCALHOST, 0).into()
            } else {
                (Ipv4Addr::UNSPECIFIED, opts.listen_port).into()
            },
            dht_state: (opts.dht && !test).then(|| opts.data_dir.join("torrent").join("dht.json")),
            trackers: !test,
            lsd: !test,
            upnp: opts.upnp && !test,
        }
    }

    /// Starts the session on the planned port; if that port can't be bound (in use, or
    /// privileged), once more on an OS-assigned one. Any other error is returned as is.
    async fn start_with_fallback(&self) -> anyhow::Result<(Arc<Session>, CancellationToken)> {
        match self.start(self.listen_addr).await {
            Err(e) if self.listen_addr.port() != 0 && is_port_unavailable(&e) => {
                tracing::warn!(
                    addr = %self.listen_addr,
                    "torrent listen port unavailable ({e:#}); using an OS-assigned port"
                );
                let mut any_port = self.listen_addr;
                any_port.set_port(0);
                self.start(any_port).await
            }
            r => r,
        }
    }

    /// One attempt, with its own cancellation token: cancelled if the attempt fails, so
    /// nothing it started (listener, DHT) outlives it.
    async fn start(
        &self,
        listen_addr: SocketAddr,
    ) -> anyhow::Result<(Arc<Session>, CancellationToken)> {
        let cancel = CancellationToken::new();
        let dht = self.dht_state.as_ref().map(|state| DhtSessionConfig {
            bootstrap_addrs: None,
            port: None,
            // The DHT routing table (not torrents) is kept between runs for a quick bootstrap.
            persistence: Some(DhtPersistenceConfig {
                dump_interval: None,
                config_filename: Some(state.clone()),
            }),
        });
        let session_opts = SessionOptions {
            dht,
            disable_trackers: !self.trackers,
            disable_local_service_discovery: !self.lsd,
            persistence: None,
            fastresume: false,
            // IPv4 only (listen on 0.0.0.0; DHT/LSD on IPv4): works on IPv6-less hosts.
            ipv4_only: true,
            listen: Some(ListenerOptions {
                listen_addr,
                enable_upnp_port_forwarding: self.upnp,
                ipv4_only: true,
                ..Default::default()
            }),
            cancellation_token: Some(cancel.clone()),
            ..Default::default()
        };
        match Session::new_with_opts(self.default_dir.clone(), session_opts).await {
            Ok(session) => Ok((session, cancel)),
            Err(e) => {
                cancel.cancel();
                Err(e)
            }
        }
    }
}

/// Whether a session start failed because the listen port can't be bound. librqbit wraps
/// the socket error in `librqbit_dualstack_sockets::Error::Bind`, whose io::Error is not
/// exposed as a `source()`, so both forms are looked for along the chain.
fn is_port_unavailable(e: &anyhow::Error) -> bool {
    use std::io::ErrorKind::{AddrInUse, PermissionDenied};
    e.chain().any(|cause| {
        let io = cause.downcast_ref::<std::io::Error>().or_else(|| {
            match cause.downcast_ref::<librqbit_dualstack_sockets::Error>() {
                Some(librqbit_dualstack_sockets::Error::Bind(io)) => Some(io),
                _ => None,
            }
        });
        io.is_some_and(|io| matches!(io.kind(), AddrInUse | PermissionDenied))
    })
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    fn local_plan(dir: &Path, listen_addr: SocketAddr) -> SessionPlan {
        SessionPlan {
            default_dir: dir.to_owned(),
            listen_addr,
            dht_state: None,
            trackers: false,
            lsd: false,
            upnp: false,
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn busy_listen_port_falls_back_without_leaking() {
        let dir = tempfile::tempdir().unwrap();
        let busy = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let busy_addr = busy.local_addr().unwrap();

        let (session, cancel) = local_plan(dir.path(), busy_addr)
            .start_with_fallback()
            .await
            .expect("falls back to an OS-assigned port");
        let got = session.listen_addr().unwrap();
        assert!(got.ip().is_loopback());
        assert_ne!(got.port(), busy_addr.port());

        // Nothing from the failed attempt holds the busy port once its owner lets go.
        drop(busy);
        assert!(
            TcpListener::bind(busy_addr).is_ok(),
            "{busy_addr} held by a leaked listener"
        );
        cancel.cancel();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn other_start_errors_are_not_retried() {
        // An address no interface has: not "in use", so no silent fallback.
        let dir = tempfile::tempdir().unwrap();
        let addr: SocketAddr = "192.0.2.1:6881".parse().unwrap();
        let err = local_plan(dir.path(), addr)
            .start_with_fallback()
            .await
            .err()
            .expect("start fails");
        assert!(!is_port_unavailable(&err), "{err:#}");
    }

    #[test]
    fn port_unavailable_detection() {
        use std::io::{Error, ErrorKind};
        let wrapped = |kind| {
            anyhow::Error::from(librqbit_dualstack_sockets::Error::Bind(Error::from(kind)))
                .context("error starting TCP listener")
                .context("error starting listeners")
        };
        assert!(is_port_unavailable(&wrapped(ErrorKind::AddrInUse)));
        assert!(is_port_unavailable(&wrapped(ErrorKind::PermissionDenied)));
        assert!(!is_port_unavailable(&wrapped(ErrorKind::AddrNotAvailable)));
        assert!(is_port_unavailable(&anyhow::Error::from(Error::from(
            ErrorKind::AddrInUse
        ))));
        assert!(!is_port_unavailable(&anyhow::anyhow!(
            "error initializing DHT"
        )));
    }
}
