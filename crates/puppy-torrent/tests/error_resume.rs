//! A fresh torrent failed in librqbit (`Error` state: a disk write failed) resumes in
//! the same session once the cause is gone. Linux only: the write error is forced with
//! a file size limit (`RLIMIT_FSIZE`), which is process-wide, so this binary holds this
//! one test only.
#![cfg(target_os = "linux")]

mod support;

use std::time::{Duration, Instant};

use puppy_core::engine::{AddOptions, Engine, StartMode};
use puppy_core::types::{AddRequest, DownloadState, RequestContext, Source};
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
    let seeder = Seeder::start(&[("one.iso", LEN)]).await;
    let dir = tempfile::tempdir().unwrap();
    let engine = TorrentEngine::new(TorrentEngineOptions {
        dht: false,
        upnp: false,
        lsd: false,
        listen_port: 0,
        seed_ratio: 0.0,
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

    // No file may grow past 256 KiB: librqbit's write fails, the torrent errors.
    set_fsize_limit(256 * 1024);
    let req = AddRequest {
        source: Source::TorrentFile { path: torrent },
        output_dir: Some(out.clone()),
        filename: None,
        context: RequestContext::default(),
        files: Default::default(),
        seed: None,
    };
    let added = engine
        .add(1, req, StartMode::Fresh, AddOptions::default())
        .await;
    let failed = async {
        let added = added.unwrap();
        wait(&engine, "the write failure", |s| {
            matches!(s, DownloadState::Failed { .. })
        })
        .await;
        added
    }
    .await;
    set_fsize_limit(libc::RLIM_INFINITY);
    assert_eq!(failed.path, out.join("one.iso"));

    // Freed: resuming re-adds it over its own files and it completes.
    engine.resume(1).await.expect("resumes in-session");
    wait(&engine, "completion", |s| *s == DownloadState::Completed).await;
    assert!(
        std::fs::read(&failed.path).unwrap() == support::content("one.iso", LEN),
        "downloaded content differs"
    );
    engine.shutdown().await;
}
