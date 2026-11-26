//! `TorrentEngine` restore (`StartMode::Restore`) after an engine restart. Network-free:
//! 127.0.0.1 only, DHT, trackers, LSD and UPnP off, peers given explicitly.

mod support;

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use puppy_core::engine::{AddOptions, Added, Engine, EngineError, StartMode};
use puppy_core::types::{
    AddRequest, AttentionReason, DownloadId, DownloadState, DownloadStatus, RequestContext, Source,
};
use puppy_torrent::{TorrentEngine, TorrentEngineOptions};
use support::{MULTI_NAME, Seeder};

const TIMEOUT: Duration = Duration::from_secs(20);
const KIB: usize = 1024;
/// A multi-file torrent of ~1 MiB.
const MULTI: &[(&str, usize)] = &[("a.bin", 600 * KIB), ("sub/b.bin", 400 * KIB + 123)];
/// Seeder upload limit: ~1 MiB takes several seconds, so a download can be caught mid-way.
const SLOW: u32 = 256 * KIB as u32;

/// An engine over `root/data` (shared by the engines of one test: a restart keeps it).
async fn engine(root: &Path, peers: Vec<SocketAddr>) -> TorrentEngine {
    TorrentEngine::new(TorrentEngineOptions {
        dht: false,
        upnp: false,
        lsd: false,
        listen_port: 0,
        seed_ratio: 0.0,
        seed_time: std::time::Duration::ZERO,
        data_dir: root.join("data"),
        default_dir: root.join("downloads"),
        test_mode: true,
        test_initial_peers: peers,
    })
    .await
    .expect("engine starts")
}

fn req(root: &Path, source: Source) -> AddRequest {
    AddRequest {
        source,
        output_dir: Some(root.join("out")),
        filename: None,
        context: RequestContext::default(),
        files: Default::default(),
        seed: None,
        checksum: None,
        mirrors: Vec::new(),
    }
}

/// Writes the seeder's `.torrent` next to the downloads and returns a request for it.
fn torrent_req(root: &Path, seeder: &Seeder) -> AddRequest {
    let path = root.join(format!("{}.torrent", seeder.info_hash_hex));
    std::fs::write(&path, &seeder.torrent_bytes).unwrap();
    req(root, Source::TorrentFile { path })
}

fn stored(root: &Path, id: DownloadId) -> PathBuf {
    root.join(format!("data/torrents/{id}.torrent"))
}

async fn wait(
    engine: &TorrentEngine,
    id: DownloadId,
    what: &str,
    pred: impl Fn(&DownloadStatus) -> bool,
) -> DownloadStatus {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        let st = engine.status(id).expect("download known");
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

/// Adds `req` fresh, waits until part of it is downloaded, pauses it, shuts the engine
/// down and drops it. Returns what `add` returned and the bytes done when paused.
async fn download_part_then_stop(
    root: &Path,
    seeder: &Seeder,
    id: DownloadId,
    req: AddRequest,
) -> (Added, u64) {
    let first = engine(root, vec![seeder.addr]).await;
    let added = first
        .add(id, req, StartMode::Fresh, AddOptions::default())
        .await
        .unwrap();
    wait(&first, id, "transferring", |s| {
        s.state == DownloadState::Active && s.done_bytes > 0
    })
    .await;
    first.pause(id).await.unwrap();
    let paused = first.status(id).unwrap();
    assert_eq!(paused.state, DownloadState::Paused);
    let total = paused.total_bytes.unwrap();
    assert!(paused.done_bytes < total, "finished before the restart");
    first.shutdown().await;
    drop(first);
    (added, paused.done_bytes)
}

fn restore(path: &Path, start_paused: bool) -> (StartMode, AddOptions) {
    (
        StartMode::Restore {
            path: path.to_owned(),
        },
        AddOptions {
            start_paused,
            limit: None,
        },
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn restore_after_restart_resumes_torrent() {
    let seeder = Seeder::start_with_limit(MULTI, SLOW).await;
    let root = tempfile::tempdir().unwrap();
    let root = root.path();
    let req = torrent_req(root, &seeder);
    let (added, before) = download_part_then_stop(root, &seeder, 1, req.clone()).await;

    // Restart: a new engine over the same data dir, restored paused like the daemon does.
    let second = engine(root, vec![seeder.addr]).await;
    let (mode, opts) = restore(&added.path, true);
    let restored = second.add(1, req, mode, opts).await.unwrap();
    assert_eq!(restored.name, MULTI_NAME);
    assert_eq!(
        restored.path, added.path,
        "same place as before the restart"
    );
    // The existing data is verified and kept, not downloaded again.
    let st = wait(&second, 1, "verified data", |s| {
        s.state == DownloadState::Paused && s.done_bytes >= before
    })
    .await;
    assert!(st.done_bytes < st.total_bytes.unwrap());
    tokio::time::sleep(Duration::from_millis(600)).await;
    assert_eq!(
        second.status(1).unwrap().state,
        DownloadState::Paused,
        "restored paused: stays paused"
    );

    second.resume(1).await.unwrap();
    wait(&second, 1, "completion", |s| {
        s.state == DownloadState::Completed
    })
    .await;
    seeder.assert_downloaded_into(&root.join("out"));
    assert!(
        matches!(
            second
                .add(
                    1,
                    torrent_req(root, &seeder),
                    StartMode::Fresh,
                    AddOptions::default()
                )
                .await,
            Err(EngineError::Invalid(_))
        ),
        "no duplicate in librqbit"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn restore_magnet_after_restart() {
    let seeder = Seeder::start_with_limit(MULTI, SLOW).await;
    let root = tempfile::tempdir().unwrap();
    let root = root.path();
    let req = req(
        root,
        Source::Magnet {
            uri: seeder.magnet.clone(),
        },
    );
    let (added, _) = download_part_then_stop(root, &seeder, 7, req.clone()).await;
    assert_eq!(added.path, root.join("out").join(&seeder.info_hash_hex));

    let second = engine(root, vec![seeder.addr]).await;
    let (mode, opts) = restore(&added.path, false);
    second.add(7, req, mode, opts).await.unwrap();
    let st = wait(&second, 7, "completion", |s| {
        s.state == DownloadState::Completed
    })
    .await;
    assert_eq!(st.path, added.path);
    // A magnet's files are directly in its folder, where they were before the restart.
    for (rel, len) in MULTI {
        let got = std::fs::read(added.path.join(rel)).unwrap();
        assert!(got == support::content(rel, *len), "{rel} differs");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn restore_single_file_resumed_while_verifying() {
    let seeder = Seeder::start_with_limit(&[("one.iso", 900 * KIB)], SLOW).await;
    let root = tempfile::tempdir().unwrap();
    let root = root.path();
    let req = torrent_req(root, &seeder);
    let (added, _) = download_part_then_stop(root, &seeder, 2, req.clone()).await;
    assert_eq!(added.path, root.join("out/one.iso"));

    // The daemon restores paused, then its scheduler resumes at once (possibly while
    // librqbit is still checking the existing data).
    let second = engine(root, vec![seeder.addr]).await;
    let (mode, opts) = restore(&added.path, true);
    second.add(2, req, mode, opts).await.unwrap();
    second.resume(2).await.unwrap();
    wait(&second, 2, "completion", |s| {
        s.state == DownloadState::Completed
    })
    .await;
    seeder.assert_downloaded_into(&root.join("out"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn restore_missing_torrent_copy_needs_attention() {
    let seeder = Seeder::start(MULTI).await;
    let root = tempfile::tempdir().unwrap();
    let root = root.path();
    let req = torrent_req(root, &seeder);
    let Source::TorrentFile { path: original } = req.source.clone() else {
        unreachable!()
    };
    let first = engine(root, vec![seeder.addr]).await;
    let added = first
        .add(1, req.clone(), StartMode::Fresh, AddOptions::default())
        .await
        .unwrap();
    first.shutdown().await;
    drop(first);

    // Stored copy gone, original still there: restored from the original (copy rewritten).
    std::fs::remove_file(stored(root, 1)).unwrap();
    let second = engine(root, Vec::new()).await;
    let (mode, opts) = restore(&added.path, true);
    let restored = second.add(1, req.clone(), mode, opts).await.unwrap();
    assert_eq!(restored.path, added.path);
    assert_eq!(restored.name, MULTI_NAME);
    assert_eq!(
        std::fs::read(stored(root, 1)).unwrap(),
        seeder.torrent_bytes,
        "copy kept again"
    );
    wait(&second, 1, "restored paused", |s| {
        matches!(s.state, DownloadState::Paused | DownloadState::Completed)
    })
    .await;
    second.shutdown().await;
    drop(second);

    // Both gone: restored as needing attention (not an error).
    std::fs::remove_file(stored(root, 1)).unwrap();
    std::fs::remove_file(&original).unwrap();
    let third = engine(root, Vec::new()).await;
    let (mode, opts) = restore(&added.path, true);
    let restored = third.add(1, req.clone(), mode, opts).await.unwrap();
    assert_eq!(restored.path, added.path);
    let st = third.status(1).expect("known");
    assert_eq!(
        st.state,
        DownloadState::NeedsAttention {
            reason: AttentionReason::ValidationFailed {
                detail: "torrent file missing".into()
            }
        }
    );
    assert_eq!(st.path, added.path);
    assert!(matches!(
        third.resume(1).await,
        Err(EngineError::Invalid(_))
    ));
    third.pause(1).await.expect("pausing is a no-op");
    assert!(third.detail(1).is_none());
    third.remove(1, false).await.unwrap();
    assert!(third.status(1).is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pause_right_after_verification_sticks() {
    let seeder = Seeder::start_with_limit(&[("one.iso", 900 * KIB)], SLOW).await;
    let root = tempfile::tempdir().unwrap();
    let root = root.path();
    let req = torrent_req(root, &seeder);
    let (added, _) = download_part_then_stop(root, &seeder, 3, req.clone()).await;

    // Restored paused, resumed while librqbit checks the data, then paused as soon as the
    // check is over (the scheduler pausing it, e.g. max_active lowered).
    let second = engine(root, vec![seeder.addr]).await;
    let (mode, opts) = restore(&added.path, true);
    second.add(3, req, mode, opts).await.unwrap();
    second.resume(3).await.unwrap();
    let deadline = Instant::now() + TIMEOUT;
    loop {
        let st = second.status(3).unwrap();
        if st.state != DownloadState::Starting && st.done_bytes > 0 {
            break;
        }
        assert!(Instant::now() < deadline, "check never ended: {st:?}");
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    second.pause(3).await.unwrap();
    let paused = second.status(3).unwrap();
    assert_eq!(paused.state, DownloadState::Paused);
    tokio::time::sleep(Duration::from_millis(1000)).await;
    let later = second.status(3).unwrap();
    assert_eq!(later.state, DownloadState::Paused, "the pause was undone");
    assert_eq!(later.done_bytes, paused.done_bytes, "progress while paused");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn magnet_restores_offline_from_its_stored_metadata() {
    let seeder = Seeder::start_with_limit(MULTI, SLOW).await;
    let root = tempfile::tempdir().unwrap();
    let root = root.path();
    let req = req(
        root,
        Source::Magnet {
            uri: seeder.magnet.clone(),
        },
    );
    let (added, before) = download_part_then_stop(root, &seeder, 9, req.clone()).await;
    assert!(stored(root, 9).exists(), "the fetched metadata is kept");
    drop(seeder); // offline from now on

    let second = engine(root, Vec::new()).await;
    let (mode, opts) = restore(&added.path, true);
    let restored = second.add(9, req, mode, opts).await.unwrap();
    assert_eq!(
        restored.name, MULTI_NAME,
        "the real name, no network needed"
    );
    assert_eq!(restored.path, added.path);
    let st = wait(&second, 9, "verified data", |s| {
        s.state == DownloadState::Paused && s.done_bytes >= before
    })
    .await;
    assert_eq!(st.name, MULTI_NAME);
    assert_eq!(st.total_bytes, Some(MULTI.iter().map(|f| f.1 as u64).sum()));
}
