//! Fast resume after an engine restart: a torrent stopped cleanly picks up from the
//! pieces it had, without re-checking all its data; anything that makes that record
//! doubtful (no clean stop, files changed, a corrupt piece) falls back to the full check.
//! Network-free: 127.0.0.1 only, DHT, trackers, LSD and UPnP off, peers given explicitly.
//!
//! The tests tell the two apart by filling in the rest of the data on disk while the
//! engine is down (the right bytes, keeping each file's size and, where noted, its
//! modification time): a full check then finds the whole torrent, fast resume only the
//! pieces it had before the restart.

mod support;

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use puppy_core::engine::{AddOptions, Added, Engine, StartMode};
use puppy_core::types::{
    AddRequest, DownloadId, DownloadState, DownloadStatus, RequestContext, Source,
};
use puppy_torrent::{TorrentEngine, TorrentEngineOptions};
use support::{PIECE_LENGTH, Seeder};

const TIMEOUT: Duration = Duration::from_secs(20);
const KIB: usize = 1024;
/// A multi-file torrent of ~1 MiB (16 KiB pieces).
const MULTI: &[(&str, usize)] = &[("a.bin", 600 * KIB), ("sub/b.bin", 400 * KIB + 123)];
/// Seeder upload limit: ~1 MiB takes several seconds, so a download can be caught mid-way.
const SLOW: u32 = 256 * KIB as u32;

async fn engine(root: &Path, peers: Vec<SocketAddr>) -> TorrentEngine {
    TorrentEngine::new(TorrentEngineOptions {
        dht: false,
        upnp: false,
        lsd: false,
        listen_port: 0,
        seed_ratio: 0.0,
        seed_time: Duration::ZERO,
        data_dir: root.join("data"),
        default_dir: root.join("downloads"),
        test_mode: true,
        test_initial_peers: peers,
    })
    .await
    .expect("engine starts")
}

fn torrent_req(root: &Path, seeder: &Seeder) -> AddRequest {
    let path = root.join(format!("{}.torrent", seeder.info_hash_hex));
    std::fs::write(&path, &seeder.torrent_bytes).unwrap();
    AddRequest {
        source: Source::TorrentFile { path },
        output_dir: Some(root.join("out")),
        filename: None,
        context: RequestContext::default(),
        files: Default::default(),
        seed: None,
        checksum: None,
        mirrors: Vec::new(),
        size: None,
    }
}

/// puppy's record that vouches for the stored piece map of download `id`.
fn record(root: &Path, id: DownloadId) -> PathBuf {
    root.join(format!("data/torrents/{id}.resume"))
}

/// The stored piece map of the torrent `hash`.
fn bitv(root: &Path, hash: &str) -> PathBuf {
    root.join(format!("data/torrent/fastresume/{hash}.bitv"))
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
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// The restored download's state once librqbit is done with the data (its piece map is
/// readable).
async fn checked(engine: &TorrentEngine, id: DownloadId) -> DownloadStatus {
    let deadline = Instant::now() + TIMEOUT;
    while engine.piece_bits(id).is_none() {
        assert!(Instant::now() < deadline, "the data check never ended");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    engine.status(id).unwrap()
}

/// Adds `req` fresh into a new engine, waits until part of it is downloaded and pauses
/// it. Returns the engine, what `add` returned, the bytes done and the piece bits.
async fn download_part(
    root: &Path,
    seeder: &Seeder,
    id: DownloadId,
    req: AddRequest,
) -> (TorrentEngine, Added, u64, Vec<bool>) {
    let first = engine(root, vec![seeder.addr]).await;
    let added = first
        .add(id, req, StartMode::Fresh, AddOptions::default())
        .await
        .unwrap();
    wait(&first, id, "transferring", |s| {
        s.state == DownloadState::Active && s.done_bytes > 3 * PIECE_LENGTH as u64
    })
    .await;
    first.pause(id).await.unwrap();
    let paused = first.status(id).unwrap();
    assert_eq!(paused.state, DownloadState::Paused);
    assert!(
        paused.done_bytes < paused.total_bytes.unwrap(),
        "finished before the restart"
    );
    let bits = first.piece_bits(id).expect("piece map of a paused torrent");
    (first, added, paused.done_bytes, bits)
}

/// Downloads part of `req`, then stops the engine cleanly (the daemon's shutdown).
async fn download_part_then_stop(
    root: &Path,
    seeder: &Seeder,
    id: DownloadId,
    req: AddRequest,
) -> (Added, u64, Vec<bool>) {
    let (first, added, done, bits) = download_part(root, seeder, id, req).await;
    first.shutdown().await;
    drop(first);
    (added, done, bits)
}

async fn restore(engine: &TorrentEngine, id: DownloadId, req: AddRequest, added: &Added) {
    let restored = engine
        .add(
            id,
            req,
            StartMode::Restore {
                path: added.path.clone(),
            },
            AddOptions {
                start_paused: true,
                limit: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(restored.path, added.path);
}

/// Writes the seeded bytes over every file of the torrent in `folder` (each file
/// keeps its current size), so all of its data on disk is valid. `keep_mtime`: each
/// file's modification time is set back to what it was.
fn fill_in(folder: &Path, keep_mtime: bool) {
    for (rel, len) in MULTI {
        let path = folder.join(rel);
        let meta = std::fs::metadata(&path).unwrap();
        let mtime = meta.modified().unwrap();
        let mut bytes = support::content(rel, *len);
        bytes.resize(meta.len() as usize, 0);
        std::fs::write(&path, &bytes).unwrap();
        if keep_mtime {
            set_mtime(&path, mtime);
        }
    }
}

fn set_mtime(path: &Path, mtime: SystemTime) {
    std::fs::OpenOptions::new()
        .write(true)
        .open(path)
        .unwrap()
        .set_modified(mtime)
        .unwrap();
}

/// The torrent offset range of file `rel` (the files' order in the torrent is the
/// seeder's, not [`MULTI`]'s).
async fn file_range(engine: &TorrentEngine, id: DownloadId, rel: &str) -> std::ops::Range<u64> {
    let mut offset = 0;
    for f in engine.list_files(id).await.unwrap() {
        if f.path == rel {
            return offset..offset + f.size;
        }
        offset += f.size;
    }
    panic!("{rel} is not in the torrent");
}

/// The pieces lying entirely inside `range`.
fn whole_pieces(range: &std::ops::Range<u64>) -> std::ops::Range<usize> {
    let plen = PIECE_LENGTH as u64;
    (range.start.div_ceil(plen) as usize)..((range.end / plen) as usize)
}

fn total() -> u64 {
    MULTI.iter().map(|f| f.1 as u64).sum()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn clean_restart_resumes_from_the_recorded_pieces() {
    let seeder = Seeder::start_with_limit(MULTI, SLOW).await;
    let root = tempfile::tempdir().unwrap();
    let root = root.path();
    let req = torrent_req(root, &seeder);
    let (added, before, bits) = download_part_then_stop(root, &seeder, 1, req.clone()).await;
    assert!(
        record(root, 1).exists(),
        "the clean stop recorded the pieces"
    );
    assert!(bitv(root, &seeder.info_hash_hex).exists());

    // The rest of the data is there too, but nothing vouches for it: fast resume takes
    // the recorded pieces as they are (a full check would find the whole torrent).
    fill_in(&added.path, true);
    let second = engine(root, Vec::new()).await;
    restore(&second, 1, req, &added).await;
    let st = checked(&second, 1).await;
    assert_eq!(st.state, DownloadState::Paused);
    assert_eq!(
        st.done_bytes, before,
        "progress as it was before the restart"
    );
    assert_eq!(st.total_bytes, Some(total()));
    assert_eq!(
        second.piece_bits(1).unwrap(),
        bits,
        "the exact piece map at once"
    );
    tokio::time::sleep(Duration::from_millis(500)).await;
    let later = second.status(1).unwrap();
    assert_eq!(later.state, DownloadState::Paused, "restored paused");
    assert_eq!(later.done_bytes, before);
    assert!(
        !record(root, 1).exists(),
        "a record is used once: a later unclean stop is checked in full"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn changed_file_falls_back_to_the_full_check() {
    let seeder = Seeder::start_with_limit(MULTI, SLOW).await;
    let root = tempfile::tempdir().unwrap();
    let root = root.path();
    let req = torrent_req(root, &seeder);
    let (added, before, _) = download_part_then_stop(root, &seeder, 1, req.clone()).await;

    // Written while the engine was down: the modification times differ.
    fill_in(&added.path, false);
    let second = engine(root, Vec::new()).await;
    restore(&second, 1, req, &added).await;
    let st = wait(&second, 1, "the full check", |s| {
        s.state == DownloadState::Completed
    })
    .await;
    assert_eq!(st.done_bytes, total(), "all the data on disk was checked");
    assert!(st.done_bytes > before);
    seeder.assert_downloaded_into(&root.join("out"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn truncated_file_falls_back_to_the_full_check() {
    let seeder = Seeder::start_with_limit(MULTI, SLOW).await;
    let root = tempfile::tempdir().unwrap();
    let root = root.path();
    let req = torrent_req(root, &seeder);
    let (added, before, _) = download_part_then_stop(root, &seeder, 1, req.clone()).await;

    // a.bin loses its data (its modification time is kept): nothing of it is had.
    let a = added.path.join("a.bin");
    let mtime = std::fs::metadata(&a).unwrap().modified().unwrap();
    std::fs::File::create(&a).unwrap();
    set_mtime(&a, mtime);
    let second = engine(root, Vec::new()).await;
    restore(&second, 1, req, &added).await;
    let st = checked(&second, 1).await;
    let bits = second.piece_bits(1).unwrap();
    let a = whole_pieces(&file_range(&second, 1, "a.bin").await);
    assert!(
        bits[a].iter().all(|b| !b),
        "no piece of the emptied file is had (done: {}, before: {before})",
        st.done_bytes
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn corrupt_piece_found_by_the_spot_check_falls_back_to_the_full_check() {
    let seeder = Seeder::start_with_limit(MULTI, SLOW).await;
    let root = tempfile::tempdir().unwrap();
    let root = root.path();
    let req = torrent_req(root, &seeder);
    let (first, added, before, bits) = download_part(root, &seeder, 1, req.clone()).await;
    // The piece the spot check always hashes for a file: the first claimed piece that
    // overlaps it, which may be shared with the file before it (the files' order in the
    // torrent follows the seeder's directory listing, so it differs between systems).
    // The torrent's last piece is shorter; skip it so the expected count below holds.
    let plen = PIECE_LENGTH as u64;
    let last = (total().div_ceil(plen) - 1) as usize;
    let (rel, range, piece) = {
        let mut found = None;
        for (rel, _) in MULTI {
            let range = file_range(&first, 1, rel).await;
            let overlapping = (range.start / plen) as usize..range.end.div_ceil(plen) as usize;
            if let Some(p) = overlapping.into_iter().find(|&p| bits[p])
                && p != last
            {
                found = Some((*rel, range, p));
                break;
            }
        }
        found.expect("a file's first claimed piece was had")
    };
    first.shutdown().await;
    drop(first);

    // Same size, same modification time, so the guard on the files passes; a piece the
    // stored map claims is corrupt (the first claimed one of its file: always checked).
    fill_in(&added.path, true);
    let a = added.path.join(rel);
    let mtime = std::fs::metadata(&a).unwrap().modified().unwrap();
    let mut data = std::fs::read(&a).unwrap();
    // A byte of that piece inside this file (the piece may start in the file before).
    data[((piece as u64 * plen).max(range.start) - range.start) as usize] ^= 0xFF;
    std::fs::write(&a, &data).unwrap();
    set_mtime(&a, mtime);

    let second = engine(root, Vec::new()).await;
    restore(&second, 1, req, &added).await;
    let st = checked(&second, 1).await;
    assert_eq!(
        st.done_bytes,
        total() - PIECE_LENGTH as u64,
        "checked in full: everything but the corrupt piece (fast resume: {before})"
    );
    assert!(!second.piece_bits(1).unwrap()[piece]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unclean_stop_falls_back_to_the_full_check() {
    let seeder = Seeder::start_with_limit(MULTI, SLOW).await;
    let root = tempfile::tempdir().unwrap();
    let root = root.path();
    let req = torrent_req(root, &seeder);
    let (added, before, _) = download_part_then_stop(root, &seeder, 1, req.clone()).await;

    // Restored from the clean stop's record, then downloading again when the daemon is
    // killed (no shutdown: the engine is just dropped).
    let second = engine(root, vec![seeder.addr]).await;
    restore(&second, 1, req.clone(), &added).await;
    second.resume(1).await.unwrap();
    wait(&second, 1, "more data", |s| {
        s.state == DownloadState::Active && s.done_bytes > before
    })
    .await;
    drop(second);
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(!record(root, 1).exists(), "nothing vouches for the pieces");

    fill_in(&added.path, true);
    let third = engine(root, Vec::new()).await;
    restore(&third, 1, req, &added).await;
    let st = wait(&third, 1, "the full check", |s| {
        s.state == DownloadState::Completed
    })
    .await;
    assert_eq!(st.done_bytes, total());
    seeder.assert_downloaded_into(&root.join("out"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn remove_clears_the_stored_pieces() {
    let seeder = Seeder::start_with_limit(MULTI, SLOW).await;
    let root = tempfile::tempdir().unwrap();
    let root = root.path();
    let req = torrent_req(root, &seeder);
    let (added, before, _) = download_part_then_stop(root, &seeder, 1, req.clone()).await;
    let map = bitv(root, &seeder.info_hash_hex);

    let second = engine(root, Vec::new()).await;
    restore(&second, 1, req.clone(), &added).await;
    assert_eq!(checked(&second, 1).await.done_bytes, before);
    assert!(map.exists());
    second.remove(1, false).await.unwrap();
    assert!(!map.exists(), "the piece map goes with the download");
    assert!(!record(root, 1).exists());
    second.shutdown().await;
    drop(second);

    // Added again (a new download, its old files still there): nothing is assumed.
    let third = engine(root, Vec::new()).await;
    let readded = third
        .add(
            2,
            torrent_req(root, &seeder),
            StartMode::Fresh,
            AddOptions {
                start_paused: true,
                limit: None,
            },
        )
        .await
        .unwrap();
    assert_ne!(readded.path, added.path, "never over the old files");
    assert_eq!(checked(&third, 2).await.done_bytes, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn removed_while_down_leaves_no_stored_pieces() {
    let seeder = Seeder::start_with_limit(MULTI, SLOW).await;
    let root = tempfile::tempdir().unwrap();
    let root = root.path();
    let req = torrent_req(root, &seeder);
    let (added, _, _) = download_part_then_stop(root, &seeder, 1, req.clone()).await;
    let map = bitv(root, &seeder.info_hash_hex);
    assert!(map.exists());

    // Its `.torrent` is gone: restored as needing attention, never added to librqbit.
    std::fs::remove_file(root.join("data/torrents/1.torrent")).unwrap();
    let Source::TorrentFile { path: original } = &req.source else {
        unreachable!()
    };
    std::fs::remove_file(original).unwrap();
    let second = engine(root, Vec::new()).await;
    restore(&second, 1, req, &added).await;
    assert!(matches!(
        second.status(1).unwrap().state,
        DownloadState::NeedsAttention { .. }
    ));
    second.remove(1, false).await.unwrap();
    assert!(!record(root, 1).exists());
    // The piece map nothing vouches for is dropped when the session starts.
    second.start_session().await.unwrap();
    assert!(!map.exists());
}

/// Downloads `MULTI` completely (both seeding targets 0: paused at completion) and lets
/// the engine see the completion.
async fn download_all(root: &Path, seeder: &Seeder) -> (TorrentEngine, Added) {
    let first = engine(root, vec![seeder.addr]).await;
    let added = first
        .add(
            1,
            torrent_req(root, seeder),
            StartMode::Fresh,
            AddOptions::default(),
        )
        .await
        .unwrap();
    wait(&first, 1, "completion", |s| {
        s.state == DownloadState::Completed
    })
    .await;
    // A few looks of the poll task: the completion is noted.
    tokio::time::sleep(Duration::from_millis(1200)).await;
    (first, added)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn completed_torrent_resumes_without_a_full_check() {
    let seeder = Seeder::start(MULTI).await;
    let root = tempfile::tempdir().unwrap();
    let root = root.path();
    let (first, added) = download_all(root, &seeder).await;
    first.shutdown().await;
    drop(first);
    assert!(record(root, 1).exists(), "untouched files are vouched for");

    let second = engine(root, Vec::new()).await;
    restore(&second, 1, torrent_req(root, &seeder), &added).await;
    let st = checked(&second, 1).await;
    assert_eq!(st.state, DownloadState::Completed);
    assert_eq!(st.done_bytes, total());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn file_cut_short_while_running_is_checked_in_full() {
    let seeder = Seeder::start(MULTI).await;
    let root = tempfile::tempdir().unwrap();
    let root = root.path();
    let (first, added) = download_all(root, &seeder).await;

    // While the engine runs: a.bin loses its last 16 pieces' worth, its modification
    // time put back (so only the sizes tell).
    let a = added.path.join("a.bin");
    let mtime = std::fs::metadata(&a).unwrap().modified().unwrap();
    let f = std::fs::OpenOptions::new().write(true).open(&a).unwrap();
    f.set_len((600 * KIB - 16 * PIECE_LENGTH as usize) as u64)
        .unwrap();
    f.set_modified(mtime).unwrap();
    drop(f);
    first.shutdown().await;
    drop(first);
    assert!(
        !record(root, 1).exists(),
        "nothing vouches for the cut file"
    );

    let second = engine(root, Vec::new()).await;
    restore(&second, 1, torrent_req(root, &seeder), &added).await;
    // (Its status stays `Completed`: it had stopped seeding, and is not seeded again.)
    checked(&second, 1).await;
    let a = file_range(&second, 1, "a.bin").await;
    let cut = whole_pieces(&(a.end - 16 * PIECE_LENGTH as u64..a.end));
    assert!(
        second.piece_bits(1).unwrap()[cut].iter().all(|b| !b),
        "checked in full: the cut pieces are missing"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn file_rewritten_while_running_is_checked_in_full() {
    let seeder = Seeder::start(MULTI).await;
    let root = tempfile::tempdir().unwrap();
    let root = root.path();
    let (first, added) = download_all(root, &seeder).await;

    // While the engine runs: a.bin rewritten in place (same size, other bytes).
    std::fs::write(added.path.join("a.bin"), vec![0u8; 600 * KIB]).unwrap();
    first.shutdown().await;
    drop(first);
    assert!(
        !record(root, 1).exists(),
        "changed after the torrent's last write"
    );

    let second = engine(root, Vec::new()).await;
    restore(&second, 1, torrent_req(root, &seeder), &added).await;
    checked(&second, 1).await;
    let a = whole_pieces(&file_range(&second, 1, "a.bin").await);
    assert!(second.piece_bits(1).unwrap()[a].iter().all(|b| !b));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn remove_during_shutdown_stores_nothing() {
    let seeder = Seeder::start_with_limit(MULTI, SLOW).await;
    let root = tempfile::tempdir().unwrap();
    let root = root.path();
    let (first, _, _, _) = download_part(root, &seeder, 1, torrent_req(root, &seeder)).await;
    let (removed, ()) = tokio::join!(first.remove(1, false), first.shutdown());
    removed.unwrap();
    drop(first);
    assert!(!record(root, 1).exists());
    assert!(!bitv(root, &seeder.info_hash_hex).exists());
}
