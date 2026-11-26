//! Seeding after completion (ratio / time targets, cumulative counters across restarts,
//! live overrides) and the exact piece map. Network-free: 127.0.0.1 only, DHT, trackers,
//! LSD and UPnP off; uploads go to a raw librqbit [`Leecher`] whose only peer is puppy.

mod support;

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use puppy_core::engine::{AddOptions, Added, Engine, EngineEvent, StartMode};
use puppy_core::types::{
    AddRequest, AttentionReason, DownloadId, DownloadState, DownloadStatus, EngineDetail,
    FileChoice, RequestContext, SeedPolicy, Source,
};
use puppy_torrent::{TorrentEngine, TorrentEngineOptions};
use support::{Leecher, Seeder};
use tokio::sync::broadcast::Receiver;

const TIMEOUT: Duration = Duration::from_secs(30);
const KIB: usize = 1024;
/// puppy's upload budget in the ratio tests: slow enough that a 500 ms poll overshoots
/// a target by little.
const UP: u64 = 64 * 1024;

async fn engine(root: &Path, peers: Vec<SocketAddr>, ratio: f32, time: Duration) -> TorrentEngine {
    TorrentEngine::new(TorrentEngineOptions {
        dht: false,
        upnp: false,
        lsd: false,
        listen_port: 0,
        seed_ratio: ratio,
        seed_time: time,
        data_dir: root.join("data"),
        default_dir: root.join("downloads"),
        test_mode: true,
        test_initial_peers: peers,
    })
    .await
    .expect("engine starts")
}

fn torrent_req(root: &Path, seeder: &Seeder, seed: Option<SeedPolicy>) -> AddRequest {
    let path = root.join(format!("{}.torrent", seeder.info_hash_hex));
    std::fs::write(&path, &seeder.torrent_bytes).unwrap();
    AddRequest {
        source: Source::TorrentFile { path },
        output_dir: Some(root.join("out")),
        filename: None,
        context: RequestContext::default(),
        files: Default::default(),
        seed,
        checksum: None,
        mirrors: Vec::new(),
        size: None,
    }
}

async fn add(engine: &TorrentEngine, id: DownloadId, req: AddRequest) -> Added {
    engine
        .add(id, req, StartMode::Fresh, AddOptions::default())
        .await
        .expect("added")
}

/// Restores like the daemon does (paused; the engine decides what runs).
async fn restore(engine: &TorrentEngine, id: DownloadId, req: AddRequest, path: &Path) -> Added {
    engine
        .add(
            id,
            req,
            StartMode::Restore {
                path: path.to_owned(),
            },
            AddOptions {
                start_paused: true,
                ..Default::default()
            },
        )
        .await
        .expect("restored")
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

fn is_seeding(s: &DownloadStatus) -> bool {
    matches!(s.state, DownloadState::Seeding { .. })
}

/// `(uploaded, ratio, seeded_secs, ratio_target, time_target_secs)` of a seeding state.
fn seeding(s: &DownloadState) -> (u64, f32, u64, Option<f32>, Option<u64>) {
    match s {
        DownloadState::Seeding {
            uploaded,
            ratio,
            seeded_secs,
            ratio_target,
            time_target_secs,
        } => (
            *uploaded,
            *ratio,
            *seeded_secs,
            *ratio_target,
            *time_target_secs,
        ),
        other => panic!("not seeding: {other:?}"),
    }
}

/// Every event received so far.
fn drain(rx: &mut Receiver<EngineEvent>) -> Vec<EngineEvent> {
    let mut got = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        got.push(ev);
    }
    got
}

fn position(events: &[EngineEvent], f: impl Fn(&EngineEvent) -> bool) -> Option<usize> {
    events.iter().position(f)
}

fn completed(id: DownloadId) -> impl Fn(&EngineEvent) -> bool {
    move |e| matches!(e, EngineEvent::Completed { id: i } if *i == id)
}

fn seeding_finished(id: DownloadId) -> impl Fn(&EngineEvent) -> bool {
    move |e| matches!(e, EngineEvent::SeedingFinished { id: i } if *i == id)
}

/// Waits until `rx` delivers an event matching `f`; returns the events seen up to it.
async fn until(
    rx: &mut Receiver<EngineEvent>,
    f: impl Fn(&EngineEvent) -> bool,
) -> Vec<EngineEvent> {
    let mut got = Vec::new();
    let deadline = tokio::time::Instant::now() + TIMEOUT;
    loop {
        let ev = tokio::time::timeout_at(deadline, rx.recv())
            .await
            .unwrap_or_else(|_| panic!("timed out; events so far: {got:?}"))
            .expect("event channel open");
        let hit = f(&ev);
        got.push(ev);
        if hit {
            return got;
        }
    }
}

fn seed_file(root: &Path, id: DownloadId) -> PathBuf {
    root.join(format!("data/torrents/{id}.seed"))
}

fn seed_record(root: &Path, id: DownloadId) -> serde_json::Value {
    let text = std::fs::read_to_string(seed_file(root, id)).expect("seed record written");
    serde_json::from_str(&text).unwrap()
}

/// A leecher downloading from puppy gets nothing (puppy does not upload).
async fn assert_not_uploading(engine: &TorrentEngine, seeder: &Seeder) {
    let l = Leecher::start(&seeder.torrent_bytes, engine.listen_addr().unwrap(), None).await;
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(l.progress(), 0, "puppy still uploads");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn seeds_until_ratio() {
    let size = 256 * KIB;
    let seeder = Seeder::start(&[("r.bin", size)]).await;
    let root = tempfile::tempdir().unwrap();
    let engine = engine(root.path(), vec![seeder.addr], 0.5, Duration::ZERO).await;
    engine.set_global_upload_budget(Some(UP));
    let mut rx = engine.subscribe();
    add(&engine, 1, torrent_req(root.path(), &seeder, None)).await;

    let st = wait(&engine, 1, "seeding", is_seeding).await;
    let (_, ratio, _, ratio_target, time_target) = seeding(&st.state);
    assert_eq!((ratio_target, time_target), (Some(0.5), None), "0 disables");
    assert!(ratio.is_finite());
    assert_eq!(st.done_bytes, size as u64);

    let leecher = Leecher::start(&seeder.torrent_bytes, engine.listen_addr().unwrap(), None).await;
    let events = until(&mut rx, seeding_finished(1)).await;
    let c = position(&events, completed(1)).expect("Completed sent");
    let f = position(&events, seeding_finished(1)).unwrap();
    assert!(c < f, "Completed (data) before SeedingFinished: {events:?}");
    let st = wait(&engine, 1, "completed", |s| {
        s.state == DownloadState::Completed
    })
    .await;
    assert_eq!(st.up_bps, 0);
    let Some(EngineDetail::Torrent { ratio, .. }) = engine.detail(1) else {
        panic!("no detail");
    };
    assert!(ratio >= 0.5, "stopped at the target: {ratio}");
    assert!(!leecher.finished(), "stopped before uploading everything");
    drop(leecher);
    assert_not_uploading(&engine, &seeder).await;
    let rec = seed_record(root.path(), 1);
    assert!(
        rec["base_uploaded"].as_u64().unwrap() >= (size / 2) as u64,
        "{rec}"
    );
    engine.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn seeds_until_time() {
    let seeder = Seeder::start(&[("t.bin", 64 * KIB)]).await;
    let root = tempfile::tempdir().unwrap();
    let engine = engine(root.path(), vec![seeder.addr], 0.0, Duration::from_secs(1)).await;
    let mut rx = engine.subscribe();
    add(&engine, 1, torrent_req(root.path(), &seeder, None)).await;
    let st = wait(&engine, 1, "seeding", is_seeding).await;
    let started = Instant::now();
    assert_eq!(seeding(&st.state).3, None, "ratio 0 disables");
    assert_eq!(seeding(&st.state).4, Some(1));

    until(&mut rx, seeding_finished(1)).await;
    let elapsed = started.elapsed();
    assert!(elapsed >= Duration::from_millis(500), "{elapsed:?}");
    assert!(elapsed < Duration::from_secs(5), "{elapsed:?}");
    assert_eq!(engine.status(1).unwrap().state, DownloadState::Completed);
    assert!(seed_record(root.path(), 1)["seeded_secs"].as_u64().unwrap() >= 1);
    assert_not_uploading(&engine, &seeder).await;
    engine.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn both_zero_stops_immediately() {
    let seeder = Seeder::start(&[("z.bin", 64 * KIB)]).await;
    let root = tempfile::tempdir().unwrap();
    let engine = engine(root.path(), vec![seeder.addr], 0.0, Duration::ZERO).await;
    let mut rx = engine.subscribe();
    add(&engine, 1, torrent_req(root.path(), &seeder, None)).await;
    let events = until(&mut rx, completed(1)).await;
    assert!(
        !events.iter().any(|e| matches!(
            e,
            EngineEvent::StateChanged {
                state: DownloadState::Seeding { .. },
                ..
            }
        )),
        "{events:?}"
    );
    assert_eq!(engine.status(1).unwrap().state, DownloadState::Completed);
    assert_not_uploading(&engine, &seeder).await;
    let later = drain(&mut rx);
    assert!(position(&later, seeding_finished(1)).is_none(), "{later:?}");
    engine.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pause_while_seeding_completes() {
    let seeder = Seeder::start(&[("p.bin", 64 * KIB)]).await;
    let root = tempfile::tempdir().unwrap();
    // Globals stop at completion; the torrent's own ratio keeps it seeding.
    let engine = engine(root.path(), vec![seeder.addr], 0.0, Duration::ZERO).await;
    let mut rx = engine.subscribe();
    let policy = SeedPolicy {
        ratio: Some(5.0),
        time_secs: None,
    };
    add(&engine, 1, torrent_req(root.path(), &seeder, Some(policy))).await;
    let st = wait(&engine, 1, "seeding", is_seeding).await;
    assert_eq!(seeding(&st.state).3, Some(5.0));
    assert_eq!(seeding(&st.state).4, None, "inherits the global 0");

    engine.pause(1).await.unwrap();
    assert_eq!(engine.status(1).unwrap().state, DownloadState::Completed);
    until(&mut rx, seeding_finished(1)).await;
    engine.resume(1).await.unwrap();
    tokio::time::sleep(Duration::from_millis(700)).await;
    assert_eq!(engine.status(1).unwrap().state, DownloadState::Completed);
    assert_not_uploading(&engine, &seeder).await;
    engine.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn set_seed_applies_live() {
    let seeder = Seeder::start(&[("l.bin", 64 * KIB)]).await;
    let root = tempfile::tempdir().unwrap();
    let engine = engine(
        root.path(),
        vec![seeder.addr],
        0.0,
        Duration::from_secs(3600),
    )
    .await;
    let mut rx = engine.subscribe();
    add(&engine, 1, torrent_req(root.path(), &seeder, None)).await;
    let st = wait(&engine, 1, "seeding", is_seeding).await;
    assert_eq!(seeding(&st.state).4, Some(3600));

    // A ratio of its own; the time still inherits.
    engine
        .set_seed(
            1,
            SeedPolicy {
                ratio: Some(2.0),
                time_secs: None,
            },
        )
        .await
        .unwrap();
    let st = engine.status(1).unwrap();
    assert_eq!(
        (seeding(&st.state).3, seeding(&st.state).4),
        (Some(2.0), Some(3600))
    );
    // A met time: stops at the next look.
    engine
        .set_seed(
            1,
            SeedPolicy {
                ratio: None,
                time_secs: Some(1),
            },
        )
        .await
        .unwrap();
    until(&mut rx, seeding_finished(1)).await;
    assert_eq!(engine.status(1).unwrap().state, DownloadState::Completed);
    assert!(engine.set_seed(9, SeedPolicy::default()).await.is_err());
    engine.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ratio_cumulative_across_restart() {
    let size = 256 * KIB;
    let target = 0.75;
    let seeder = Seeder::start(&[("c.bin", size)]).await;
    let root = tempfile::tempdir().unwrap();
    let first = engine(root.path(), vec![seeder.addr], target, Duration::ZERO).await;
    first.set_global_upload_budget(Some(UP));
    let added = add(&first, 1, torrent_req(root.path(), &seeder, None)).await;
    wait(&first, 1, "seeding", is_seeding).await;

    // Part of the target in the first run.
    let leecher = Leecher::start(&seeder.torrent_bytes, first.listen_addr().unwrap(), None).await;
    let st = wait(&first, 1, "some upload", |s| {
        matches!(s.state, DownloadState::Seeding { uploaded, .. } if uploaded >= 96 * KIB as u64)
    })
    .await;
    let before = seeding(&st.state).0;
    drop(leecher);
    first.shutdown().await;
    drop(first);
    let base = seed_record(root.path(), 1)["base_uploaded"]
        .as_u64()
        .unwrap();
    assert!(base >= before, "{base} < {before}");
    assert!(
        (base as f64) < target as f64 * size as f64,
        "target met in run 1"
    );

    let second = engine(root.path(), vec![seeder.addr], target, Duration::ZERO).await;
    second.set_global_upload_budget(Some(UP));
    let mut rx = second.subscribe();
    restore(
        &second,
        1,
        torrent_req(root.path(), &seeder, None),
        &added.path,
    )
    .await;
    let st = wait(&second, 1, "seeding again", is_seeding).await;
    assert!(seeding(&st.state).0 >= base, "the persisted upload counts");

    let leecher = Leecher::start(&seeder.torrent_bytes, second.listen_addr().unwrap(), None).await;
    until(&mut rx, seeding_finished(1)).await;
    let total = seed_record(root.path(), 1)["base_uploaded"]
        .as_u64()
        .unwrap();
    assert!(
        total as f64 >= target as f64 * size as f64,
        "stopped below target: {total}"
    );
    assert!(
        ((total - base) as f64) < target as f64 * size as f64,
        "run 2 alone reached the target: the base was not counted ({base} + {})",
        total - base
    );
    drop(leecher);
    second.shutdown().await;
}

/// A multi-file torrent puppy cannot download (no peers): only used to choose files.
fn choosing_req(root: &Path) -> AddRequest {
    let bytes = support::craft_torrent("pick", 0, Some(&[(&["a.bin"], 1000), (&["b.bin"], 2000)]));
    let path = root.join("pick.torrent");
    std::fs::write(&path, bytes).unwrap();
    AddRequest {
        source: Source::TorrentFile { path },
        output_dir: Some(root.join("out")),
        filename: None,
        context: RequestContext::default(),
        files: FileChoice::Ask,
        seed: None,
        checksum: None,
        mirrors: Vec::new(),
        size: None,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn restart_restores_seeding_and_choosing() {
    let seeder = Seeder::start(&[("s.bin", 64 * KIB)]).await;
    let root = tempfile::tempdir().unwrap();
    let first = engine(
        root.path(),
        vec![seeder.addr],
        0.0,
        Duration::from_secs(3600),
    )
    .await;
    let added = add(&first, 1, torrent_req(root.path(), &seeder, None)).await;
    let choosing = add(&first, 2, choosing_req(root.path())).await;
    assert_eq!(first.status(2).unwrap().state, DownloadState::ChoosingFiles);
    wait(
        &first,
        1,
        "seeded a while",
        |s| matches!(s.state, DownloadState::Seeding { seeded_secs, .. } if seeded_secs >= 1),
    )
    .await;
    first.shutdown().await;
    drop(first);
    let secs = seed_record(root.path(), 1)["seeded_secs"].as_u64().unwrap();
    assert!(secs >= 1, "seeded time persisted at shutdown");

    let second = engine(
        root.path(),
        vec![seeder.addr],
        0.0,
        Duration::from_secs(3600),
    )
    .await;
    let mut rx = second.subscribe();
    restore(
        &second,
        1,
        torrent_req(root.path(), &seeder, None),
        &added.path,
    )
    .await;
    restore(&second, 2, choosing_req(root.path()), &choosing.path).await;
    let st = second.status(1).unwrap();
    assert!(
        is_seeding(&st),
        "seeding at once (re-check pending): {st:?}"
    );
    assert!(seeding(&st.state).2 >= secs);
    assert_eq!(
        second.status(2).unwrap().state,
        DownloadState::ChoosingFiles
    );
    // Live again: seeding continues (the time goes on from the persisted value).
    wait(
        &second,
        1,
        "seeding time advancing",
        |s| matches!(s.state, DownloadState::Seeding { seeded_secs, .. } if seeded_secs > secs),
    )
    .await;
    let l = Leecher::start(&seeder.torrent_bytes, second.listen_addr().unwrap(), None).await;
    let deadline = Instant::now() + TIMEOUT;
    while !l.finished() {
        assert!(
            Instant::now() < deadline,
            "the restored torrent does not seed"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    drop(l);
    tokio::time::sleep(Duration::from_millis(700)).await;
    let events = drain(&mut rx);
    assert!(position(&events, completed(1)).is_none(), "{events:?}");
    second.shutdown().await;
    drop(second);

    // A target met by the persisted counters: Completed at once.
    let third = engine(
        root.path(),
        vec![seeder.addr],
        0.0,
        Duration::from_secs(3600),
    )
    .await;
    let policy = SeedPolicy {
        ratio: None,
        time_secs: Some(1),
    };
    restore(
        &third,
        1,
        torrent_req(root.path(), &seeder, Some(policy)),
        &added.path,
    )
    .await;
    assert_eq!(third.status(1).unwrap().state, DownloadState::Completed);
    tokio::time::sleep(Duration::from_millis(1200)).await;
    assert_eq!(third.status(1).unwrap().state, DownloadState::Completed);
    third.remove(1, false).await.unwrap();
    assert!(
        !seed_file(root.path(), 1).exists(),
        "forgotten with the download"
    );
    third.shutdown().await;
}

fn popcount(have: &[u8]) -> u32 {
    have.iter().map(|b| b.count_ones()).sum()
}

fn piece_map(d: Option<EngineDetail>) -> (u32, Vec<u8>) {
    match d {
        Some(EngineDetail::Torrent {
            piece_count, have, ..
        }) => (piece_count, have),
        other => panic!("not a torrent detail: {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn exact_piece_map_matches_api() {
    // 1 MiB (64 pieces) from a seeder throttled to 256 KiB/s: several seconds in flight.
    let seeder = Seeder::start_with_limit(&[("slow.bin", 1024 * KIB)], 256 * 1024).await;
    let root = tempfile::tempdir().unwrap();
    let engine = engine(root.path(), vec![seeder.addr], 0.0, Duration::ZERO).await;
    add(&engine, 1, torrent_req(root.path(), &seeder, None)).await;
    let deadline = Instant::now() + TIMEOUT;
    loop {
        assert!(
            Instant::now() < deadline,
            "never saw a stable mid-download map"
        );
        // librqbit's bits, piece by piece (not puppy's encoding), around puppy's map.
        let a = engine.piece_bits(1);
        let d = engine.detail(1);
        let b = engine.piece_bits(1);
        if let (Some(a), Some(_)) = (&a, &d)
            && Some(a) == b.as_ref()
        {
            let got = a.iter().filter(|&&x| x).count();
            if got > 0 && got < a.len() {
                let (n, have) = piece_map(d);
                assert_eq!(n as usize, a.len());
                assert_eq!(n, 64);
                assert_eq!(have.len(), 8, "one bit per piece, MSB first, byte padded");
                for (i, &bit) in a.iter().enumerate() {
                    let ours = have[i / 8] & (0x80 >> (i % 8)) != 0;
                    assert_eq!(ours, bit, "piece {i}");
                }
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    wait(&engine, 1, "completed", |s| {
        s.state == DownloadState::Completed
    })
    .await;
    let (n, have) = piece_map(engine.detail(1));
    assert_eq!(popcount(&have), n);
    engine.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn piece_map_fallbacks() {
    let seeder = Seeder::start(&[("f.bin", 128 * KIB)]).await;
    let root = tempfile::tempdir().unwrap();
    let first = engine(root.path(), vec![seeder.addr], 0.0, Duration::from_secs(1)).await;
    let mut rx = first.subscribe();
    let added = add(&first, 1, torrent_req(root.path(), &seeder, None)).await;
    until(&mut rx, seeding_finished(1)).await;
    // Stopped (paused in librqbit): still the full map.
    let (n, have) = piece_map(first.detail(1));
    assert_eq!((n, popcount(&have)), (8, 8));
    first.shutdown().await;
    drop(first);

    // Restored completed, before librqbit is done checking (or after): every piece of
    // the (selected) files is shown, never an empty map.
    let second = engine(root.path(), vec![seeder.addr], 0.0, Duration::from_secs(1)).await;
    restore(
        &second,
        1,
        torrent_req(root.path(), &seeder, None),
        &added.path,
    )
    .await;
    assert_eq!(second.status(1).unwrap().state, DownloadState::Completed);
    let (n, have) = piece_map(second.detail(1));
    assert_eq!((n, popcount(&have)), (8, 8));
    second.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn failed_recheck_needs_attention_then_downloads_again() {
    let seeder = Seeder::start(&[("k.bin", 128 * KIB)]).await;
    let root = tempfile::tempdir().unwrap();
    let first = engine(
        root.path(),
        vec![seeder.addr],
        0.0,
        Duration::from_secs(3600),
    )
    .await;
    let added = add(&first, 1, torrent_req(root.path(), &seeder, None)).await;
    wait(&first, 1, "seeding", is_seeding).await;
    first.shutdown().await;
    drop(first);
    // Someone changes the completed file.
    let mut data = std::fs::read(&added.path).unwrap();
    data[0] ^= 0xFF;
    std::fs::write(&added.path, &data).unwrap();

    // No peers: nothing can repair it behind puppy's back.
    let second = engine(root.path(), Vec::new(), 0.0, Duration::from_secs(3600)).await;
    let mut rx = second.subscribe();
    restore(
        &second,
        1,
        torrent_req(root.path(), &seeder, None),
        &added.path,
    )
    .await;
    let attention = |s: &DownloadStatus| {
        matches!(&s.state, DownloadState::NeedsAttention {
            reason: AttentionReason::ValidationFailed { detail }
        } if detail == "data changed on disk; resume to download again")
    };
    wait(&second, 1, "needs attention", attention).await;
    tokio::time::sleep(Duration::from_millis(1200)).await;
    let st = second.status(1).unwrap();
    assert!(attention(&st), "stays until resumed: {st:?}");
    let rec = seed_record(root.path(), 1);
    assert_eq!(rec["last_completed_unix"], serde_json::Value::Null, "{rec}");
    assert_eq!(rec["stopped"], false);

    second.resume(1).await.unwrap();
    let _copy = support::serving_copy(&seeder, second.listen_addr().unwrap()).await;
    until(&mut rx, completed(1)).await;
    wait(&second, 1, "seeding again", is_seeding).await;
    assert!(
        std::fs::read(&added.path).unwrap() == support::content("k.bin", 128 * KIB),
        "repaired"
    );
    second.shutdown().await;
}

/// Complete data restored paused without a seeding record (lost, or never written: the
/// engine sees a download whose data turns out complete): `Paused` until resumed, then
/// live and seeding (its clock running); with both targets 0, `Completed`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn complete_data_without_record_seeds_on_resume() {
    let seeder = Seeder::start(&[("r.bin", 64 * KIB)]).await;
    let root = tempfile::tempdir().unwrap();
    let hour = Duration::from_secs(3600);
    let first = engine(root.path(), vec![seeder.addr], 0.0, hour).await;
    let added = add(&first, 1, torrent_req(root.path(), &seeder, None)).await;
    wait(&first, 1, "seeding", is_seeding).await;
    first.shutdown().await;
    drop(first);
    let total = Some(64 * KIB as u64);

    for resume_at_once in [false, true] {
        std::fs::remove_file(seed_file(root.path(), 1)).unwrap();
        let next = engine(root.path(), Vec::new(), 0.0, hour).await;
        restore(
            &next,
            1,
            torrent_req(root.path(), &seeder, None),
            &added.path,
        )
        .await;
        if !resume_at_once {
            let st = wait(&next, 1, "checked", |s| s.done_bytes == 64 * KIB as u64).await;
            assert_eq!(st.state, DownloadState::Paused, "not seeding while paused");
            tokio::time::sleep(Duration::from_millis(700)).await;
            assert_eq!(next.status(1).unwrap().state, DownloadState::Paused);
        }
        next.resume(1).await.unwrap();
        let st = wait(
            &next,
            1,
            "seeding, its clock running",
            |s| matches!(s.state, DownloadState::Seeding { seeded_secs, .. } if seeded_secs >= 1),
        )
        .await;
        assert_eq!(st.total_bytes, total);
        next.shutdown().await;
    }

    // Both targets 0: complete is `Completed`, and it stays so on resume.
    std::fs::remove_file(seed_file(root.path(), 1)).unwrap();
    let last = engine(root.path(), Vec::new(), 0.0, Duration::ZERO).await;
    restore(
        &last,
        1,
        torrent_req(root.path(), &seeder, None),
        &added.path,
    )
    .await;
    wait(&last, 1, "completed", |s| {
        s.state == DownloadState::Completed
    })
    .await;
    last.resume(1).await.unwrap();
    tokio::time::sleep(Duration::from_millis(1200)).await;
    assert_eq!(last.status(1).unwrap().state, DownloadState::Completed);
    last.shutdown().await;
}
