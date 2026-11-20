//! `TorrentEngine` add / pause / resume / remove / status against a local seeder.
//! Network-free: 127.0.0.1 only, DHT, trackers, LSD and UPnP off, peers given explicitly.

mod support;

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use librqbit::{AddTorrent, AddTorrentOptions, Session};
use puppy_core::engine::{AddOptions, Engine, EngineError, EngineEvent, StartMode};
use puppy_core::types::{
    AddRequest, DownloadId, DownloadState, DownloadStatus, RequestContext, Source,
};
use puppy_torrent::{TorrentEngine, TorrentEngineOptions};
use support::{MULTI_NAME, Seeder};
use tempfile::TempDir;

const TIMEOUT: Duration = Duration::from_secs(15);
const KIB: usize = 1024;
/// A multi-file torrent of ~1 MiB.
const MULTI: &[(&str, usize)] = &[("a.bin", 600 * KIB), ("sub/b.bin", 400 * KIB + 123)];

struct Env {
    dir: TempDir,
    engine: TorrentEngine,
}

impl Env {
    async fn new(peers: Vec<SocketAddr>) -> Env {
        Self::with(peers, 0.0).await
    }

    async fn with(peers: Vec<SocketAddr>, seed_ratio: f32) -> Env {
        let dir = tempfile::tempdir().unwrap();
        let engine = TorrentEngine::new(TorrentEngineOptions {
            dht: false,
            upnp: false,
            lsd: false,
            listen_port: 0,
            seed_ratio,
            data_dir: dir.path().join("data"),
            default_dir: dir.path().join("downloads"),
            test_mode: true,
            test_initial_peers: peers,
        })
        .await
        .expect("engine starts");
        Env { dir, engine }
    }

    fn out(&self) -> PathBuf {
        self.dir.path().join("out")
    }

    fn stored(&self, id: DownloadId) -> PathBuf {
        self.dir.path().join(format!("data/torrents/{id}.torrent"))
    }

    /// Writes the seeder's `.torrent` to a file and returns a request for it.
    fn torrent_req(&self, seeder: &Seeder) -> AddRequest {
        let path = self
            .dir
            .path()
            .join(format!("{}.torrent", seeder.info_hash_hex));
        std::fs::write(&path, &seeder.torrent_bytes).unwrap();
        self.req(Source::TorrentFile { path })
    }

    fn req(&self, source: Source) -> AddRequest {
        AddRequest {
            source,
            output_dir: Some(self.out()),
            filename: None,
            context: RequestContext::default(),
        }
    }

    async fn add(
        &self,
        id: DownloadId,
        req: AddRequest,
    ) -> Result<puppy_core::engine::Added, EngineError> {
        self.engine
            .add(id, req, StartMode::Fresh, AddOptions::default())
            .await
    }

    fn status(&self, id: DownloadId) -> DownloadStatus {
        self.engine.status(id).expect("download known")
    }

    async fn wait(
        &self,
        id: DownloadId,
        what: &str,
        pred: impl Fn(&DownloadStatus) -> bool,
    ) -> DownloadStatus {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            let st = self.status(id);
            if pred(&st) {
                return st;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {what}: {st:?}"
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    async fn wait_state(&self, id: DownloadId, state: DownloadState) -> DownloadStatus {
        self.wait(id, &format!("{state:?}"), |s| s.state == state)
            .await
    }
}

/// Asserts each `(relative path, len)` file is under `dir` with the seeded content.
fn assert_files(dir: &Path, files: &[(&str, usize)]) {
    for (rel, len) in files {
        let got = std::fs::read(dir.join(rel)).unwrap_or_else(|e| panic!("reading {rel}: {e}"));
        assert!(
            got == support::content(rel, *len),
            "{rel} differs from the seeded file"
        );
    }
}

/// Collects events for `id` until `Completed`.
async fn events_until_completed(
    rx: &mut tokio::sync::broadcast::Receiver<EngineEvent>,
    id: DownloadId,
) -> Vec<EngineEvent> {
    let mut got = Vec::new();
    let deadline = tokio::time::Instant::now() + TIMEOUT;
    loop {
        let ev = tokio::time::timeout_at(deadline, rx.recv())
            .await
            .expect("timed out waiting for Completed")
            .expect("event channel open");
        let done = matches!(ev, EngineEvent::Completed { id: i } if i == id);
        got.push(ev);
        if done {
            return got;
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn torrent_file_downloads_to_completion() {
    let seeder = Seeder::start(MULTI).await;
    let env = Env::new(vec![seeder.addr]).await;
    let mut rx = env.engine.subscribe();

    let added = env.add(1, env.torrent_req(&seeder)).await.unwrap();
    assert_eq!(added.name, MULTI_NAME);
    assert_eq!(
        added.path,
        env.out().join(MULTI_NAME),
        "multi-file: its own folder"
    );
    let total = MULTI.iter().map(|f| f.1 as u64).sum::<u64>();
    assert_eq!(added.total_bytes, Some(total));
    assert!(env.stored(1).exists(), "the .torrent is kept for restore");
    assert_eq!(std::fs::read(env.stored(1)).unwrap(), seeder.torrent_bytes);

    let events = events_until_completed(&mut rx, 1).await;
    let states: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            EngineEvent::StateChanged { id: 1, state } => Some(state.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(states.last(), Some(&DownloadState::Completed), "{events:?}");
    let st = env.status(1);
    assert_eq!(st.state, DownloadState::Completed);
    assert_eq!((st.done_bytes, st.total_bytes), (total, Some(total)));
    assert_eq!(st.path, env.out().join(MULTI_NAME));
    assert_eq!(st.name, MULTI_NAME);
    assert_files(&env.out().join(MULTI_NAME), MULTI);

    // `Completed` once; the state stays put.
    tokio::time::sleep(Duration::from_millis(1200)).await;
    if let Ok(ev) = rx.try_recv() {
        panic!("unexpected event after completion: {ev:?}");
    }
    assert_eq!(env.status(1).state, DownloadState::Completed);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn single_file_torrent_goes_into_output_dir_and_remove_keeps_the_dir() {
    let seeder = Seeder::start(&[("one.iso", 300 * KIB)]).await;
    let env = Env::new(vec![seeder.addr]).await;
    let added = env.add(1, env.torrent_req(&seeder)).await.unwrap();
    assert_eq!(added.name, "one.iso");
    assert_eq!(
        added.path,
        env.out().join("one.iso"),
        "directly in the output dir"
    );
    env.wait_state(1, DownloadState::Completed).await;
    assert_files(&env.out(), &[("one.iso", 300 * KIB)]);

    env.engine.remove(1, true).await.unwrap();
    assert!(!env.out().join("one.iso").exists(), "file deleted");
    assert!(
        env.out().is_dir(),
        "the (now empty) output dir itself is kept"
    );
    assert!(!env.stored(1).exists());
    assert!(env.engine.status(1).is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn magnet_with_initial_peer_downloads() {
    let seeder = Seeder::start(MULTI).await;
    let env = Env::new(vec![seeder.addr]).await;
    let hash = &seeder.info_hash_hex;

    let started = Instant::now();
    let added = env
        .add(
            1,
            env.req(Source::Magnet {
                uri: seeder.magnet.clone(),
            }),
        )
        .await
        .unwrap();
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "add waited for the metadata"
    );
    assert_eq!(added.name, format!("magnet {}", &hash[..8]));
    assert_eq!(
        added.path,
        env.out().join(hash),
        "a magnet gets its own folder"
    );
    assert_eq!(added.total_bytes, None);
    assert!(!env.stored(1).exists());

    let st = env.wait_state(1, DownloadState::Completed).await;
    assert_eq!(
        st.name, MULTI_NAME,
        "the torrent's name once the metadata is known"
    );
    assert_eq!(st.path, env.out().join(hash));
    let total = MULTI.iter().map(|f| f.1 as u64).sum::<u64>();
    assert_eq!((st.done_bytes, st.total_bytes), (total, Some(total)));
    assert_files(&env.out().join(hash), MULTI);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pause_and_resume() {
    // ~1 MiB at 256 KiB/s: several seconds, so the download can be caught mid-way.
    let seeder = Seeder::start_with_limit(MULTI, 256 * KIB as u32).await;
    let env = Env::new(vec![seeder.addr]).await;
    env.add(1, env.torrent_req(&seeder)).await.unwrap();

    let st = env
        .wait(1, "transferring", |s| {
            s.state == DownloadState::Active && s.done_bytes > 0
        })
        .await;
    assert!(st.peers.unwrap_or(0) >= 1, "{st:?}");
    env.engine.pause(1).await.unwrap();
    let paused = env.status(1);
    assert_eq!(paused.state, DownloadState::Paused);
    assert_eq!((paused.down_bps, paused.eta_secs), (0, None));
    env.engine.pause(1).await.expect("pausing twice is fine");
    tokio::time::sleep(Duration::from_millis(700)).await;
    let later = env.status(1);
    assert_eq!(later.state, DownloadState::Paused);
    assert_eq!(
        later.done_bytes, paused.done_bytes,
        "no progress while paused"
    );
    assert!(later.done_bytes < later.total_bytes.unwrap());

    env.engine.resume(1).await.unwrap();
    env.wait_state(1, DownloadState::Completed).await;
    assert_files(&env.out().join(MULTI_NAME), MULTI);
    env.engine
        .resume(1)
        .await
        .expect("resuming a completed torrent is a no-op");
    assert_eq!(env.status(1).state, DownloadState::Completed);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn remove_multi_file_deletes_only_its_files() {
    let seeder = Seeder::start(MULTI).await;
    let env = Env::new(vec![seeder.addr]).await;
    std::fs::create_dir_all(env.out()).unwrap();
    std::fs::write(env.out().join("keep.txt"), b"not ours").unwrap();
    std::fs::create_dir_all(env.out().join("other")).unwrap();

    env.add(1, env.torrent_req(&seeder)).await.unwrap();
    env.wait_state(1, DownloadState::Completed).await;
    // Something of the user's inside the torrent's folder survives too.
    let foreign = env.out().join(MULTI_NAME).join("notes.txt");
    std::fs::write(&foreign, b"mine").unwrap();

    env.engine.remove(1, true).await.unwrap();
    assert!(env.engine.status(1).is_none());
    assert!(!env.stored(1).exists(), "stored .torrent copy removed");
    let folder = env.out().join(MULTI_NAME);
    for (rel, _) in MULTI {
        assert!(!folder.join(rel).exists(), "{rel} not deleted");
    }
    assert!(
        !folder.join("sub").exists(),
        "empty sub-folder of the torrent removed"
    );
    assert!(
        foreign.exists(),
        "a file that is not the torrent's was deleted"
    );
    assert!(env.out().join("keep.txt").exists());
    assert!(env.out().join("other").is_dir());

    // Without delete_files, the files stay.
    let env2 = Env::new(vec![seeder.addr]).await;
    env2.add(2, env2.torrent_req(&seeder)).await.unwrap();
    env2.wait_state(2, DownloadState::Completed).await;
    env2.engine.remove(2, false).await.unwrap();
    assert_files(&env2.out().join(MULTI_NAME), MULTI);
    assert!(matches!(
        env2.engine.remove(2, false).await,
        Err(EngineError::NotFound(2))
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn duplicate_infohash_rejected() {
    let seeder = Seeder::start_with_limit(MULTI, 256 * KIB as u32).await;
    let env = Env::new(vec![seeder.addr]).await;
    env.add(1, env.torrent_req(&seeder)).await.unwrap();

    for (id, source) in [
        (2, env.torrent_req(&seeder).source),
        (
            3,
            Source::Magnet {
                uri: seeder.magnet.clone(),
            },
        ),
        (
            4,
            Source::Magnet {
                uri: format!(
                    "magnet:?xt=urn:btih:{}&dn=x",
                    seeder.info_hash_hex.to_uppercase()
                ),
            },
        ),
    ] {
        match env.add(id, env.req(source)).await {
            Err(EngineError::Invalid(m)) => assert_eq!(m, "already downloading this torrent"),
            other => panic!("duplicate {id} accepted: {other:?}"),
        }
        assert!(env.engine.status(id).is_none());
        assert!(!env.stored(id).exists());
    }
    // The same id twice is refused as well.
    assert!(matches!(
        env.add(
            1,
            env.req(Source::Magnet {
                uri: format!("magnet:?xt=urn:btih:{}", "1".repeat(40))
            })
        )
        .await,
        Err(EngineError::Invalid(_))
    ));

    // The first one is untouched.
    assert!(env.stored(1).exists());
    env.wait_state(1, DownloadState::Completed).await;
    assert_files(&env.out().join(MULTI_NAME), MULTI);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn invalid_sources_fail_cleanly() {
    let env = Env::new(Vec::new()).await;
    let garbage = env.dir.path().join("garbage.torrent");
    std::fs::write(&garbage, b"this is not bencode").unwrap();
    let huge = env.dir.path().join("huge.torrent");
    std::fs::File::create(&huge)
        .unwrap()
        .set_len(16 * 1024 * 1024 + 1)
        .unwrap();

    let started = Instant::now();
    for (source, want) in [
        (
            Source::TorrentFile { path: garbage },
            "not a valid torrent file",
        ),
        (Source::TorrentFile { path: huge }, "too large"),
        (
            Source::Magnet {
                uri: "magnet:?xt=bad".into(),
            },
            "invalid magnet link",
        ),
        (
            Source::Magnet {
                uri: "magnet:?dn=no-hash".into(),
            },
            "invalid magnet link",
        ),
    ] {
        match env.add(1, env.req(source.clone())).await {
            Err(EngineError::Invalid(m)) => assert!(m.contains(want), "{source:?}: {m}"),
            other => panic!("{source:?} accepted: {other:?}"),
        }
        assert!(env.engine.status(1).is_none(), "{source:?} left an entry");
        assert!(!env.stored(1).exists(), "{source:?} left a stored copy");
    }
    let missing = Source::TorrentFile {
        path: env.dir.path().join("missing.torrent"),
    };
    assert!(matches!(
        env.add(1, env.req(missing)).await,
        Err(EngineError::Io(_))
    ));
    assert!(started.elapsed() < Duration::from_secs(2), "fails fast");
    assert!(!env.out().exists(), "nothing created in the output dir");

    // The id was released each time: a valid add with it works.
    let seeder = Seeder::start(&[("x.bin", 50 * KIB)]).await;
    env.add(1, env.torrent_req(&seeder))
        .await
        .expect("id 1 is free");
}

/// A peer that accepts connections and never says anything: metadata never arrives
/// (librqbit gives up on it after its 10 s read timeout).
async fn silent_peer() -> (SocketAddr, tokio::task::JoinHandle<()>) {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    let task = tokio::spawn(async move {
        let mut held = Vec::new();
        while let Ok((s, _)) = l.accept().await {
            held.push(s);
        }
    });
    (addr, task)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn magnet_without_peers_stays_starting_and_removable() {
    let (silent, _peer) = silent_peer().await;
    let env = Env::new(vec![silent]).await;
    let hash = "0123456789abcdef0123456789abcdef01234567";
    let uri = format!("magnet:?xt=urn:btih:{hash}&dn=Some%20Show");

    let started = Instant::now();
    let added = env
        .add(1, env.req(Source::Magnet { uri: uri.clone() }))
        .await
        .unwrap();
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "add waited for the metadata"
    );
    assert_eq!(added.name, "Some Show");
    assert_eq!(added.path, env.out().join("Some Show"));
    assert_eq!(added.total_bytes, None);

    for _ in 0..6 {
        let st = env.status(1);
        assert_eq!(st.state, DownloadState::Starting);
        assert_eq!(
            (st.name.as_str(), st.total_bytes, st.done_bytes),
            ("Some Show", None, 0)
        );
        tokio::time::sleep(Duration::from_millis(250)).await;
    }

    env.engine.pause(1).await.unwrap();
    assert_eq!(env.status(1).state, DownloadState::Paused);
    env.engine.resume(1).await.unwrap();
    assert_eq!(env.status(1).state, DownloadState::Starting);

    let started = Instant::now();
    env.engine.remove(1, true).await.unwrap();
    assert!(started.elapsed() < Duration::from_secs(1));
    assert!(env.engine.status(1).is_none());
    assert!(!env.out().join("Some Show").exists());

    // The info hash is free again; a dn-less magnet is named after its hash.
    let added = env
        .add(
            2,
            env.req(Source::Magnet {
                uri: format!("magnet:?xt=urn:btih:{hash}"),
            }),
        )
        .await
        .unwrap();
    assert_eq!(added.name, "magnet 01234567");
    assert_eq!(added.path, env.out().join(hash));
    env.engine.remove(2, false).await.unwrap();

    // Added paused: nothing is fetched until resumed.
    env.engine
        .add(
            3,
            env.req(Source::Magnet { uri }),
            StartMode::Fresh,
            AddOptions {
                start_paused: true,
                limit: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(env.status(3).state, DownloadState::Paused);
    env.engine.resume(3).await.unwrap();
    assert_eq!(env.status(3).state, DownloadState::Starting);
    env.engine.remove(3, true).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn magnet_metadata_failure_is_transient_and_retryable() {
    // The only peer refuses connections: the fetch fails, but could work later.
    let refused = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap();
    let env = Env::new(vec![refused]).await;
    let uri = format!("magnet:?xt=urn:btih:{}", "ab".repeat(20));
    env.add(1, env.req(Source::Magnet { uri })).await.unwrap();
    let st = env
        .wait(1, "failed", |s| {
            matches!(s.state, DownloadState::Failed { .. })
        })
        .await;
    assert!(
        matches!(&st.state, DownloadState::Failed { transient: true, reason }
            if reason.starts_with("fetching the torrent metadata")),
        "{st:?}"
    );
    // Pausing a failed magnet keeps the failure.
    env.engine.pause(1).await.unwrap();
    assert_eq!(env.status(1).state, st.state);
    env.engine.resume(1).await.expect("retry");
    env.wait(1, "failed again", |s| {
        matches!(s.state, DownloadState::Failed { .. })
    })
    .await;
    env.engine.remove(1, true).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn magnet_without_any_peer_source_fails_permanently() {
    // Test mode has no DHT and no trackers; with no peers either nothing can ever work.
    let env = Env::new(Vec::new()).await;
    let uri = format!("magnet:?xt=urn:btih:{}", "cd".repeat(20));
    env.add(1, env.req(Source::Magnet { uri })).await.unwrap();
    let st = env
        .wait(1, "failed", |s| {
            matches!(s.state, DownloadState::Failed { .. })
        })
        .await;
    assert!(
        matches!(&st.state, DownloadState::Failed { transient: false, reason }
            if reason.contains("no known way to resolve peers")),
        "{st:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn magnet_with_unsafe_file_names_is_refused_before_writing() {
    // A seeder serving a torrent with a file named like a Windows device: the magnet's
    // metadata is fetched, checked and refused before anything is written.
    let seeder = Seeder::start(&[("dir/con.txt", 20 * KIB), ("dir/ok.bin", 20 * KIB)]).await;
    let env = Env::new(vec![seeder.addr]).await;
    let added = env
        .add(
            1,
            env.req(Source::Magnet {
                uri: seeder.magnet.clone(),
            }),
        )
        .await
        .unwrap();
    let st = env
        .wait(1, "failed", |s| {
            matches!(s.state, DownloadState::Failed { .. })
        })
        .await;
    assert!(
        matches!(&st.state, DownloadState::Failed { transient: false, reason }
            if reason.starts_with("unsafe file name in torrent")),
        "{st:?}"
    );
    assert!(!added.path.exists(), "files written for a refused torrent");
    env.engine.remove(1, true).await.unwrap();
}

/// Starts a raw librqbit leecher of the seeder's torrent whose only peer is `peer`.
async fn leech_from(
    seeder: &Seeder,
    peer: SocketAddr,
    dir: &Path,
) -> (
    std::sync::Arc<Session>,
    std::sync::Arc<librqbit::ManagedTorrent>,
) {
    let session =
        Session::new_with_opts(dir.to_owned(), support::local_session_options(false, None))
            .await
            .unwrap();
    let handle = session
        .add_torrent(
            AddTorrent::TorrentFileBytes(seeder.torrent_bytes.clone()),
            Some(AddTorrentOptions {
                overwrite: true,
                disable_trackers: true,
                initial_peers: Some(vec![peer]),
                ..Default::default()
            }),
        )
        .await
        .unwrap()
        .into_handle()
        .unwrap();
    (session, handle)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn completion_stops_seeding() {
    let seeder = Seeder::start(&[("s.bin", 200 * KIB)]).await;

    // Default policy (seed ratio 0): once Completed, nothing is served to other peers.
    let env = Env::new(vec![seeder.addr]).await;
    let mut rx = env.engine.subscribe();
    env.add(1, env.torrent_req(&seeder)).await.unwrap();
    events_until_completed(&mut rx, 1).await;
    let ours = env.engine.listen_addr().unwrap();
    let leech_dir = tempfile::tempdir().unwrap();
    let (session, h) = leech_from(&seeder, ours, leech_dir.path()).await;
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(
        h.stats().progress_bytes,
        0,
        "the completed torrent still seeds"
    );
    assert_eq!(env.status(1).state, DownloadState::Completed);
    session.cancellation_token().cancel();

    // Control: with a non-zero ratio (reserved: keeps seeding) the same leecher completes,
    // so the check above can fail.
    let env = Env::with(vec![seeder.addr], 1.0).await;
    let mut rx = env.engine.subscribe();
    env.add(1, env.torrent_req(&seeder)).await.unwrap();
    events_until_completed(&mut rx, 1).await;
    let ours = env.engine.listen_addr().unwrap();
    let leech_dir = tempfile::tempdir().unwrap();
    let (session, h) = leech_from(&seeder, ours, leech_dir.path()).await;
    tokio::time::timeout(Duration::from_secs(10), h.wait_until_completed())
        .await
        .expect("served by the seeding engine")
        .unwrap();
    session.cancellation_token().cancel();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unknown_ids_are_not_found() {
    let env = Env::new(Vec::new()).await;
    assert!(env.engine.status(9).is_none());
    assert!(matches!(
        env.engine.pause(9).await,
        Err(EngineError::NotFound(9))
    ));
    assert!(matches!(
        env.engine.resume(9).await,
        Err(EngineError::NotFound(9))
    ));
    assert!(matches!(
        env.engine.remove(9, true).await,
        Err(EngineError::NotFound(9))
    ));
    let http = Source::Http {
        url: "http://example.com/a".parse().unwrap(),
    };
    assert!(matches!(
        env.add(9, env.req(http)).await,
        Err(EngineError::Unsupported(_))
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unsafe_file_names_are_refused_on_every_os() {
    let env = Env::new(Vec::new()).await;
    // Sanity: the crafted format itself is accepted.
    let ok = support::craft_torrent("fine", 0, Some(&[(&["dir", "a.bin"], 100)]));
    let ok_path = env.dir.path().join("ok.torrent");
    std::fs::write(&ok_path, ok).unwrap();
    env.add(1, env.req(Source::TorrentFile { path: ok_path }))
        .await
        .expect("a well-formed crafted torrent is accepted");
    env.engine.remove(1, true).await.unwrap();

    let cases: [(&str, Vec<u8>); 5] = [
        (
            "drive prefix",
            support::craft_torrent("x", 0, Some(&[(&["C:evil"], 10)])),
        ),
        (
            "device name",
            support::craft_torrent("x", 0, Some(&[(&["sub", "con.txt"], 10)])),
        ),
        (
            "stream",
            support::craft_torrent("x", 0, Some(&[(&["a:b"], 10)])),
        ),
        (
            "single-file drive prefix",
            support::craft_torrent("C:evil", 10, None),
        ),
        (
            "single-file device name",
            support::craft_torrent("NUL", 10, None),
        ),
    ];
    for (i, (what, bytes)) in cases.into_iter().enumerate() {
        let id = 10 + i as DownloadId;
        let path = env.dir.path().join(format!("{i}.torrent"));
        std::fs::write(&path, bytes).unwrap();
        match env.add(id, env.req(Source::TorrentFile { path })).await {
            Err(EngineError::Invalid(m)) => {
                assert!(m.starts_with("unsafe file name in torrent"), "{what}: {m}")
            }
            other => panic!("{what}: accepted: {other:?}"),
        }
        assert!(env.engine.status(id).is_none());
        assert!(!env.stored(id).exists());
    }
    assert!(!env.dir.path().join("C:evil").exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fresh_adds_never_overwrite_existing_files() {
    let single = Seeder::start(&[("one.iso", 100 * KIB)]).await;
    let multi = Seeder::start(MULTI).await;
    let env = Env::new(vec![single.addr, multi.addr]).await;
    std::fs::create_dir_all(env.out().join(MULTI_NAME).join("sub")).unwrap();
    let user = |p: &Path| std::fs::write(p, b"the user's own data").unwrap();
    user(&env.out().join("one.iso"));
    user(&env.out().join(MULTI_NAME).join("a.bin"));
    user(&env.out().join(MULTI_NAME).join("sub/b.bin"));
    // A magnet's folder name taken by a user's file.
    let hash = &multi.info_hash_hex;
    user(&env.out().join(hash));

    // Single-file: its own fresh folder, since `out/one.iso` is taken.
    let added = env.add(1, env.torrent_req(&single)).await.unwrap();
    assert_eq!(added.path, env.out().join("one.iso (1)").join("one.iso"));
    // Multi-file: a fresh folder next to the existing one.
    let added = env.add(2, env.torrent_req(&multi)).await.unwrap();
    assert_eq!(added.path, env.out().join(format!("{MULTI_NAME} (1)")));
    env.wait_state(1, DownloadState::Completed).await;
    env.wait_state(2, DownloadState::Completed).await;
    assert_files(&env.out().join("one.iso (1)"), &[("one.iso", 100 * KIB)]);
    assert_files(&env.out().join(format!("{MULTI_NAME} (1)")), MULTI);
    env.engine.remove(2, true).await.unwrap();

    let added = env
        .add(
            3,
            env.req(Source::Magnet {
                uri: multi.magnet.clone(),
            }),
        )
        .await
        .unwrap();
    assert_eq!(added.path, env.out().join(format!("{hash} (1)")));
    env.wait_state(3, DownloadState::Completed).await;
    assert_files(&env.out().join(format!("{hash} (1)")), MULTI);

    for p in [
        env.out().join("one.iso"),
        env.out().join(MULTI_NAME).join("a.bin"),
        env.out().join(MULTI_NAME).join("sub/b.bin"),
        env.out().join(hash),
    ] {
        assert_eq!(
            std::fs::read(&p).unwrap(),
            b"the user's own data",
            "{} was touched",
            p.display()
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn completed_torrent_files_are_deleted_from_the_stored_copy() {
    let seeder = Seeder::start(MULTI).await;
    let env = Env::new(vec![seeder.addr]).await;
    std::fs::create_dir_all(env.out()).unwrap();
    std::fs::write(env.out().join("keep.txt"), b"mine").unwrap();
    let added = env.add(1, env.torrent_req(&seeder)).await.unwrap();
    env.wait_state(1, DownloadState::Completed).await;

    // Dropped like the daemon drops completed downloads: the copy stays.
    env.engine.drop_completed(1).await.unwrap();
    assert!(env.engine.status(1).is_none());
    assert!(
        env.stored(1).exists(),
        "the copy is needed to delete the files later"
    );
    assert_files(&added.path, MULTI);

    env.engine.delete_completed(1, &added.path).await.unwrap();
    assert!(
        !added.path.exists(),
        "the torrent's files and folder are gone"
    );
    assert!(env.out().join("keep.txt").exists());
    env.engine.forget(1).await;
    assert!(!env.stored(1).exists());

    // Without the copy nothing is deleted.
    let env2 = Env::new(vec![seeder.addr]).await;
    let added = env2.add(2, env2.torrent_req(&seeder)).await.unwrap();
    env2.wait_state(2, DownloadState::Completed).await;
    env2.engine.drop_completed(2).await.unwrap();
    env2.engine.forget(2).await;
    env2.engine.delete_completed(2, &added.path).await.unwrap();
    assert_files(&added.path, MULTI);
}
