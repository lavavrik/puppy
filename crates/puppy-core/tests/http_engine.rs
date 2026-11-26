use puppy_core::config::HttpConfig;
use puppy_core::engine::{AddOptions, Engine, EngineError, EngineEvent, StartMode};
use puppy_core::http::engine::HttpEngine;
use puppy_core::http::resume::{part_path, sidecar_path};
use puppy_core::types::{AddRequest, DownloadState, RequestContext, Source};
use puppy_testserver::{ServerCfg, TestServer};
use std::path::Path;
use std::time::{Duration, Instant};

const KIB: u64 = 1024;

fn cfg(dir: &Path) -> HttpConfig {
    HttpConfig {
        backoff_base_ms: 10,
        backoff_max_ms: 100,
        flush_interval_ms: 100,
        default_dir: dir.to_path_buf(),
        ..HttpConfig::default()
    }
}

fn request(s: &TestServer, dir: &Path) -> AddRequest {
    AddRequest {
        source: Source::Http {
            url: s.url("/file.bin"),
        },
        output_dir: Some(dir.to_path_buf()),
        filename: None,
        context: RequestContext::default(),
        files: Default::default(),
        seed: None,
        checksum: None,
        mirrors: Vec::new(),
        size: None,
    }
}

async fn wait_completed(e: &HttpEngine, id: u64) {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let st = e.status(id).expect("status");
        if matches!(st.state, DownloadState::Completed) {
            return;
        }
        assert!(Instant::now() < deadline, "timed out; last {st:?}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test]
async fn engine_add_and_complete_emits_event() {
    let dir = tempfile::tempdir().unwrap();
    let s = TestServer::start(ServerCfg {
        len: 256 * KIB,
        ..ServerCfg::default()
    })
    .await;
    let e = HttpEngine::new(cfg(dir.path())).unwrap();
    let mut rx = e.subscribe();
    e.add(
        1,
        request(&s, dir.path()),
        StartMode::Fresh,
        AddOptions::default(),
    )
    .await
    .unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let ev = tokio::time::timeout_at(deadline.into(), rx.recv())
            .await
            .expect("timed out")
            .unwrap();
        if matches!(ev, EngineEvent::Completed { id: 1 }) {
            break;
        }
    }
    assert!(matches!(
        e.status(1).unwrap().state,
        DownloadState::Completed
    ));
}

#[tokio::test]
async fn engine_rejects_magnet_and_duplicates() {
    let dir = tempfile::tempdir().unwrap();
    let s = TestServer::start(ServerCfg::default()).await;
    let e = HttpEngine::new(cfg(dir.path())).unwrap();
    let magnet = Source::Magnet {
        uri: "magnet:?xt=urn:btih:abc".into(),
    };
    assert!(!e.accepts(&magnet));
    for url in ["ftp://x/y", "ftps://x/y", "ftpes://x/y", "sftp://x/y"] {
        assert!(e.accepts(&Source::Http {
            url: url.parse().unwrap()
        }));
    }
    assert!(!e.accepts(&Source::Http {
        url: "scp://x/y".parse().unwrap()
    }));
    let mut req = request(&s, dir.path());
    req.source = magnet;
    assert!(matches!(
        e.add(1, req, StartMode::Fresh, AddOptions::default()).await,
        Err(EngineError::Unsupported(_))
    ));
    e.add(
        2,
        request(&s, dir.path()),
        StartMode::Fresh,
        AddOptions::default(),
    )
    .await
    .unwrap();
    assert!(matches!(
        e.add(
            2,
            request(&s, dir.path()),
            StartMode::Fresh,
            AddOptions::default()
        )
        .await,
        Err(EngineError::Invalid(_))
    ));
    assert!(matches!(
        e.remove(99, false).await,
        Err(EngineError::NotFound(99))
    ));
    assert!(matches!(
        e.set_download_limit(99, None),
        Err(EngineError::NotFound(99))
    ));
    e.remove(2, true).await.unwrap();
}

#[tokio::test]
async fn engine_global_budget_applies_across_downloads() {
    let dir = tempfile::tempdir().unwrap();
    let s = TestServer::start(ServerCfg {
        len: 512 * KIB,
        ..ServerCfg::default()
    })
    .await;
    let e = HttpEngine::new(cfg(dir.path())).unwrap();
    e.set_global_download_budget(Some(256 * KIB));
    let start = Instant::now();
    for id in [1, 2] {
        let mut req = request(&s, dir.path());
        req.filename = Some(format!("f{id}.bin"));
        e.add(id, req, StartMode::Fresh, AddOptions::default())
            .await
            .unwrap();
    }
    wait_completed(&e, 1).await;
    wait_completed(&e, 2).await;
    // 1 MiB total at 256 KiB/s is ~4 s; allow for the initial burst.
    let el = start.elapsed();
    assert!(el >= Duration::from_millis(3000), "too fast: {el:?}");
}

#[tokio::test]
async fn engine_remove_deletes_files() {
    let dir = tempfile::tempdir().unwrap();
    let s = TestServer::start(ServerCfg {
        len: 4 * 1024 * KIB,
        bytes_per_sec: Some(512 * KIB),
        ..ServerCfg::default()
    })
    .await;
    let e = HttpEngine::new(cfg(dir.path())).unwrap();
    let added = e
        .add(
            1,
            request(&s, dir.path()),
            StartMode::Fresh,
            AddOptions::default(),
        )
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;
    e.pause(1).await.unwrap();
    assert!(part_path(&added.path).exists());
    e.remove(1, true).await.unwrap();
    assert!(!part_path(&added.path).exists());
    assert!(!sidecar_path(&added.path).exists());
    assert!(!added.path.exists());
    assert!(e.status(1).is_none());
}

#[tokio::test]
async fn engine_remove_completed_deletes_final_file() {
    let dir = tempfile::tempdir().unwrap();
    let s = TestServer::start(ServerCfg {
        len: 64 * KIB,
        ..ServerCfg::default()
    })
    .await;
    let e = HttpEngine::new(cfg(dir.path())).unwrap();
    let added = e
        .add(
            1,
            request(&s, dir.path()),
            StartMode::Fresh,
            AddOptions::default(),
        )
        .await
        .unwrap();
    wait_completed(&e, 1).await;
    assert!(added.path.exists());
    e.remove(1, true).await.unwrap();
    assert!(!added.path.exists());
    assert!(e.status(1).is_none());
}

#[tokio::test]
async fn engine_restore_mode() {
    let dir = tempfile::tempdir().unwrap();
    let s = TestServer::start(ServerCfg {
        len: 2 * 1024 * KIB,
        bytes_per_sec: Some(1024 * KIB),
        ..ServerCfg::default()
    })
    .await;
    let e1 = HttpEngine::new(cfg(dir.path())).unwrap();
    let req = request(&s, dir.path());
    let added = e1
        .add(1, req.clone(), StartMode::Fresh, AddOptions::default())
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;
    e1.pause(1).await.unwrap();
    assert!(matches!(e1.status(1).unwrap().state, DownloadState::Paused));
    drop(e1);

    let e2 = HttpEngine::new(cfg(dir.path())).unwrap();
    e2.add(
        1,
        req,
        StartMode::Restore {
            path: added.path.clone(),
        },
        AddOptions::default(),
    )
    .await
    .unwrap();
    wait_completed(&e2, 1).await;
    assert_eq!(
        std::fs::metadata(&added.path).unwrap().len(),
        2 * 1024 * KIB
    );
}

#[tokio::test]
async fn engine_failed_add_releases_id() {
    let dir = tempfile::tempdir().unwrap();
    let s = TestServer::start(ServerCfg {
        len: 16 * KIB,
        ..ServerCfg::default()
    })
    .await;
    let e = HttpEngine::new(cfg(dir.path())).unwrap();
    let mut bad = request(&s, dir.path());
    bad.source = Source::Http {
        url: s.url("/missing"),
    };
    assert!(
        e.add(1, bad, StartMode::Fresh, AddOptions::default())
            .await
            .is_err()
    );
    e.add(
        1,
        request(&s, dir.path()),
        StartMode::Fresh,
        AddOptions::default(),
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn engine_dropped_add_releases_id() {
    let dir = tempfile::tempdir().unwrap();
    let stall = TestServer::start(ServerCfg {
        stall_headers: true,
        ..ServerCfg::default()
    })
    .await;
    let good = TestServer::start(ServerCfg {
        len: 16 * KIB,
        ..ServerCfg::default()
    })
    .await;
    let e = HttpEngine::new(cfg(dir.path())).unwrap();
    let r = tokio::time::timeout(
        Duration::from_millis(100),
        e.add(
            1,
            request(&stall, dir.path()),
            StartMode::Fresh,
            AddOptions::default(),
        ),
    )
    .await;
    assert!(r.is_err(), "add should have timed out");
    e.add(
        1,
        request(&good, dir.path()),
        StartMode::Fresh,
        AddOptions::default(),
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn engine_concurrent_same_id_one_wins() {
    let dir = tempfile::tempdir().unwrap();
    let s = TestServer::start(ServerCfg {
        len: 16 * KIB,
        ..ServerCfg::default()
    })
    .await;
    let e = HttpEngine::new(cfg(dir.path())).unwrap();
    let (a, b) = tokio::join!(
        e.add(
            1,
            request(&s, dir.path()),
            StartMode::Fresh,
            AddOptions::default()
        ),
        e.add(
            1,
            request(&s, dir.path()),
            StartMode::Fresh,
            AddOptions::default()
        )
    );
    let oks = [a.is_ok(), b.is_ok()].iter().filter(|x| **x).count();
    assert_eq!(oks, 1);
    let err = if a.is_err() { a } else { b }.unwrap_err();
    assert!(matches!(err, EngineError::Invalid(_)), "{err:?}");
}

fn paused() -> AddOptions {
    AddOptions {
        start_paused: true,
        limit: None,
    }
}

#[tokio::test]
async fn engine_add_fresh_paused_does_not_transfer() {
    let dir = tempfile::tempdir().unwrap();
    let s = TestServer::start(ServerCfg {
        len: 256 * KIB,
        ..ServerCfg::default()
    })
    .await;
    let e = HttpEngine::new(cfg(dir.path())).unwrap();
    let added = e
        .add(1, request(&s, dir.path()), StartMode::Fresh, paused())
        .await
        .unwrap();
    assert!(matches!(e.status(1).unwrap().state, DownloadState::Paused));
    assert!(part_path(&added.path).exists());
    assert!(sidecar_path(&added.path).exists(), "sidecar not written");
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(s.range_requests(), 0, "transferred while paused");
    assert_eq!(e.status(1).unwrap().done_bytes, 0);
    e.resume(1).await.unwrap();
    wait_completed(&e, 1).await;
    assert_eq!(std::fs::metadata(&added.path).unwrap().len(), 256 * KIB);
}

#[tokio::test]
async fn engine_restore_paused_no_network() {
    let dir = tempfile::tempdir().unwrap();
    let s = TestServer::start(ServerCfg {
        len: 2 * 1024 * KIB,
        bytes_per_sec: Some(1024 * KIB),
        ..ServerCfg::default()
    })
    .await;
    let e1 = HttpEngine::new(cfg(dir.path())).unwrap();
    let req = request(&s, dir.path());
    let added = e1
        .add(1, req.clone(), StartMode::Fresh, AddOptions::default())
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    e1.pause(1).await.unwrap();
    drop(e1);
    s.set_cfg(|c| c.bytes_per_sec = None);

    let e2 = HttpEngine::new(cfg(dir.path())).unwrap();
    let before = s.requests_seen().len();
    let mode = StartMode::Restore {
        path: added.path.clone(),
    };
    e2.add(1, req, mode, paused()).await.unwrap();
    assert!(matches!(e2.status(1).unwrap().state, DownloadState::Paused));
    assert_eq!(
        s.requests_seen().len(),
        before,
        "network used while restoring"
    );
    e2.resume(1).await.unwrap();
    wait_completed(&e2, 1).await;
    assert_eq!(
        std::fs::metadata(&added.path).unwrap().len(),
        2 * 1024 * KIB
    );
}

#[tokio::test]
async fn engine_initial_limit_applies() {
    // 64 KiB at 32 KiB/s: the limiter starts empty, so even the first chunk waits.
    let dir = tempfile::tempdir().unwrap();
    let s = TestServer::start(ServerCfg {
        len: 64 * KIB,
        ..ServerCfg::default()
    })
    .await;
    let e = HttpEngine::new(cfg(dir.path())).unwrap();
    let opts = AddOptions {
        start_paused: false,
        limit: Some(32 * KIB),
    };
    let t = Instant::now();
    e.add(1, request(&s, dir.path()), StartMode::Fresh, opts)
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    let st = e.status(1).unwrap();
    assert_eq!(
        st.done_bytes, 0,
        "limit not applied from the first byte: {st:?}"
    );
    wait_completed(&e, 1).await;
    let took = t.elapsed();
    assert!(took >= Duration::from_millis(1500), "took only {took:?}");
}
