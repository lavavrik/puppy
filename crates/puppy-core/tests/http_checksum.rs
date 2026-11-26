//! Checksum verification of finished HTTP downloads.

use puppy_core::checksum::{Checksum, HashAlgo};
use puppy_core::config::HttpConfig;
use puppy_core::engine::{AddOptions, Added, EngineEvent};
use puppy_core::http::download::HttpDownload;
use puppy_core::http::probe::build_client;
use puppy_core::http::resume::{part_path, sidecar_path};
use puppy_core::limiter::TokenBucket;
use puppy_core::types::{AddRequest, DownloadState, DownloadStatus, RequestContext, Source};
use puppy_testserver::{ServerCfg, TestServer, body_sha256};
use std::path::Path;
use std::time::{Duration, Instant};
use tokio::sync::broadcast;

const MIB: u64 = 1024 * 1024;
const LEN: u64 = 4 * MIB;
const TIMEOUT: Duration = Duration::from_secs(20);

fn cfg(dir: &Path) -> HttpConfig {
    HttpConfig {
        backoff_base_ms: 10,
        backoff_max_ms: 100,
        flush_interval_ms: 100,
        default_dir: dir.to_path_buf(),
        ..HttpConfig::default()
    }
}

fn request(s: &TestServer, dir: &Path, checksum: Option<Checksum>) -> AddRequest {
    AddRequest {
        source: Source::Http {
            url: s.url("/file.bin"),
        },
        output_dir: Some(dir.to_path_buf()),
        filename: None,
        context: RequestContext::default(),
        files: Default::default(),
        seed: None,
        checksum,
        mirrors: Vec::new(),
        size: None,
    }
}

fn good() -> Checksum {
    Checksum::new(HashAlgo::Sha256, &body_sha256(LEN, 0)).unwrap()
}

fn bad() -> Checksum {
    Checksum::new(HashAlgo::Sha256, &"0".repeat(64)).unwrap()
}

async fn start(
    req: AddRequest,
    dir: &Path,
    tx: broadcast::Sender<EngineEvent>,
) -> (HttpDownload, Added) {
    let c = cfg(dir);
    let client = build_client(&c).unwrap();
    HttpDownload::start_fresh(
        1,
        req,
        AddOptions::default(),
        client,
        c,
        TokenBucket::new(None),
        tx,
    )
    .await
    .unwrap()
}

async fn restore(req: AddRequest, final_path: &Path, start_paused: bool) -> (HttpDownload, Added) {
    let c = cfg(final_path.parent().unwrap());
    let client = build_client(&c).unwrap();
    let opts = AddOptions {
        start_paused,
        limit: None,
    };
    let (tx, _) = broadcast::channel(256);
    HttpDownload::restore(
        2,
        req,
        final_path,
        opts,
        client,
        c,
        TokenBucket::new(None),
        tx,
    )
    .await
    .unwrap()
}

async fn wait_status(dl: &HttpDownload, pred: impl Fn(&DownloadStatus) -> bool) -> DownloadStatus {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        let st = dl.status();
        if pred(&st) {
            return st;
        }
        assert!(Instant::now() < deadline, "timed out; last status {st:?}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

fn ended(s: &DownloadStatus) -> bool {
    matches!(
        s.state,
        DownloadState::Completed | DownloadState::Failed { .. }
    )
}

fn states(rx: &mut broadcast::Receiver<EngineEvent>) -> Vec<DownloadState> {
    let mut v = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        if let EngineEvent::StateChanged { state, .. } = ev {
            v.push(state);
        }
    }
    v
}

#[tokio::test]
async fn matching_checksum_completes_after_verifying() {
    let s = TestServer::start(ServerCfg::default()).await;
    let dir = tempfile::tempdir().unwrap();
    let (tx, mut rx) = broadcast::channel(256);
    let (dl, added) = start(request(&s, dir.path(), Some(good())), dir.path(), tx).await;
    let st = wait_status(&dl, ended).await;
    assert_eq!(st.state, DownloadState::Completed, "{st:?}");
    let seen = states(&mut rx);
    let verifying = seen.iter().position(|s| *s == DownloadState::Verifying);
    let completed = seen.iter().position(|s| *s == DownloadState::Completed);
    assert!(verifying.is_some() && verifying < completed, "{seen:?}");
    assert!(added.path.exists());
    assert!(!part_path(&added.path).exists());
    assert!(
        !sidecar_path(&added.path).exists(),
        "sidecar removed once verified"
    );
}

#[tokio::test]
async fn uppercase_and_other_algorithms_match() {
    let s = TestServer::start(ServerCfg {
        len: 1000,
        ..ServerCfg::default()
    })
    .await;
    let dir = tempfile::tempdir().unwrap();
    let body: Vec<u8> = (0..1000)
        .map(|i| puppy_testserver::body_byte(i, 0))
        .collect();
    let md5 = {
        use md5::Digest;
        md5::Md5::digest(&body)
            .iter()
            .map(|b| format!("{b:02X}"))
            .collect::<String>()
    };
    let sum: Checksum = format!("MD5={md5}").parse().unwrap();
    let (tx, _) = broadcast::channel(256);
    let (dl, _) = start(request(&s, dir.path(), Some(sum)), dir.path(), tx).await;
    let st = wait_status(&dl, ended).await;
    assert_eq!(st.state, DownloadState::Completed, "{st:?}");
}

#[tokio::test]
async fn mismatch_fails_naming_both_digests_and_keeps_the_file() {
    let s = TestServer::start(ServerCfg::default()).await;
    let dir = tempfile::tempdir().unwrap();
    let (tx, mut rx) = broadcast::channel(256);
    let (dl, added) = start(request(&s, dir.path(), Some(bad())), dir.path(), tx).await;
    let st = wait_status(&dl, ended).await;
    let DownloadState::Failed { reason, transient } = st.state else {
        panic!("{st:?}")
    };
    assert!(!transient, "a mismatch is not retried automatically");
    assert_eq!(
        reason,
        format!(
            "checksum mismatch (sha-256): expected {}, got {}",
            "0".repeat(64),
            body_sha256(LEN, 0)
        )
    );
    assert!(
        !completed_seen(&mut rx),
        "no Completed event for a mismatch"
    );
    assert_eq!(
        std::fs::metadata(&added.path).unwrap().len(),
        LEN,
        "file kept"
    );
    assert!(
        sidecar_path(&added.path).exists(),
        "sidecar kept for a re-check"
    );
    assert_eq!(st.done_bytes, LEN);

    // Resuming checks it again (still wrong), with no network.
    let before = s.requests_seen().len();
    dl.resume().await.unwrap();
    let st = wait_status(&dl, ended).await;
    assert!(matches!(st.state, DownloadState::Failed { .. }), "{st:?}");
    assert_eq!(s.requests_seen().len(), before);

    // Removing with its files deletes the bad file too.
    dl.cancel(true).await;
    assert!(!added.path.exists());
    assert!(!sidecar_path(&added.path).exists());
}

fn completed_seen(rx: &mut broadcast::Receiver<EngineEvent>) -> bool {
    let mut seen = false;
    while let Ok(ev) = rx.try_recv() {
        seen |= matches!(ev, EngineEvent::Completed { .. });
    }
    seen
}

/// The daemon stopped after the file was finalized but before it was verified: the
/// finished file and its sidecar are there, and restoring verifies it.
#[tokio::test]
async fn restart_between_completion_and_verification_verifies() {
    let s = TestServer::start(ServerCfg::default()).await;
    let dir = tempfile::tempdir().unwrap();
    // A mismatching download leaves exactly that layout (file + sidecar, no .part).
    let (tx, _) = broadcast::channel(256);
    let (dl, added) = start(request(&s, dir.path(), Some(bad())), dir.path(), tx).await;
    wait_status(&dl, ended).await;
    drop(dl);
    assert!(sidecar_path(&added.path).exists());
    let before = s.requests_seen().len();

    // Restored paused (as the daemon does): finished, nothing transferred, not checked.
    let req = request(&s, dir.path(), Some(good()));
    let (dl, restored) = restore(req, &added.path, true).await;
    assert_eq!(restored.path, added.path);
    assert_eq!(dl.status().state, DownloadState::Paused);
    dl.resume().await.unwrap();
    let st = wait_status(&dl, ended).await;
    assert_eq!(st.state, DownloadState::Completed, "{st:?}");
    assert!(!sidecar_path(&added.path).exists());
    assert_eq!(s.requests_seen().len(), before, "no network needed");
}

#[tokio::test]
async fn restored_unpaused_verifies_at_once() {
    let s = TestServer::start(ServerCfg::default()).await;
    let dir = tempfile::tempdir().unwrap();
    let (tx, _) = broadcast::channel(256);
    let (dl, added) = start(request(&s, dir.path(), Some(bad())), dir.path(), tx).await;
    wait_status(&dl, ended).await;
    drop(dl);
    let (dl, _) = restore(request(&s, dir.path(), Some(good())), &added.path, false).await;
    let st = wait_status(&dl, ended).await;
    assert_eq!(st.state, DownloadState::Completed, "{st:?}");
}

#[tokio::test]
async fn pausing_while_verifying_stops_the_check() {
    // A large file so hashing takes a moment (in debug builds).
    let len = 64 * MIB;
    let s = TestServer::start(ServerCfg {
        len,
        ..ServerCfg::default()
    })
    .await;
    let dir = tempfile::tempdir().unwrap();
    let sum = Checksum::new(HashAlgo::Sha512, &"0".repeat(128)).unwrap();
    let (tx, _) = broadcast::channel(256);
    let (dl, _) = start(request(&s, dir.path(), Some(sum)), dir.path(), tx).await;
    let st = wait_status(&dl, |s| s.state == DownloadState::Verifying || ended(s)).await;
    if st.state != DownloadState::Verifying {
        // Hashed faster than we looked: nothing to pause.
        return;
    }
    assert_eq!(st.total_bytes, Some(len));
    assert!(st.done_bytes <= len);
    dl.pause().await;
    let st = dl.status();
    if st.state == DownloadState::Paused {
        dl.resume().await.unwrap();
    }
    let st = wait_status(&dl, ended).await;
    assert!(matches!(st.state, DownloadState::Failed { .. }), "{st:?}");
}

#[tokio::test]
async fn without_checksum_the_sidecar_goes_at_once() {
    let s = TestServer::start(ServerCfg::default()).await;
    let dir = tempfile::tempdir().unwrap();
    let (tx, mut rx) = broadcast::channel(256);
    let (dl, added) = start(request(&s, dir.path(), None), dir.path(), tx).await;
    let st = wait_status(&dl, ended).await;
    assert_eq!(st.state, DownloadState::Completed);
    assert!(!states(&mut rx).contains(&DownloadState::Verifying));
    assert!(!sidecar_path(&added.path).exists());
}
