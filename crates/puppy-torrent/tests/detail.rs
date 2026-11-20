//! `TorrentEngine::detail` and the rate limits. Network-free: 127.0.0.1 only, DHT,
//! trackers, LSD and UPnP off, peers given explicitly.

mod support;

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use puppy_core::engine::{AddOptions, Engine, EngineError, StartMode};
use puppy_core::types::{
    AddRequest, DownloadId, DownloadState, EngineDetail, RequestContext, Source,
};
use puppy_torrent::{TorrentEngine, TorrentEngineOptions};
use support::Seeder;
use tempfile::TempDir;

const TIMEOUT: Duration = Duration::from_secs(30);
const KIB: usize = 1024;

struct Env {
    dir: TempDir,
    engine: TorrentEngine,
}

impl Env {
    async fn new(peers: Vec<SocketAddr>) -> Env {
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
            test_initial_peers: peers,
        })
        .await
        .expect("engine starts");
        Env { dir, engine }
    }

    fn req(&self, source: Source) -> AddRequest {
        AddRequest {
            source,
            output_dir: Some(self.dir.path().join("out")),
            filename: None,
            context: RequestContext::default(),
        }
    }

    fn torrent_req(&self, seeder: &Seeder) -> AddRequest {
        let path = self
            .dir
            .path()
            .join(format!("{}.torrent", seeder.info_hash_hex));
        std::fs::write(&path, &seeder.torrent_bytes).unwrap();
        self.req(Source::TorrentFile { path })
    }

    async fn add(&self, id: DownloadId, req: AddRequest, opts: AddOptions) {
        self.engine
            .add(id, req, StartMode::Fresh, opts)
            .await
            .expect("added");
    }
}

fn parts(d: EngineDetail) -> (u32, Vec<u8>, u32, u32, u32, f32) {
    match d {
        EngineDetail::Torrent {
            piece_count,
            have,
            peers,
            seeds,
            leechers,
            ratio,
        } => (piece_count, have, peers, seeds, leechers, ratio),
        other => panic!("not a torrent detail: {other:?}"),
    }
}

fn popcount(have: &[u8]) -> u32 {
    have.iter().map(|b| b.count_ones()).sum()
}

#[tokio::test]
async fn detail_reports_pieces_and_peers() {
    // 1 MiB (64 pieces) from a seeder throttled to 256 KiB/s: several seconds in flight.
    let seeder = Seeder::start_with_limit(&[("slow.bin", 1024 * KIB)], 256 * 1024).await;
    let env = Env::new(vec![seeder.addr]).await;
    env.add(1, env.torrent_req(&seeder), AddOptions::default())
        .await;

    let deadline = Instant::now() + TIMEOUT;
    let mid = loop {
        if let Some(d) = env.engine.detail(1) {
            let (pieces, have, peers, ..) = parts(d);
            let got = popcount(&have);
            if got > 0 && got < pieces && peers >= 1 {
                break (pieces, have, peers);
            }
        }
        assert!(Instant::now() < deadline, "never saw a mid-download state");
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    let (pieces, have, peers) = mid;
    assert_eq!(pieces, 64);
    assert_eq!(have.len(), 8, "one bit per piece, MSB first, byte padded");
    assert!(peers >= 1);
    // In-order synthesis: a single file's had pieces form a prefix (MSB first).
    let got = popcount(&have);
    for i in 0..pieces as usize {
        let bit = have[i / 8] & (0x80 >> (i % 8)) != 0;
        assert_eq!(bit, (i as u32) < got, "piece {i}");
    }

    // Completion: every piece set, nothing left to leech from.
    let deadline = Instant::now() + TIMEOUT;
    while env.engine.status(1).unwrap().state != DownloadState::Completed {
        assert!(Instant::now() < deadline, "download did not complete");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let (pieces, have, _, seeds, leechers, ratio) = parts(env.engine.detail(1).unwrap());
    assert_eq!(popcount(&have), pieces);
    assert_eq!((seeds, leechers), (0, 0), "stopped at completion");
    assert!(ratio >= 0.0 && ratio.is_finite());
    assert!(env.engine.detail(2).is_none(), "unknown id");
}

#[tokio::test]
async fn detail_of_a_seed_peer_counts_seeds() {
    let seeder = Seeder::start_with_limit(&[("slow.bin", 1024 * KIB)], 256 * 1024).await;
    let env = Env::new(vec![seeder.addr]).await;
    env.add(1, env.torrent_req(&seeder), AddOptions::default())
        .await;
    let deadline = Instant::now() + TIMEOUT;
    loop {
        if let Some(d) = env.engine.detail(1) {
            let (_, _, peers, seeds, leechers, _) = parts(d);
            if peers >= 1 {
                assert_eq!(seeds + leechers, peers);
                // The only peer has every piece; librqbit may need a moment to know.
                if seeds == 1 {
                    return;
                }
            }
        }
        assert!(
            Instant::now() < deadline,
            "the seeder never counted as a seed"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test]
async fn pending_torrents_have_no_detail() {
    let env = Env::new(Vec::new()).await;
    let uri = "magnet:?xt=urn:btih:0123456789abcdef0123456789abcdef01234567&dn=x".to_string();
    env.add(
        1,
        env.req(Source::Magnet { uri }),
        AddOptions {
            start_paused: true,
            ..Default::default()
        },
    )
    .await;
    assert!(env.engine.detail(1).is_none());
}

#[tokio::test]
async fn global_limits_apply_to_session() {
    let env = Env::new(Vec::new()).await;
    assert_eq!(env.engine.global_limits(), (None, None));
    // Set before the session runs: applied when it starts.
    env.engine.set_global_download_budget(Some(500_000));
    env.engine.set_global_upload_budget(Some(40_000));
    assert!(!env.engine.session_started());
    env.engine.start_session().await.unwrap();
    assert_eq!(env.engine.global_limits(), (Some(500_000), Some(40_000)));
    // 0 and None are unlimited; too-large values clamp.
    env.engine.set_global_download_budget(Some(0));
    env.engine.set_global_upload_budget(Some(u64::MAX));
    assert_eq!(env.engine.global_limits(), (None, Some(u32::MAX)));
    env.engine.set_global_upload_budget(None);
    env.engine
        .set_global_download_budget(Some(u64::from(u32::MAX) + 1));
    assert_eq!(env.engine.global_limits(), (Some(u32::MAX), None));
}

#[tokio::test]
async fn per_torrent_runtime_limit_unsupported() {
    let seeder = Seeder::start(&[("a.bin", 64 * KIB)]).await;
    let env = Env::new(vec![seeder.addr]).await;
    env.add(1, env.torrent_req(&seeder), AddOptions::default())
        .await;
    let err = env.engine.set_download_limit(1, Some(1000)).unwrap_err();
    assert!(
        matches!(err, EngineError::Unsupported(m) if m.contains("before the torrent starts")),
        "{err:?}"
    );
    assert!(matches!(
        env.engine.set_download_limit(99, Some(1000)),
        Err(EngineError::NotFound(99))
    ));

    // A magnet that has not been added to librqbit yet takes the limit (and clears it).
    let uri = "magnet:?xt=urn:btih:0123456789abcdef0123456789abcdef01234567&dn=x".to_string();
    env.add(
        2,
        env.req(Source::Magnet { uri }),
        AddOptions {
            start_paused: true,
            ..Default::default()
        },
    )
    .await;
    env.engine.set_download_limit(2, Some(5000)).unwrap();
    env.engine.set_download_limit(2, None).unwrap();
}
