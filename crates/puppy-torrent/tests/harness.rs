//! The engine's session lifecycle and the local seeder harness the other tests rely on.
//! Network-free: everything runs on 127.0.0.1 with DHT, trackers, LSD and UPnP off.

mod support;

use std::time::{Duration, Instant};

use puppy_core::engine::{Engine, EngineKind};
use puppy_core::types::Source;
use puppy_torrent::{TorrentEngine, TorrentEngineOptions};
use support::Seeder;

fn test_options(dir: &std::path::Path) -> TorrentEngineOptions {
    TorrentEngineOptions {
        dht: true, // ignored in test mode
        upnp: true,
        lsd: true, // ignored in test mode
        listen_port: 42069,
        seed_ratio: 0.0,
        seed_time: std::time::Duration::ZERO,
        data_dir: dir.join("data"),
        default_dir: dir.join("downloads"),
        test_mode: true,
        test_initial_peers: Vec::new(),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn engine_starts_and_stops_cleanly() {
    let dir = tempfile::tempdir().unwrap();
    let engine = TorrentEngine::new(test_options(dir.path()))
        .await
        .expect("engine starts");
    // Nothing is opened (or created) until a torrent needs the session.
    assert!(!engine.session_started());
    assert!(engine.listen_addr().is_none());
    assert!(!dir.path().join("data/torrent").exists());
    engine.start_session().await.expect("session starts");
    assert!(engine.session_started());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(dir.path().join("data/torrent"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o700, "engine dir is owner-only");
    }

    assert_eq!(engine.kind(), EngineKind::Torrent);
    assert!(engine.accepts(&Source::Magnet {
        uri: "magnet:?xt=urn:btih:0000000000000000000000000000000000000000".into()
    }));
    assert!(engine.accepts(&Source::TorrentFile {
        path: "a.torrent".into()
    }));
    assert!(!engine.accepts(&Source::Http {
        url: "http://example.com/".parse().unwrap()
    }));
    let _events = engine.subscribe();

    // Test mode listens on loopback only, on an OS-assigned port.
    let addr = engine.listen_addr().expect("engine listens");
    assert!(addr.ip().is_loopback(), "{addr}");
    assert_ne!(addr.port(), 42069);
    assert!(
        std::net::TcpStream::connect(addr).is_ok(),
        "listener accepts"
    );

    let started = Instant::now();
    tokio::time::timeout(Duration::from_secs(5), engine.shutdown())
        .await
        .expect("shutdown completes within 5 s");
    assert!(started.elapsed() < Duration::from_secs(5));

    // The session's tasks are gone: its listener no longer holds the port.
    let rebound = (0..20).any(|_| {
        let ok = std::net::TcpListener::bind(addr).is_ok();
        if !ok {
            std::thread::sleep(Duration::from_millis(50));
        }
        ok
    });
    assert!(rebound, "listen port {addr} still held after shutdown");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn seeder_harness_serves_torrent() {
    let seeder = Seeder::start(&[("a.bin", 200_000), ("sub/b.bin", 70_001)]).await;
    assert!(seeder.addr.ip().is_loopback());
    assert_eq!(seeder.info_hash_hex.len(), 40);
    assert!(
        seeder
            .magnet
            .starts_with(&format!("magnet:?xt=urn:btih:{}", seeder.info_hash_hex)),
        "{}",
        seeder.magnet
    );

    let out = tempfile::tempdir().unwrap();
    tokio::time::timeout(
        Duration::from_secs(10),
        support::leech_raw(&seeder, out.path()),
    )
    .await
    .expect("leecher completes within 10 s");
    seeder.assert_downloaded_into(out.path());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn seeder_harness_serves_single_file_torrent() {
    let seeder = Seeder::start(&[("one.iso", 100_000)]).await;
    assert_eq!(seeder.name, "one.iso");
    let out = tempfile::tempdir().unwrap();
    tokio::time::timeout(
        Duration::from_secs(10),
        support::leech_raw(&seeder, out.path()),
    )
    .await
    .expect("leecher completes within 10 s");
    seeder.assert_downloaded_into(out.path());
}
