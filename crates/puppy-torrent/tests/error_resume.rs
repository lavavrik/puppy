//! A fresh torrent failed in librqbit (`Error` state: a disk write failed) shows its
//! last exact piece map and resumes in the same session once the cause is gone. Linux only: the write error is forced with
//! a file size limit (`RLIMIT_FSIZE`), which is process-wide, so this binary holds this
//! one test only.
#![cfg(target_os = "linux")]

mod support;

use std::time::{Duration, Instant};

use puppy_core::engine::{AddOptions, Engine, StartMode};
use puppy_core::types::{AddRequest, DownloadState, EngineDetail, RequestContext, Source};
use puppy_torrent::{TorrentEngine, TorrentEngineOptions};
use support::Seeder;

const TIMEOUT: Duration = Duration::from_secs(20);
const LEN: usize = 1024 * 1024;

fn set_fsize_limit(bytes: libc::rlim_t) {
    let mut lim = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: plain libc calls with valid pointers.
    unsafe {
        assert_eq!(libc::getrlimit(libc::RLIMIT_FSIZE, &mut lim), 0);
        lim.rlim_cur = bytes.min(lim.rlim_max);
        assert_eq!(libc::setrlimit(libc::RLIMIT_FSIZE, &lim), 0);
    }
}

async fn wait(engine: &TorrentEngine, what: &str, pred: impl Fn(&DownloadState) -> bool) {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        let st = engine.status(1).expect("download known").state;
        if pred(&st) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {what}: {st:?}"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_failed_fresh_torrent_resumes_in_session() {
    // SAFETY: writes past the limit then fail with EFBIG instead of killing the process.
    unsafe {
        libc::signal(libc::SIGXFSZ, libc::SIG_IGN);
    }
    // Throttled: the torrent is live (with an exact piece map) for a while before the
    // write past the limit fails.
    let seeder = Seeder::start_with_limit(&[("one.iso", LEN)], 64 * 1024).await;
    let dir = tempfile::tempdir().unwrap();
    let engine = TorrentEngine::new(TorrentEngineOptions {
        dht: false,
        upnp: false,
        lsd: false,
        listen_port: 0,
        seed_ratio: 0.0,
        seed_time: std::time::Duration::ZERO,
        data_dir: dir.path().join("data"),
        default_dir: dir.path().join("downloads"),
        test_mode: true,
        test_initial_peers: vec![seeder.addr],
    })
    .await
    .unwrap();
    let torrent = dir.path().join("one.torrent");
    std::fs::write(&torrent, &seeder.torrent_bytes).unwrap();
    let out = dir.path().join("out");
    engine.start_session().await.unwrap();

    let req = AddRequest {
        source: Source::TorrentFile { path: torrent },
        output_dir: Some(out.clone()),
        filename: None,
        context: RequestContext::default(),
        files: Default::default(),
        seed: None,
        checksum: None,
    };
    let added = engine
        .add(1, req, StartMode::Fresh, AddOptions::default())
        .await;
    let (failed, last_seen) = async {
        let added = added.unwrap();
        // Once librqbit has created the file (full length) and has a piece: no write may
        // reach past the first byte, so the next piece's write fails and the torrent
        // errors (pieces come in any order).
        let deadline = Instant::now() + TIMEOUT;
        loop {
            if let Some(EngineDetail::Torrent { have, .. }) = engine.detail(1)
                && have.iter().any(|b| *b != 0)
            {
                break;
            }
            assert!(Instant::now() < deadline, "no piece downloaded");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        set_fsize_limit(1);
        // The maps puppy shows while it is live.
        let mut last_seen = Vec::new();
        let deadline = Instant::now() + TIMEOUT;
        loop {
            if let Some(EngineDetail::Torrent { have, .. }) = engine.detail(1) {
                last_seen = have;
            }
            let st = engine.status(1).unwrap().state;
            if matches!(st, DownloadState::Failed { .. }) {
                break;
            }
            assert!(Instant::now() < deadline, "no write failure: {st:?}");
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        (added, last_seen)
    }
    .await;
    // Failed: librqbit has no piece map; puppy shows the last exact one (the per-file
    // synthesis would be empty: librqbit reports no file progress in this state).
    let no_map = engine.piece_dump(1).is_none();
    let detail = engine.detail(1);
    set_fsize_limit(libc::RLIM_INFINITY);
    assert!(no_map, "librqbit still has a map");
    let Some(EngineDetail::Torrent { have, .. }) = detail else {
        panic!("no detail");
    };
    assert!(have.iter().any(|b| *b != 0), "the last exact map is gone");
    assert_eq!(have, last_seen);
    assert_eq!(failed.path, out.join("one.iso"));
    seeder.session.ratelimits.set_upload_bps(None);

    // Freed: resuming re-adds it over its own files and it completes.
    engine.resume(1).await.expect("resumes in-session");
    wait(&engine, "completion", |s| *s == DownloadState::Completed).await;
    assert!(
        std::fs::read(&failed.path).unwrap() == support::content("one.iso", LEN),
        "downloaded content differs"
    );
    engine.shutdown().await;
}
