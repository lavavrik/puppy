//! Torrents through the whole daemon: `run_daemon` in-process (its torrent engine
//! network-free via `DaemonOptions::torrent_test`), driven by the client, downloading
//! from a local seeder on 127.0.0.1.

// The torrent engine's local seeder harness (librqbit is a dev-dependency here only).
#[path = "../../puppy-torrent/tests/support/mod.rs"]
mod support;

use puppy_client::Client;
use puppy_core::config::HttpConfig;
use puppy_core::types::{AddRequest, DownloadId, RequestContext, Source};
use puppy_daemon::lifecycle::{
    DaemonOptions, RunOutcome, TorrentProbe, TorrentTestOptions, run_daemon,
};
use puppy_daemon::{Paths, SocketName};
use puppy_proto::{DownloadSnapshot, EngineKindTag, ReplyData, Request, WireState};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};
use support::{MULTI_NAME, Seeder};
use tokio::task::JoinHandle;

const TIMEOUT: Duration = Duration::from_secs(20);
const KIB: usize = 1024;
/// A multi-file torrent of ~1 MiB.
const MULTI: &[(&str, usize)] = &[("a.bin", 600 * KIB), ("sub/b.bin", 400 * KIB + 123)];
/// Seeder upload limit: ~1 MiB takes ~4 s, so the download can be watched mid-way.
const SLOW: u32 = 256 * KIB as u32;

type Daemon = JoinHandle<anyhow::Result<RunOutcome>>;

/// A short, unique socket name (macOS `sun_path` is ~104 bytes).
fn socket_name() -> SocketName {
    static N: AtomicUsize = AtomicUsize::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    if cfg!(windows) {
        SocketName::Pipe(format!("puppy-tt-{pid}-{n}"))
    } else {
        SocketName::Path(PathBuf::from(format!("/tmp/pz-{pid}-t{n}.sock")))
    }
}

struct Env {
    home: tempfile::TempDir,
    socket: SocketName,
    peers: Vec<SocketAddr>,
    probe: TorrentProbe,
}

impl Env {
    fn new(seeder: &Seeder) -> Self {
        Env {
            home: tempfile::tempdir().unwrap(),
            socket: socket_name(),
            peers: vec![seeder.addr],
            probe: TorrentProbe::default(),
        }
    }

    fn dl_dir(&self) -> PathBuf {
        self.home.path().join("dl")
    }

    fn opts(&self) -> DaemonOptions {
        DaemonOptions {
            idle_exit: None,
            paths: Some(Paths::with_home(self.home.path(), self.socket.clone()).unwrap()),
            http: Some(HttpConfig {
                default_dir: self.dl_dir(),
                ..HttpConfig::default()
            }),
            engine: None,
            torrent_test: Some(TorrentTestOptions {
                initial_peers: self.peers.clone(),
                probe: self.probe.clone(),
            }),
        }
    }

    /// Starts a daemon and returns it with a connected client.
    async fn start(&self) -> (Daemon, Client) {
        let d = tokio::spawn(run_daemon(self.opts()));
        let deadline = Instant::now() + TIMEOUT;
        loop {
            assert!(!d.is_finished(), "daemon exited early: {:?}", d.await);
            if let Ok(c) = Client::connect(&self.socket, "test").await {
                return (d, c);
            }
            assert!(Instant::now() < deadline, "daemon never listened");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    async fn shutdown(&self, d: Daemon, c: Client) {
        assert_eq!(c.call(Request::Shutdown).await.unwrap(), ReplyData::Done);
        drop(c);
        let started = Instant::now();
        let r = tokio::time::timeout(TIMEOUT, d)
            .await
            .expect("daemon did not exit")
            .unwrap()
            .unwrap();
        assert_eq!(r, RunOutcome::Exited);
        assert!(
            started.elapsed() < Duration::from_secs(12),
            "shutdown took {:?}",
            started.elapsed()
        );
    }

    /// Writes the seeder's `.torrent` into the home dir and returns a request for it.
    fn torrent_req(&self, seeder: &Seeder) -> AddRequest {
        let path = self.home.path().join("in.torrent");
        std::fs::write(&path, &seeder.torrent_bytes).unwrap();
        AddRequest {
            source: Source::TorrentFile { path },
            output_dir: Some(self.dl_dir()),
            filename: None,
            context: RequestContext::default(),
            files: Default::default(),
            seed: None,
        }
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        if let SocketName::Path(p) = &self.socket {
            let _ = std::fs::remove_file(p);
        }
    }
}

async fn snapshot(c: &Client, id: DownloadId) -> DownloadSnapshot {
    c.list()
        .await
        .unwrap()
        .into_iter()
        .find(|d| d.id == id)
        .expect("download listed")
}

/// Polls the list until `pred` holds for `id`; returns every snapshot seen.
async fn watch(
    c: &Client,
    id: DownloadId,
    what: &str,
    pred: impl Fn(&DownloadSnapshot) -> bool,
) -> Vec<DownloadSnapshot> {
    let deadline = Instant::now() + TIMEOUT;
    let mut seen = Vec::new();
    loop {
        let d = snapshot(c, id).await;
        let done = pred(&d);
        seen.push(d);
        if done {
            return seen;
        }
        if let WireState::Failed { reason, .. } = &seen.last().unwrap().state {
            panic!("download failed: {reason}");
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {what}: {:?}",
            seen.last()
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn assert_files(dir: &Path) {
    for (rel, len) in MULTI {
        let got = std::fs::read(dir.join(rel)).unwrap_or_else(|e| panic!("reading {rel}: {e}"));
        assert!(got == support::content(rel, *len), "{rel} differs");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn daemon_downloads_local_torrent() {
    let seeder = Seeder::start_with_limit(MULTI, SLOW).await;
    let env = Env::new(&seeder);
    let (d, c) = env.start().await;

    let id = c.add(env.torrent_req(&seeder), false, None).await.unwrap();
    let seen = watch(&c, id, "completion", |s| s.state == WireState::Completed).await;
    let done = seen.last().unwrap();
    assert_eq!(done.kind, EngineKindTag::Torrent);
    assert_eq!(done.name, MULTI_NAME);
    assert_eq!(done.path.as_deref(), Some(&*env.dl_dir().join(MULTI_NAME)));
    let total = MULTI.iter().map(|f| f.1 as u64).sum::<u64>();
    assert_eq!((done.done_bytes, done.total_bytes), (total, Some(total)));
    assert!(
        seen.iter()
            .any(|s| s.state == WireState::Active && s.peers.unwrap_or(0) >= 1),
        "peers reported while downloading: {seen:?}"
    );
    assert_files(&env.dl_dir().join(MULTI_NAME));

    env.shutdown(d, c).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn daemon_restart_restores_torrent() {
    let seeder = Seeder::start_with_limit(MULTI, SLOW).await;
    let env = Env::new(&seeder);
    let (d, c) = env.start().await;
    let id = c.add(env.torrent_req(&seeder), false, None).await.unwrap();
    watch(&c, id, "transferring", |s| {
        s.state == WireState::Active && s.done_bytes > 0
    })
    .await;
    env.shutdown(d, c).await;

    // Restarted: the torrent is restored (re-verified) and goes on to completion.
    let (d, c) = env.start().await;
    let list = c.list().await.unwrap();
    assert_eq!(list.len(), 1, "no duplicate: {list:?}");
    assert_ne!(list[0].state, WireState::Completed, "stopped mid-way");
    let seen = watch(&c, id, "completion", |s| s.state == WireState::Completed).await;
    assert_eq!(seen.last().unwrap().kind, EngineKindTag::Torrent);
    assert_files(&env.dl_dir().join(MULTI_NAME));
    env.shutdown(d, c).await;
}

const USER_DATA: &[u8] = b"the user's own data";

async fn remove(c: &Client, id: DownloadId, delete_files: bool) {
    assert_eq!(
        c.call(Request::Remove { id, delete_files }).await.unwrap(),
        ReplyData::Done
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn daemon_fresh_add_never_overwrites_user_files() {
    let seeder = Seeder::start(MULTI).await;
    let env = Env::new(&seeder);
    let mine = env.dl_dir().join(MULTI_NAME);
    std::fs::create_dir_all(mine.join("sub")).unwrap();
    for (rel, _) in MULTI {
        std::fs::write(mine.join(rel), USER_DATA).unwrap();
    }
    let (d, c) = env.start().await;

    let id = c.add(env.torrent_req(&seeder), false, None).await.unwrap();
    let seen = watch(&c, id, "completion", |s| s.state == WireState::Completed).await;
    let elsewhere = env.dl_dir().join(format!("{MULTI_NAME} (1)"));
    assert_eq!(seen.last().unwrap().path.as_deref(), Some(&*elsewhere));
    assert_files(&elsewhere);
    remove(&c, id, true).await;
    for (rel, _) in MULTI {
        assert_eq!(std::fs::read(mine.join(rel)).unwrap(), USER_DATA, "{rel}");
        assert!(!elsewhere.join(rel).exists(), "{rel} of the torrent left");
    }
    env.shutdown(d, c).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn daemon_remove_completed_torrent_deletes_its_files() {
    let seeder = Seeder::start(MULTI).await;
    let env = Env::new(&seeder);
    std::fs::create_dir_all(env.dl_dir()).unwrap();
    std::fs::write(env.dl_dir().join("unrelated.txt"), USER_DATA).unwrap();
    let (d, c) = env.start().await;

    let id = c.add(env.torrent_req(&seeder), false, None).await.unwrap();
    watch(&c, id, "completion", |s| s.state == WireState::Completed).await;
    let folder = env.dl_dir().join(MULTI_NAME);
    assert_files(&folder);
    // The manager drops completed downloads from the engine: wait for that.
    tokio::time::sleep(Duration::from_millis(1500)).await;
    remove(&c, id, true).await;
    for (rel, _) in MULTI {
        assert!(!folder.join(rel).exists(), "{rel} not deleted");
    }
    assert!(!folder.exists(), "the torrent's (now empty) folder is left");
    assert!(env.dl_dir().is_dir(), "the download dir itself is kept");
    assert_eq!(
        std::fs::read(env.dl_dir().join("unrelated.txt")).unwrap(),
        USER_DATA
    );
    let stored = env
        .home
        .path()
        .join("data/torrents")
        .join(format!("{id}.torrent"));
    assert!(!stored.exists(), "stored .torrent copy left after removal");
    env.shutdown(d, c).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn daemon_restart_restores_magnet_offline_with_its_name() {
    let seeder = Seeder::start_with_limit(MULTI, SLOW).await;
    let env = Env::new(&seeder);
    let (d, c) = env.start().await;
    let req = AddRequest {
        source: Source::Magnet {
            uri: format!("{}&dn=placeholder", seeder.magnet),
        },
        output_dir: Some(env.dl_dir()),
        filename: None,
        context: RequestContext::default(),
        files: Default::default(),
        seed: None,
    };
    let id = c.add(req, false, None).await.unwrap();
    let seen = watch(&c, id, "transferring", |s| {
        s.state == WireState::Active && s.done_bytes > 0
    })
    .await;
    assert_eq!(seen.last().unwrap().name, MULTI_NAME);
    env.shutdown(d, c).await;
    drop(seeder); // offline from now on

    let (d, c) = env.start().await;
    let seen = watch(&c, id, "verified data", |s| s.done_bytes > 0).await;
    assert!(
        seen.iter().all(|s| s.name == MULTI_NAME),
        "the placeholder name came back: {seen:?}"
    );
    assert_ne!(seen.last().unwrap().state, WireState::Completed);
    env.shutdown(d, c).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn http_only_daemon_never_starts_the_torrent_session() {
    let server = puppy_testserver::TestServer::start(puppy_testserver::ServerCfg {
        len: 64 * KIB as u64,
        ..Default::default()
    })
    .await;
    let env = Env {
        home: tempfile::tempdir().unwrap(),
        socket: socket_name(),
        peers: Vec::new(),
        probe: TorrentProbe::default(),
    };
    let (d, c) = env.start().await;
    let req = AddRequest {
        source: Source::Http {
            url: server.url("/file.bin"),
        },
        output_dir: Some(env.dl_dir()),
        filename: Some("f.bin".into()),
        context: RequestContext::default(),
        files: Default::default(),
        seed: None,
    };
    let id = c.add(req, false, None).await.unwrap();
    watch(&c, id, "completion", |s| s.state == WireState::Completed).await;
    assert_eq!(
        env.probe.session_started(),
        Some(false),
        "an HTTP-only daemon opened torrent sockets"
    );
    env.shutdown(d, c).await;
}
