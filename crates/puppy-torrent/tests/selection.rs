//! File selection: choosing files once a torrent's contents are known, downloading only
//! those, restore and delete. Network-free: 127.0.0.1 only, peers given explicitly.

mod support;

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use puppy_core::engine::{AddOptions, Added, Engine, EngineError, StartMode};
use puppy_core::types::{
    AddRequest, DownloadId, DownloadState, DownloadStatus, FileChoice, RequestContext, Selection,
    Source, TorrentFile,
};
use puppy_torrent::{TorrentEngine, TorrentEngineOptions, resolve};
use support::{MULTI_NAME, PIECE_LENGTH, Seeder};

const TIMEOUT: Duration = Duration::from_secs(20);
const P: usize = PIECE_LENGTH as usize;
/// Nested folders, same file names in different folders. Every file is a whole number
/// of pieces, so no piece spans a selected and an unselected file (librqbit then
/// writes nothing into unselected files).
const TREE: &[(&str, usize)] = &[
    ("s1/e01.mkv", 3 * P),
    ("s1/info.txt", P),
    ("s2/e01.mkv", 2 * P),
    ("s2/info.txt", P),
    ("extra/deep/x.mkv", P),
    ("readme.txt", P),
];

fn files(paths: &[&str]) -> Vec<TorrentFile> {
    paths
        .iter()
        .enumerate()
        .map(|(index, p)| TorrentFile {
            index,
            path: (*p).into(),
            size: 1,
        })
        .collect()
}

fn sel(indices: &[usize], globs: &[&str]) -> Selection {
    Selection {
        indices: indices.to_vec(),
        globs: globs.iter().map(|g| g.to_string()).collect(),
    }
}

fn select(indices: &[usize], globs: &[&str]) -> FileChoice {
    FileChoice::Select {
        selection: sel(indices, globs),
    }
}

struct Env {
    root: tempfile::TempDir,
    engine: TorrentEngine,
    peers: Vec<SocketAddr>,
}

impl Env {
    async fn new(peers: Vec<SocketAddr>) -> Env {
        let root = tempfile::tempdir().unwrap();
        let engine = engine(root.path(), peers.clone()).await;
        Env {
            root,
            engine,
            peers,
        }
    }

    /// A new engine over the same data dir (a restart).
    async fn restart(&mut self) {
        self.engine.shutdown().await;
        self.engine = engine(self.root.path(), self.peers.clone()).await;
    }

    fn path(&self, rel: &str) -> PathBuf {
        self.root.path().join(rel)
    }

    fn out(&self) -> PathBuf {
        self.path("out")
    }

    fn folder(&self) -> PathBuf {
        self.out().join(MULTI_NAME)
    }

    fn selection_file(&self, id: DownloadId) -> PathBuf {
        self.path(&format!("data/torrents/{id}.selection"))
    }

    fn req(&self, source: Source, files: FileChoice) -> AddRequest {
        AddRequest {
            source,
            output_dir: Some(self.out()),
            filename: None,
            context: RequestContext::default(),
            files,
            seed: None,
        }
    }

    fn torrent_req(&self, seeder: &Seeder, files: FileChoice) -> AddRequest {
        let path = self.path(&format!("{}.torrent", seeder.info_hash_hex));
        std::fs::write(&path, &seeder.torrent_bytes).unwrap();
        self.req(Source::TorrentFile { path }, files)
    }

    fn magnet_req(&self, seeder: &Seeder, files: FileChoice) -> AddRequest {
        self.req(
            Source::Magnet {
                uri: seeder.magnet.clone(),
            },
            files,
        )
    }

    async fn add(&self, id: DownloadId, req: AddRequest) -> Result<Added, EngineError> {
        self.engine
            .add(id, req, StartMode::Fresh, AddOptions::default())
            .await
    }

    async fn restore(&self, id: DownloadId, req: AddRequest, path: &Path) -> Added {
        self.engine
            .add(
                id,
                req,
                StartMode::Restore {
                    path: path.to_owned(),
                },
                AddOptions {
                    start_paused: true,
                    limit: None,
                },
            )
            .await
            .unwrap()
    }

    async fn wait(
        &self,
        id: DownloadId,
        what: &str,
        pred: impl Fn(&DownloadStatus) -> bool,
    ) -> DownloadStatus {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            let st = self.engine.status(id).expect("download known");
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

    /// The indices of the files whose path ends with any of `suffixes`.
    async fn indices_of(&self, id: DownloadId, suffixes: &[&str]) -> Vec<usize> {
        self.engine
            .list_files(id)
            .await
            .unwrap()
            .into_iter()
            .filter(|f| suffixes.iter().any(|s| f.path.ends_with(s)))
            .map(|f| f.index)
            .collect()
    }
}

async fn engine(root: &Path, peers: Vec<SocketAddr>) -> TorrentEngine {
    TorrentEngine::new(TorrentEngineOptions {
        dht: false,
        upnp: false,
        lsd: false,
        listen_port: 0,
        seed_ratio: 0.0,
        data_dir: root.join("data"),
        default_dir: root.join("downloads"),
        test_mode: true,
        test_initial_peers: peers,
    })
    .await
    .expect("engine starts")
}

fn size_of(paths: &[&str]) -> u64 {
    TREE.iter()
        .filter(|(p, _)| paths.contains(p))
        .map(|(_, l)| *l as u64)
        .sum()
}

/// The selected files are complete; every other file is absent or empty (librqbit
/// creates a placeholder for each file of the torrent, selected or not).
fn assert_only(folder: &Path, selected: &[&str]) {
    for (rel, len) in TREE {
        let path = folder.join(rel);
        if selected.contains(rel) {
            let got = std::fs::read(&path).unwrap_or_else(|e| panic!("reading {rel}: {e}"));
            assert!(got == support::content(rel, *len), "{rel} differs");
        } else {
            let len = std::fs::metadata(&path).map_or(0, |m| m.len());
            assert_eq!(len, 0, "unselected {rel} has data");
        }
    }
}

#[test]
fn resolve_indices_and_globs() {
    let f = files(&[
        "s1/e01.mkv",
        "s1/info.txt",
        "s2/e01.mkv",
        "extra/deep/x.mkv",
        "README.txt",
    ]);
    // A union, sorted, without duplicates.
    assert_eq!(resolve(&f, &sel(&[4, 0, 0], &[])), Ok(vec![0, 4]));
    // A glob without `/` matches file names in every folder.
    assert_eq!(resolve(&f, &sel(&[], &["*.mkv"])), Ok(vec![0, 2, 3]));
    assert_eq!(resolve(&f, &sel(&[1], &["e01.*"])), Ok(vec![0, 1, 2]));
    // With `/`: the whole relative path; `*` does not cross `/`, `**` does.
    assert_eq!(resolve(&f, &sel(&[], &["s1/*"])), Ok(vec![0, 1]));
    assert!(resolve(&f, &sel(&[], &["extra/*.mkv"])).is_err());
    assert_eq!(resolve(&f, &sel(&[], &["extra/**/*.mkv"])), Ok(vec![3]));
    assert_eq!(resolve(&f, &sel(&[], &["**/e01.mkv"])), Ok(vec![0, 2]));
    // Case-sensitive.
    assert!(resolve(&f, &sel(&[], &["readme.txt"])).is_err());
    assert_eq!(resolve(&f, &sel(&[], &["README.*"])), Ok(vec![4]));
    assert_eq!(resolve(&f, &sel(&[], &["s?/info.[t]xt"])), Ok(vec![1]));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn selection_validation() {
    let f = files(&["a.bin", "b.bin"]);
    let err = resolve(&f, &sel(&[], &[])).unwrap_err();
    assert!(err.contains("no files selected"), "{err}");
    let err = resolve(&f, &sel(&[0, 7], &[])).unwrap_err();
    assert_eq!(err, "file 8 doesn't exist — the torrent has 2 files");
    let err = resolve(&f, &sel(&[], &["*.iso"])).unwrap_err();
    assert!(
        err.contains("*.iso") && err.contains("matches no file"),
        "{err}"
    );
    let err = resolve(&f, &sel(&[], &["a[.bin"])).unwrap_err();
    assert!(
        err.contains("invalid pattern") && err.contains("a[.bin"),
        "{err}"
    );

    let seeder = Seeder::start(TREE).await;
    let env = Env::new(vec![seeder.addr]).await;
    // A `.torrent` with an invalid selection is refused up front, naming the culprit.
    for (choice, culprit) in [
        (select(&[99], &[]), "file 100 doesn't exist"),
        (select(&[], &["*.iso"]), "*.iso"),
        (select(&[], &[]), "no files selected"),
    ] {
        let err = env
            .add(1, env.torrent_req(&seeder, choice))
            .await
            .unwrap_err();
        assert!(
            matches!(&err, EngineError::Invalid(m) if m.contains(culprit)),
            "{err:?}"
        );
        assert!(env.engine.status(1).is_none(), "nothing registered");
        assert!(!env.path("data/torrents/1.torrent").exists());
    }
    assert!(!env.out().exists(), "nothing written");
    assert_eq!(env.engine.librqbit_torrents(), 0);

    // Every file selected: the same as no selection.
    let all: Vec<usize> = (0..TREE.len()).collect();
    let added = env
        .add(1, env.torrent_req(&seeder, select(&all, &[])))
        .await
        .unwrap();
    assert_eq!(
        added.total_bytes,
        Some(size_of(&TREE.iter().map(|f| f.0).collect::<Vec<_>>()))
    );
    assert!(!env.selection_file(1).exists());
    env.wait_state(1, DownloadState::Completed).await;
    seeder.assert_downloaded_into(&env.out());

    // Only valid while choosing files.
    let err = env
        .engine
        .select_files(1, sel(&[0], &[]))
        .await
        .unwrap_err();
    assert!(
        matches!(&err, EngineError::Invalid(m) if m == "not choosing files"),
        "{err:?}"
    );
    assert!(matches!(
        env.engine.select_files(9, sel(&[0], &[])).await,
        Err(EngineError::NotFound(9))
    ));
    env.engine.remove(1, true).await.unwrap();

    // Choosing: an empty or out-of-range selection is refused; it stays choosing.
    env.add(2, env.torrent_req(&seeder, FileChoice::Ask))
        .await
        .unwrap();
    for bad in [sel(&[], &[]), sel(&[TREE.len()], &[])] {
        let err = env.engine.select_files(2, bad).await.unwrap_err();
        assert!(matches!(err, EngineError::Invalid(_)), "{err:?}");
        assert_eq!(
            env.engine.status(2).unwrap().state,
            DownloadState::ChoosingFiles
        );
    }

    // Asking about a single-file torrent: nothing to choose, it just downloads.
    let single = Seeder::start(&[("one.iso", 2 * P)]).await;
    let env = Env::new(vec![single.addr]).await;
    env.add(3, env.torrent_req(&single, FileChoice::Ask))
        .await
        .unwrap();
    env.wait_state(3, DownloadState::Completed).await;
    single.assert_downloaded_into(&env.out());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn select_glob_across_folders() {
    let seeder = Seeder::start(TREE).await;
    let env = Env::new(vec![seeder.addr]).await;
    let mkv = ["s1/e01.mkv", "s2/e01.mkv", "extra/deep/x.mkv"];
    let added = env
        .add(1, env.torrent_req(&seeder, select(&[], &["*.mkv"])))
        .await
        .unwrap();
    assert_eq!(added.total_bytes, Some(size_of(&mkv)));
    // Full relative paths are listed.
    let listed: Vec<_> = env
        .engine
        .list_files(1)
        .await
        .unwrap()
        .into_iter()
        .map(|f| (f.path, f.size))
        .collect();
    for (rel, len) in TREE {
        assert!(
            listed.contains(&(rel.to_string(), *len as u64)),
            "{listed:?}"
        );
    }
    let st = env.wait_state(1, DownloadState::Completed).await;
    assert_eq!(
        (st.done_bytes, st.total_bytes),
        (size_of(&mkv), Some(size_of(&mkv)))
    );
    assert_only(&env.folder(), &mkv);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ask_waits_without_writing() {
    let seeder = Seeder::start(TREE).await;
    let env = Env::new(vec![seeder.addr]).await;

    // A `.torrent`: choosing at once; the session is not even started.
    let added = env
        .add(1, env.torrent_req(&seeder, FileChoice::Ask))
        .await
        .unwrap();
    assert_eq!(added.path, env.folder());
    let st = env.engine.status(1).unwrap();
    assert_eq!(st.state, DownloadState::ChoosingFiles);
    assert_eq!(st.name, MULTI_NAME);
    assert_eq!(env.engine.list_files(1).await.unwrap().len(), TREE.len());
    assert!(!env.engine.session_started());
    assert!(
        env.path("data/torrents/1.torrent").exists(),
        "stored copy written"
    );
    // Pausing changes nothing; resuming needs a choice first.
    env.engine.pause(1).await.unwrap();
    let err = env.engine.resume(1).await.unwrap_err();
    assert!(
        matches!(&err, EngineError::Invalid(m) if m.contains("choose files")),
        "{err:?}"
    );

    // A magnet: choosing once its metadata is known.
    env.engine.remove(1, true).await.unwrap();
    assert!(!env.path("data/torrents/1.torrent").exists());
    assert!(!env.selection_file(1).exists());
    env.add(2, env.magnet_req(&seeder, FileChoice::Ask))
        .await
        .unwrap();
    env.wait_state(2, DownloadState::ChoosingFiles).await;
    assert_eq!(env.engine.list_files(2).await.unwrap().len(), TREE.len());
    assert_eq!(env.engine.status(2).unwrap().name, MULTI_NAME);
    assert!(env.path("data/torrents/2.torrent").exists());

    // Nothing in librqbit, nothing on disk, for a while.
    tokio::time::sleep(Duration::from_millis(1200)).await;
    assert_eq!(
        env.engine.status(2).unwrap().state,
        DownloadState::ChoosingFiles
    );
    assert_eq!(env.engine.librqbit_torrents(), 0);
    assert!(!env.out().exists(), "something was written");

    // Removing it with its files deletes nothing (nothing was written).
    std::fs::create_dir_all(env.out()).unwrap();
    env.engine.remove(2, true).await.unwrap();
    assert!(env.out().is_dir());
    assert!(!env.path("data/torrents/2.torrent").exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn select_then_downloads_only_selected() {
    let seeder = Seeder::start(TREE).await;
    let env = Env::new(vec![seeder.addr]).await;
    env.add(1, env.magnet_req(&seeder, FileChoice::Ask))
        .await
        .unwrap();
    env.wait_state(1, DownloadState::ChoosingFiles).await;
    env.engine.set_download_limit(1, Some(10_000_000)).unwrap();
    let picked = env.indices_of(1, &["s1/e01.mkv", "readme.txt"]).await;
    assert_eq!(picked.len(), 2);
    env.engine.select_files(1, sel(&picked, &[])).await.unwrap();
    // Added paused: the caller starts it.
    assert_eq!(env.engine.librqbit_torrents(), 1);
    let want = ["s1/e01.mkv", "readme.txt"];
    let st = env.wait_state(1, DownloadState::Paused).await;
    assert_eq!(st.total_bytes, Some(size_of(&want)));
    assert!(env.selection_file(1).exists());
    // Once only.
    assert!(matches!(
        env.engine.select_files(1, sel(&[0], &[])).await,
        Err(EngineError::Invalid(_))
    ));
    env.engine.resume(1).await.unwrap();
    let st = env.wait_state(1, DownloadState::Completed).await;
    assert_eq!(
        (st.done_bytes, st.total_bytes),
        (size_of(&want), Some(size_of(&want)))
    );
    assert_eq!(st.path, env.out().join(&seeder.info_hash_hex));
    assert_only(&st.path, &want);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn magnet_invalid_preselection_goes_choosing() {
    let seeder = Seeder::start(TREE).await;
    let env = Env::new(vec![seeder.addr]).await;
    env.add(1, env.magnet_req(&seeder, select(&[], &["*.iso"])))
        .await
        .unwrap();
    env.wait_state(1, DownloadState::ChoosingFiles).await;
    let reason = env.engine.choosing_reason(1).expect("a reason");
    assert!(reason.contains("*.iso"), "{reason}");
    assert_eq!(env.engine.status(1).unwrap().note, Some(reason));
    assert_eq!(env.engine.librqbit_torrents(), 0);
    assert!(!env.out().exists());

    // A valid magnet pre-selection downloads without asking.
    env.engine
        .select_files(1, sel(&[], &["info.txt"]))
        .await
        .unwrap();
    env.engine.resume(1).await.unwrap();
    let st = env.wait_state(1, DownloadState::Completed).await;
    assert_only(&st.path, &["s1/info.txt", "s2/info.txt"]);

    let seeder2 = Seeder::start(&[("a/x.bin", P), ("a/y.bin", P), ("z.txt", P)]).await;
    let env = Env::new(vec![seeder2.addr]).await;
    env.add(2, env.magnet_req(&seeder2, select(&[], &["*.bin"])))
        .await
        .unwrap();
    let st = env.wait_state(2, DownloadState::Completed).await;
    assert_eq!(st.total_bytes, Some(2 * P as u64));
    assert!(st.path.join("a/x.bin").exists());
    assert_eq!(
        std::fs::metadata(st.path.join("z.txt")).map_or(0, |m| m.len()),
        0
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn delete_only_selected_files() {
    let seeder = Seeder::start(TREE).await;
    let mut env = Env::new(vec![seeder.addr]).await;
    let mkv = ["s1/e01.mkv", "s2/e01.mkv", "extra/deep/x.mkv"];

    // While held by the engine: the selected files and librqbit's placeholders go, and
    // the torrent's (now empty) folder; the output dir stays.
    std::fs::create_dir_all(env.out()).unwrap();
    std::fs::write(env.out().join("mine.txt"), b"user").unwrap();
    env.add(1, env.torrent_req(&seeder, select(&[], &["*.mkv"])))
        .await
        .unwrap();
    env.wait_state(1, DownloadState::Completed).await;
    env.engine.remove(1, true).await.unwrap();
    assert!(
        !env.folder().exists(),
        "left: {:?}",
        std::fs::read_dir(env.out()).unwrap().collect::<Vec<_>>()
    );
    assert_eq!(std::fs::read(env.out().join("mine.txt")).unwrap(), b"user");

    // An unselected file that was already there (not created by this download) is
    // never deleted: completed, dropped, then deleted from the stored metainfo.
    let req = env.torrent_req(&seeder, select(&[], &["*.mkv"]));
    env.add(2, req.clone()).await.unwrap();
    env.wait_state(2, DownloadState::Completed).await;
    let indices = env.indices_of(2, &mkv).await;
    env.restart().await;
    // The user replaced a placeholder with a file of their own; puppy's record says so
    // (nothing created by this download besides the selection).
    std::fs::write(env.folder().join("s1/info.txt"), b"user notes").unwrap();
    std::fs::write(
        env.selection_file(2),
        format!(r#"{{"kind":"selected","indices":{indices:?},"created":[]}}"#),
    )
    .unwrap();
    env.restore(2, req, &env.folder()).await;
    env.wait_state(2, DownloadState::Completed).await;
    env.engine.drop_completed(2).await.unwrap();
    env.engine.delete_completed(2, &env.folder()).await.unwrap();
    for rel in mkv {
        assert!(!env.folder().join(rel).exists(), "{rel} kept");
    }
    assert_eq!(
        std::fs::read(env.folder().join("s1/info.txt")).unwrap(),
        b"user notes"
    );
    assert!(
        !env.folder().join("extra").exists(),
        "emptied folder removed"
    );
    assert!(
        env.folder().join("s2/info.txt").exists(),
        "unlisted placeholder kept"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn restore_choosing_and_selected() {
    let seeder = Seeder::start(TREE).await;
    let mut env = Env::new(vec![seeder.addr]).await;
    // 1: choosing (a `.torrent`); 2: choosing (a magnet, after its metadata);
    // 3: selected, completed.
    let req1 = env.torrent_req(&seeder, FileChoice::Ask);
    let added1 = env.add(1, req1.clone()).await.unwrap();
    let seeder2 = Seeder::start(&[("a/x.bin", P), ("a/y.bin", 2 * P)]).await;
    let seeder3 = Seeder::start(&[("b/x.bin", P), ("b/y.bin", 2 * P)]).await;
    env.peers.push(seeder2.addr);
    env.peers.push(seeder3.addr);
    env.restart().await;
    let added1b = env.restore(1, req1.clone(), &added1.path).await;
    assert_eq!(added1b.path, added1.path);
    let req2 = env.magnet_req(&seeder2, FileChoice::Ask);
    let added2 = env.add(2, req2.clone()).await.unwrap();
    env.wait_state(2, DownloadState::ChoosingFiles).await;
    let req3 = env.torrent_req(&seeder3, select(&[], &["y.bin"]));
    let mut req3 = req3;
    req3.output_dir = Some(env.path("out3"));
    let added3 = env.add(3, req3.clone()).await.unwrap();
    env.wait_state(3, DownloadState::Completed).await;

    env.restart().await;
    let mut r1 = req1;
    r1.files = FileChoice::All; // the recorded choice wins over the request's
    env.restore(1, r1, &added1.path).await;
    env.restore(2, req2, &added2.path).await;
    env.restore(3, req3, &added3.path).await;
    for id in [1, 2] {
        assert_eq!(
            env.engine.status(id).unwrap().state,
            DownloadState::ChoosingFiles,
            "{id}"
        );
        assert!(!env.engine.list_files(id).await.unwrap().is_empty());
    }
    let st = env
        .wait(3, "verified", |s| s.done_bytes == 2 * P as u64)
        .await;
    assert_eq!(st.total_bytes, Some(2 * P as u64));
    env.engine.resume(3).await.unwrap();
    env.wait_state(3, DownloadState::Completed).await;
    assert_eq!(env.engine.librqbit_torrents(), 1, "choosing adds nothing");
    assert!(!env.out().exists());
    assert_eq!(
        std::fs::metadata(env.path("out3").join(MULTI_NAME).join("b/x.bin")).map_or(0, |m| m.len()),
        0
    );

    // Choosing after a restore works like before.
    let picked = env.indices_of(1, &["readme.txt"]).await;
    env.engine.select_files(1, sel(&picked, &[])).await.unwrap();
    env.engine.resume(1).await.unwrap();
    env.wait_state(1, DownloadState::Completed).await;
    assert_only(&env.folder(), &["readme.txt"]);

    // And the selection survives another restart.
    env.restart().await;
    env.restore(1, env.torrent_req(&seeder, FileChoice::All), &added1.path)
        .await;
    let st = env.wait(1, "verified", |s| s.done_bytes == P as u64).await;
    assert_eq!(st.total_bytes, Some(P as u64));
}

/// Not piece-aligned: pieces 1 and 2 are each shared by the selected `a/two.bin` and an
/// unselected file, so librqbit writes those bytes into the unselected files.
const SHARED: &[(&str, usize)] = &[
    ("a/one.bin", P + 100),
    ("a/two.bin", P + 300),
    ("b/three.bin", 2 * P + 7),
];

async fn download_two(env: &Env, seeder: &Seeder, id: DownloadId) -> DownloadStatus {
    env.add(id, env.torrent_req(seeder, select(&[], &["two.bin"])))
        .await
        .unwrap();
    let st = env.wait_state(id, DownloadState::Completed).await;
    let two = (P + 300) as u64;
    assert_eq!((st.done_bytes, st.total_bytes), (two, Some(two)));
    let got = std::fs::read(env.folder().join("a/two.bin")).unwrap();
    assert!(got == support::content("a/two.bin", P + 300));
    for (rel, len) in [SHARED[0], SHARED[2]] {
        let got = std::fs::read(env.folder().join(rel)).unwrap_or_default();
        assert!(
            got != support::content(rel, len),
            "unselected {rel} downloaded"
        );
    }
    // The poll task has seen the completion (and the placeholders).
    tokio::time::sleep(Duration::from_millis(1200)).await;
    st
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shared_pieces_selected_only_and_delete() {
    let seeder = Seeder::start(SHARED).await;
    let env = Env::new(vec![seeder.addr]).await;
    let st = download_two(&env, &seeder, 1).await;
    assert!(
        std::fs::metadata(env.folder().join("b/three.bin"))
            .unwrap()
            .len()
            > 0,
        "librqbit wrote the shared piece's bytes into the placeholder"
    );
    env.engine.remove(1, true).await.unwrap();
    assert!(!st.path.exists(), "placeholders and folders deleted");
    assert!(env.out().is_dir());
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn replaced_placeholder_is_kept() {
    let seeder = Seeder::start(SHARED).await;
    let env = Env::new(vec![seeder.addr]).await;
    download_two(&env, &seeder, 1).await;
    // The user replaces a placeholder with a file of their own, of the same length.
    let three = env.folder().join("b/three.bin");
    let len = std::fs::metadata(&three).unwrap().len() as usize;
    std::fs::remove_file(&three).unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    std::fs::write(&three, vec![b'u'; len]).unwrap();
    env.engine.remove(1, true).await.unwrap();
    assert_eq!(std::fs::read(&three).unwrap(), vec![b'u'; len]);
    assert!(!env.folder().join("a").exists(), "the rest is deleted");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn restored_choosing_never_overwrites() {
    let seeder = Seeder::start(TREE).await;
    let mut env = Env::new(vec![seeder.addr]).await;
    let req = env.torrent_req(&seeder, FileChoice::Ask);
    let added = env.add(1, req.clone()).await.unwrap();
    env.restart().await;
    env.restore(1, req, &added.path).await;
    assert_eq!(
        env.engine.status(1).unwrap().state,
        DownloadState::ChoosingFiles
    );
    // Something not puppy's at a selected path.
    let theirs = env.folder().join("readme.txt");
    std::fs::create_dir_all(env.folder()).unwrap();
    std::fs::write(&theirs, b"theirs").unwrap();
    let picked = env.indices_of(1, &["readme.txt", "s1/e01.mkv"]).await;
    let err = env
        .engine
        .select_files(1, sel(&picked, &[]))
        .await
        .unwrap_err();
    assert!(
        matches!(&err, EngineError::Invalid(m) if m.contains("already exists")),
        "{err:?}"
    );
    assert_eq!(std::fs::read(&theirs).unwrap(), b"theirs");
    let left: Vec<_> = walk(&env.folder());
    assert_eq!(
        left,
        vec![theirs.clone()],
        "the failed add left files behind"
    );
    // Still choosing (also after a restart).
    assert_eq!(
        env.engine.status(1).unwrap().state,
        DownloadState::ChoosingFiles
    );
    let sc = std::fs::read_to_string(env.selection_file(1)).unwrap();
    assert!(sc.contains("choosing"), "{sc}");
    std::fs::remove_file(&theirs).unwrap();
    env.engine.select_files(1, sel(&picked, &[])).await.unwrap();
    env.engine.resume(1).await.unwrap();
    env.wait_state(1, DownloadState::Completed).await;
    assert_only(&env.folder(), &["readme.txt", "s1/e01.mkv"]);
}

/// Every regular file under `dir`, sorted.
fn walk(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_owned()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).into_iter().flatten().flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else {
                out.push(p);
            }
        }
    }
    out.sort();
    out
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn magnet_files_unknown_before_metadata() {
    let seeder = Seeder::start(TREE).await;
    let env = Env::new(vec![seeder.addr]).await;
    env.engine
        .add(
            1,
            env.magnet_req(&seeder, FileChoice::Ask),
            StartMode::Fresh,
            AddOptions {
                start_paused: true,
                limit: None,
            },
        )
        .await
        .unwrap();
    let err = env.engine.list_files(1).await.unwrap_err();
    assert!(
        matches!(&err, EngineError::Invalid(m) if m.contains("not known yet")),
        "{err:?}"
    );
    assert!(matches!(
        env.engine.list_files(7).await,
        Err(EngineError::NotFound(7))
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn limit_set_while_choosing_reaches_the_real_add() {
    let files: &[(&str, usize)] = &[("a/big.bin", 24 * P), ("a/small.bin", P)];
    let seeder = Seeder::start(files).await;
    let env = Env::new(vec![seeder.addr]).await;
    env.add(1, env.torrent_req(&seeder, FileChoice::Ask))
        .await
        .unwrap();
    // 384 KiB at 96 KiB/s: about 3-4 s (unlimited, locally: well under a second).
    env.engine.set_download_limit(1, Some(96 * 1024)).unwrap();
    let big = env.indices_of(1, &["big.bin"]).await;
    env.engine.select_files(1, sel(&big, &[])).await.unwrap();
    let started = Instant::now();
    env.engine.resume(1).await.unwrap();
    env.wait_state(1, DownloadState::Completed).await;
    let took = started.elapsed();
    assert!(took >= Duration::from_millis(2000), "not limited: {took:?}");
}
