//! Torrents through the whole daemon: `run_daemon` in-process (its torrent engine
//! network-free via `DaemonOptions::torrent_test`), driven by the client, downloading
//! from a local seeder on 127.0.0.1.

// The torrent engine's local seeder harness (librqbit is a dev-dependency here only).
#[path = "../../puppy-torrent/tests/support/mod.rs"]
mod support;

use puppy_client::Client;
use puppy_client::ClientError;
use puppy_core::config::HttpConfig;
use puppy_core::types::{
    AddRequest, DownloadId, FileChoice, RequestContext, SeedPolicy, Selection, Source,
};
use puppy_daemon::lifecycle::{
    DaemonOptions, RunOutcome, TorrentProbe, TorrentTestOptions, run_daemon,
};
use puppy_daemon::{Paths, SocketName};
use puppy_proto::{
    DownloadSnapshot, EngineKindTag, ErrorCode, Event, ReplyData, Request, WireAttention, WireState,
};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};
use support::{MULTI_NAME, Seeder};
use tokio::task::JoinHandle;

const TIMEOUT: Duration = Duration::from_secs(20);
const KIB: usize = 1024;
/// A multi-file torrent of ~1 MiB.
const MULTI: &[(&str, usize)] = &[("a.bin", 600 * KIB), ("sub/b.bin", 400 * KIB + 123)];
/// Seeder upload limit: ~1 MiB takes ~4 s, so the download can be watched mid-way.
const SLOW: u32 = 256 * KIB as u32;

type Daemon = JoinHandle<anyhow::Result<RunOutcome>>;

/// A short, unique socket name (macOS `sun_path` is ~104 bytes).
fn socket_name() -> SocketName {
    static N: AtomicUsize = AtomicUsize::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    if cfg!(windows) {
        SocketName::Pipe(format!("puppy-tt-{pid}-{n}"))
    } else {
        SocketName::Path(PathBuf::from(format!("/tmp/pz-{pid}-t{n}.sock")))
    }
}

struct Env {
    home: tempfile::TempDir,
    socket: SocketName,
    peers: Vec<SocketAddr>,
    probe: TorrentProbe,
}

impl Env {
    fn new(seeder: &Seeder) -> Self {
        Self::with_seeders(&[seeder])
    }

    fn with_seeders(seeders: &[&Seeder]) -> Self {
        Env {
            home: tempfile::tempdir().unwrap(),
            socket: socket_name(),
            peers: seeders.iter().map(|s| s.addr).collect(),
            probe: TorrentProbe::default(),
        }
    }

    /// Writes the daemon's `config.toml` (read when it starts).
    fn write_config(&self, text: &str) {
        let dir = self.home.path().join("config");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("config.toml"), text).unwrap();
    }

    fn state_json(&self) -> serde_json::Value {
        let text = std::fs::read_to_string(self.home.path().join("data/state.json")).unwrap();
        serde_json::from_str(&text).unwrap()
    }

    /// The files puppy keeps for torrent `id` in its data dir.
    fn stored_files(&self, id: DownloadId) -> Vec<PathBuf> {
        let dir = self.home.path().join("data/torrents");
        let Ok(rd) = std::fs::read_dir(&dir) else {
            return Vec::new();
        };
        rd.map(|e| e.unwrap().path())
            .filter(|p| {
                p.file_stem()
                    .is_some_and(|s| s.to_string_lossy() == id.to_string())
            })
            .collect()
    }

    fn dl_dir(&self) -> PathBuf {
        self.home.path().join("dl")
    }

    fn opts(&self) -> DaemonOptions {
        DaemonOptions {
            idle_exit: None,
            paths: Some(Paths::with_home(self.home.path(), self.socket.clone()).unwrap()),
            http: Some(HttpConfig {
                default_dir: self.dl_dir(),
                ..HttpConfig::default()
            }),
            engine: None,
            torrent_test: Some(TorrentTestOptions {
                initial_peers: self.peers.clone(),
                probe: self.probe.clone(),
            }),
        }
    }

    /// Starts a daemon and returns it with a connected client.
    async fn start(&self) -> (Daemon, Client) {
        let d = tokio::spawn(run_daemon(self.opts()));
        let deadline = Instant::now() + TIMEOUT;
        loop {
            assert!(!d.is_finished(), "daemon exited early: {:?}", d.await);
            if let Ok(c) = Client::connect(&self.socket, "test").await {
                return (d, c);
            }
            assert!(Instant::now() < deadline, "daemon never listened");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    async fn shutdown(&self, d: Daemon, c: Client) {
        assert_eq!(c.call(Request::Shutdown).await.unwrap(), ReplyData::Done);
        drop(c);
        let started = Instant::now();
        let r = tokio::time::timeout(TIMEOUT, d)
            .await
            .expect("daemon did not exit")
            .unwrap()
            .unwrap();
        assert_eq!(r, RunOutcome::Exited);
        assert!(
            started.elapsed() < Duration::from_secs(12),
            "shutdown took {:?}",
            started.elapsed()
        );
    }

    /// Writes the seeder's `.torrent` into the home dir and returns a request for it.
    fn torrent_req(&self, seeder: &Seeder) -> AddRequest {
        self.torrent_req_into(seeder, self.dl_dir())
    }

    /// Like `torrent_req`, downloading into `dir`.
    fn torrent_req_into(&self, seeder: &Seeder, dir: PathBuf) -> AddRequest {
        let path = self
            .home
            .path()
            .join(format!("{}.torrent", seeder.info_hash_hex));
        std::fs::write(&path, &seeder.torrent_bytes).unwrap();
        AddRequest {
            source: Source::TorrentFile { path },
            output_dir: Some(dir),
            filename: None,
            context: RequestContext::default(),
            files: Default::default(),
            seed: None,
        }
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        if let SocketName::Path(p) = &self.socket {
            let _ = std::fs::remove_file(p);
        }
    }
}

/// The data is complete: completed, or seeding.
fn is_done(s: &DownloadSnapshot) -> bool {
    matches!(s.state, WireState::Completed | WireState::Seeding { .. })
}

fn is_seeding(s: &DownloadSnapshot) -> bool {
    matches!(s.state, WireState::Seeding { .. })
}

/// The error code a request fails with.
async fn call_err(c: &Client, req: Request) -> ErrorCode {
    match c.call(req.clone()).await {
        Err(ClientError::Daemon { code, .. }) => code,
        other => panic!("{req:?} did not fail: {other:?}"),
    }
}

async fn call_ok(c: &Client, req: Request) {
    assert_eq!(c.call(req).await.unwrap(), ReplyData::Done);
}

async fn snapshot(c: &Client, id: DownloadId) -> DownloadSnapshot {
    c.list()
        .await
        .unwrap()
        .into_iter()
        .find(|d| d.id == id)
        .expect("download listed")
}

/// Polls the list until `pred` holds for `id`; returns every snapshot seen.
async fn watch(
    c: &Client,
    id: DownloadId,
    what: &str,
    pred: impl Fn(&DownloadSnapshot) -> bool,
) -> Vec<DownloadSnapshot> {
    let deadline = Instant::now() + TIMEOUT;
    let mut seen = Vec::new();
    loop {
        let d = snapshot(c, id).await;
        let done = pred(&d);
        seen.push(d);
        if done {
            return seen;
        }
        if let WireState::Failed { reason, .. } = &seen.last().unwrap().state {
            panic!("download failed: {reason}");
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {what}: {:?}",
            seen.last()
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn assert_files(dir: &Path) {
    for (rel, len) in MULTI {
        let got = std::fs::read(dir.join(rel)).unwrap_or_else(|e| panic!("reading {rel}: {e}"));
        assert!(got == support::content(rel, *len), "{rel} differs");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn daemon_downloads_local_torrent() {
    let seeder = Seeder::start_with_limit(MULTI, SLOW).await;
    let env = Env::new(&seeder);
    let (d, c) = env.start().await;

    let id = c.add(env.torrent_req(&seeder), false, None).await.unwrap();
    let seen = watch(&c, id, "completion", is_done).await;
    let done = seen.last().unwrap();
    assert_eq!(done.kind, EngineKindTag::Torrent);
    assert_eq!(done.name, MULTI_NAME);
    assert_eq!(done.path.as_deref(), Some(&*env.dl_dir().join(MULTI_NAME)));
    let total = MULTI.iter().map(|f| f.1 as u64).sum::<u64>();
    assert_eq!((done.done_bytes, done.total_bytes), (total, Some(total)));
    assert!(
        seen.iter()
            .any(|s| s.state == WireState::Active && s.peers.unwrap_or(0) >= 1),
        "peers reported while downloading: {seen:?}"
    );
    assert_files(&env.dl_dir().join(MULTI_NAME));

    env.shutdown(d, c).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn daemon_restart_restores_torrent() {
    let seeder = Seeder::start_with_limit(MULTI, SLOW).await;
    let env = Env::new(&seeder);
    let (d, c) = env.start().await;
    let id = c.add(env.torrent_req(&seeder), false, None).await.unwrap();
    watch(&c, id, "transferring", |s| {
        s.state == WireState::Active && s.done_bytes > 0
    })
    .await;
    env.shutdown(d, c).await;

    // Restarted: the torrent is restored (re-verified) and goes on to completion.
    let (d, c) = env.start().await;
    let list = c.list().await.unwrap();
    assert_eq!(list.len(), 1, "no duplicate: {list:?}");
    assert_ne!(list[0].state, WireState::Completed, "stopped mid-way");
    let seen = watch(&c, id, "completion", is_done).await;
    assert_eq!(seen.last().unwrap().kind, EngineKindTag::Torrent);
    assert_files(&env.dl_dir().join(MULTI_NAME));
    env.shutdown(d, c).await;
}

const USER_DATA: &[u8] = b"the user's own data";

async fn remove(c: &Client, id: DownloadId, delete_files: bool) {
    assert_eq!(
        c.call(Request::Remove { id, delete_files }).await.unwrap(),
        ReplyData::Done
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn daemon_fresh_add_never_overwrites_user_files() {
    let seeder = Seeder::start(MULTI).await;
    let env = Env::new(&seeder);
    let mine = env.dl_dir().join(MULTI_NAME);
    std::fs::create_dir_all(mine.join("sub")).unwrap();
    for (rel, _) in MULTI {
        std::fs::write(mine.join(rel), USER_DATA).unwrap();
    }
    let (d, c) = env.start().await;

    let id = c.add(env.torrent_req(&seeder), false, None).await.unwrap();
    let seen = watch(&c, id, "completion", is_done).await;
    let elsewhere = env.dl_dir().join(format!("{MULTI_NAME} (1)"));
    assert_eq!(seen.last().unwrap().path.as_deref(), Some(&*elsewhere));
    assert_files(&elsewhere);
    remove(&c, id, true).await;
    for (rel, _) in MULTI {
        assert_eq!(std::fs::read(mine.join(rel)).unwrap(), USER_DATA, "{rel}");
        assert!(!elsewhere.join(rel).exists(), "{rel} of the torrent left");
    }
    env.shutdown(d, c).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn daemon_remove_completed_torrent_deletes_its_files() {
    let seeder = Seeder::start(MULTI).await;
    let env = Env::new(&seeder);
    std::fs::create_dir_all(env.dl_dir()).unwrap();
    std::fs::write(env.dl_dir().join("unrelated.txt"), USER_DATA).unwrap();
    let (d, c) = env.start().await;

    let id = c.add(env.torrent_req(&seeder), false, None).await.unwrap();
    watch(&c, id, "completion", is_done).await;
    let folder = env.dl_dir().join(MULTI_NAME);
    assert_files(&folder);
    // The manager drops completed downloads from the engine: wait for that.
    tokio::time::sleep(Duration::from_millis(1500)).await;
    remove(&c, id, true).await;
    for (rel, _) in MULTI {
        assert!(!folder.join(rel).exists(), "{rel} not deleted");
    }
    assert!(!folder.exists(), "the torrent's (now empty) folder is left");
    assert!(env.dl_dir().is_dir(), "the download dir itself is kept");
    assert_eq!(
        std::fs::read(env.dl_dir().join("unrelated.txt")).unwrap(),
        USER_DATA
    );
    let stored = env
        .home
        .path()
        .join("data/torrents")
        .join(format!("{id}.torrent"));
    assert!(!stored.exists(), "stored .torrent copy left after removal");
    env.shutdown(d, c).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn daemon_restart_restores_magnet_offline_with_its_name() {
    let seeder = Seeder::start_with_limit(MULTI, SLOW).await;
    let env = Env::new(&seeder);
    let (d, c) = env.start().await;
    let req = AddRequest {
        source: Source::Magnet {
            uri: format!("{}&dn=placeholder", seeder.magnet),
        },
        output_dir: Some(env.dl_dir()),
        filename: None,
        context: RequestContext::default(),
        files: Default::default(),
        seed: None,
    };
    let id = c.add(req, false, None).await.unwrap();
    let seen = watch(&c, id, "transferring", |s| {
        s.state == WireState::Active && s.done_bytes > 0
    })
    .await;
    assert_eq!(seen.last().unwrap().name, MULTI_NAME);
    env.shutdown(d, c).await;
    drop(seeder); // offline from now on

    let (d, c) = env.start().await;
    let seen = watch(&c, id, "verified data", |s| s.done_bytes > 0).await;
    assert!(
        seen.iter().all(|s| s.name == MULTI_NAME),
        "the placeholder name came back: {seen:?}"
    );
    assert_ne!(seen.last().unwrap().state, WireState::Completed);
    env.shutdown(d, c).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn http_only_daemon_never_starts_the_torrent_session() {
    let server = puppy_testserver::TestServer::start(puppy_testserver::ServerCfg {
        len: 64 * KIB as u64,
        ..Default::default()
    })
    .await;
    let env = Env {
        home: tempfile::tempdir().unwrap(),
        socket: socket_name(),
        peers: Vec::new(),
        probe: TorrentProbe::default(),
    };
    let (d, c) = env.start().await;
    let req = AddRequest {
        source: Source::Http {
            url: server.url("/file.bin"),
        },
        output_dir: Some(env.dl_dir()),
        filename: Some("f.bin".into()),
        context: RequestContext::default(),
        files: Default::default(),
        seed: None,
    };
    let id = c.add(req, false, None).await.unwrap();
    watch(&c, id, "completion", |s| s.state == WireState::Completed).await;
    assert_eq!(
        env.probe.session_started(),
        Some(false),
        "an HTTP-only daemon opened torrent sockets"
    );
    env.shutdown(d, c).await;
}

// ---- File selection and seeding through the daemon ----

/// Other multi-file torrents (different info hashes from `MULTI`).
const MULTI_B: &[(&str, usize)] = &[("x/c.bin", 200 * KIB), ("d.bin", 100 * KIB + 7)];
const MULTI_C: &[(&str, usize)] = &[("e.bin", 500 * KIB), ("f/g.bin", 500 * KIB + 3)];
/// Seeding stops after a second; the ratio trigger is off.
const SEED_1S: &str = "[torrent]\nseed_ratio = 0\nseed_time = \"1s\"\n";

fn total(files: &[(&str, usize)]) -> u64 {
    files.iter().map(|f| f.1 as u64).sum()
}

/// Collects the discrete events (no snapshots) of a second, subscribed connection.
struct Events {
    seen: std::sync::Arc<std::sync::Mutex<Vec<Event>>>,
    _task: tokio::task::JoinHandle<()>,
}

impl Events {
    async fn start(env: &Env) -> Self {
        let mut c = Client::connect(&env.socket, "events").await.unwrap();
        c.subscribe(1000).await.unwrap();
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let task = tokio::spawn({
            let seen = seen.clone();
            async move {
                while let Some(ev) = c.next_event().await {
                    if !matches!(ev, Event::Snapshot { .. }) {
                        seen.lock().unwrap().push(ev);
                    }
                }
            }
        });
        Events { seen, _task: task }
    }

    fn of(&self, id: DownloadId) -> Vec<Event> {
        let ids = |e: &Event| match e {
            Event::Added { id }
            | Event::Completed { id, .. }
            | Event::Failed { id, .. }
            | Event::Removed { id }
            | Event::NeedsAttention { id, .. }
            | Event::SeedingFinished { id } => Some(*id),
            _ => None,
        };
        self.seen
            .lock()
            .unwrap()
            .iter()
            .filter(|e| ids(e) == Some(id))
            .cloned()
            .collect()
    }

    async fn wait_for(&self, what: &str, pred: impl Fn(&Event) -> bool) {
        let deadline = Instant::now() + TIMEOUT;
        while !self.seen.lock().unwrap().iter().any(&pred) {
            assert!(Instant::now() < deadline, "no {what} event");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
}

fn completed_count(evs: &[Event]) -> usize {
    evs.iter()
        .filter(|e| matches!(e, Event::Completed { .. }))
        .count()
}

fn finished_count(evs: &[Event]) -> usize {
    evs.iter()
        .filter(|e| matches!(e, Event::SeedingFinished { .. }))
        .count()
}

fn ask(mut req: AddRequest) -> AddRequest {
    req.files = FileChoice::Ask;
    req
}

async fn list_files(c: &Client, id: DownloadId) -> Vec<puppy_core::types::TorrentFile> {
    match c.call(Request::ListFiles { id }).await.unwrap() {
        ReplyData::Files { files } => files,
        other => panic!("not files: {other:?}"),
    }
}

fn select(id: DownloadId, indices: &[usize]) -> Request {
    Request::SelectFiles {
        id,
        selection: Selection {
            indices: indices.to_vec(),
            globs: Vec::new(),
        },
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn choose_then_download_via_daemon() {
    let seeder = Seeder::start(MULTI).await;
    let env = Env::new(&seeder);
    let (d, c) = env.start().await;

    let id = c
        .add(ask(env.torrent_req(&seeder)), false, None)
        .await
        .unwrap();
    let seen = watch(&c, id, "choosing files", |s| {
        s.state == WireState::ChoosingFiles
    })
    .await;
    assert_eq!(seen.last().unwrap().total_bytes, Some(total(MULTI)));
    assert!(
        !env.dl_dir().join(MULTI_NAME).exists(),
        "nothing is written while choosing"
    );
    let files = list_files(&c, id).await;
    let mut listed: Vec<_> = files.iter().map(|f| (f.path.as_str(), f.size)).collect();
    listed.sort();
    assert_eq!(
        listed,
        vec![
            ("a.bin", MULTI[0].1 as u64),
            ("sub/b.bin", MULTI[1].1 as u64)
        ]
    );
    assert!(files.iter().enumerate().all(|(i, f)| f.index == i));
    let (ia, ib) = (
        files.iter().position(|f| f.path == "a.bin").unwrap(),
        files.iter().position(|f| f.path == "sub/b.bin").unwrap(),
    );
    assert_eq!(
        call_err(&c, Request::Resume { id }).await,
        ErrorCode::InvalidState
    );
    assert_eq!(call_err(&c, select(id, &[])).await, ErrorCode::BadRequest);
    assert_eq!(call_err(&c, select(id, &[7])).await, ErrorCode::BadRequest);
    // Still choosing after the bad choices.
    assert_eq!(snapshot(&c, id).await.state, WireState::ChoosingFiles);

    call_ok(&c, select(id, &[ib])).await;
    assert_ne!(snapshot(&c, id).await.state, WireState::ChoosingFiles);
    let seen = watch(&c, id, "completion", is_done).await;
    let done = seen.last().unwrap();
    let b = MULTI[1].1 as u64;
    assert_eq!((done.done_bytes, done.total_bytes), (b, Some(b)));
    let folder = env.dl_dir().join(MULTI_NAME);
    assert!(
        std::fs::read(folder.join("sub/b.bin")).unwrap()
            == support::content("sub/b.bin", MULTI[1].1),
        "the selected file differs"
    );
    let a_len = std::fs::metadata(folder.join("a.bin")).map_or(0, |m| m.len());
    assert!(
        a_len < MULTI[0].1 as u64,
        "the unselected file was downloaded"
    );
    // The choice is made once.
    assert_eq!(
        call_err(&c, select(id, &[ia])).await,
        ErrorCode::InvalidState
    );
    env.shutdown(d, c).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn file_requests_on_http_downloads_are_unsupported() {
    let server = puppy_testserver::TestServer::start(puppy_testserver::ServerCfg {
        len: 16 * KIB as u64,
        ..Default::default()
    })
    .await;
    let env = Env::with_seeders(&[]);
    let (d, c) = env.start().await;
    let req = AddRequest {
        source: Source::Http {
            url: server.url("/file.bin"),
        },
        output_dir: Some(env.dl_dir()),
        filename: Some("f.bin".into()),
        context: RequestContext::default(),
        files: Default::default(),
        seed: None,
    };
    let id = c.add(req, false, None).await.unwrap();
    watch(&c, id, "completion", |s| s.state == WireState::Completed).await;
    for req in [
        Request::ListFiles { id },
        select(id, &[0]),
        Request::SetSeed {
            id,
            policy: SeedPolicy::default(),
        },
    ] {
        assert_eq!(call_err(&c, req).await, ErrorCode::Unsupported);
    }
    assert_eq!(
        call_err(&c, Request::ListFiles { id: 99 }).await,
        ErrorCode::NotFound
    );
    env.shutdown(d, c).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn seeding_then_completed_via_daemon() {
    let seeder = Seeder::start(MULTI).await;
    let env = Env::new(&seeder);
    env.write_config(SEED_1S);
    let (d, c) = env.start().await;
    let events = Events::start(&env).await;

    let id = c.add(env.torrent_req(&seeder), false, None).await.unwrap();
    let seen = watch(&c, id, "seeding", is_seeding).await;
    let s = seen.last().unwrap();
    assert_eq!(
        (s.done_bytes, s.total_bytes),
        (total(MULTI), Some(total(MULTI)))
    );
    let WireState::Seeding {
        ratio_target,
        time_target_secs,
        ..
    } = &s.state
    else {
        unreachable!()
    };
    assert_eq!((*ratio_target, *time_target_secs), (None, Some(1)));
    // The Completed event comes with the data, not when seeding stops.
    events
        .wait_for(
            "completed",
            |e| matches!(e, Event::Completed { id: i, .. } if *i == id),
        )
        .await;
    assert!(is_seeding(&snapshot(&c, id).await) || finished_count(&events.of(id)) > 0);

    let seen = watch(&c, id, "seeding stopped", |s| {
        s.state == WireState::Completed
    })
    .await;
    let s = seen.last().unwrap();
    assert_eq!(s.done_bytes, total(MULTI));
    events
        .wait_for(
            "seeding finished",
            |e| matches!(e, Event::SeedingFinished { id: i } if *i == id),
        )
        .await;
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let evs = events.of(id);
    assert_eq!(
        (completed_count(&evs), finished_count(&evs)),
        (1, 1),
        "{evs:?}"
    );
    let completed_at = evs
        .iter()
        .position(|e| matches!(e, Event::Completed { .. }));
    let finished_at = evs
        .iter()
        .position(|e| matches!(e, Event::SeedingFinished { .. }));
    assert!(completed_at < finished_at, "{evs:?}");
    assert_files(&env.dl_dir().join(MULTI_NAME));
    env.shutdown(d, c).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pause_remove_in_new_states() {
    let seeder = Seeder::start(MULTI).await;
    let env = Env::new(&seeder);
    let (d, c) = env.start().await;
    let events = Events::start(&env).await;
    let folder = env.dl_dir().join(MULTI_NAME);

    // Choosing files: pause changes nothing; remove with files deletes nothing of the
    // download dir (nothing was written) and cleans the stored copies.
    let id = c
        .add(ask(env.torrent_req(&seeder)), false, None)
        .await
        .unwrap();
    watch(&c, id, "choosing files", |s| {
        s.state == WireState::ChoosingFiles
    })
    .await;
    assert!(
        !env.stored_files(id).is_empty(),
        "the stored copy is written"
    );
    call_ok(&c, Request::Pause { id }).await;
    assert_eq!(snapshot(&c, id).await.state, WireState::ChoosingFiles);
    remove(&c, id, true).await;
    assert!(c.list().await.unwrap().iter().all(|s| s.id != id));
    assert!(!folder.exists());
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(env.stored_files(id), Vec::<PathBuf>::new());

    // Seeding: pause ⇒ completed (seeding stops).
    let id = c.add(env.torrent_req(&seeder), false, None).await.unwrap();
    watch(&c, id, "seeding", is_seeding).await;
    call_ok(&c, Request::Pause { id }).await;
    watch(&c, id, "completed", |s| s.state == WireState::Completed).await;
    events
        .wait_for(
            "seeding finished",
            |e| matches!(e, Event::SeedingFinished { id: i } if *i == id),
        )
        .await;
    assert_eq!(
        call_err(&c, Request::Resume { id }).await,
        ErrorCode::InvalidState
    );
    assert_files(&folder);
    tokio::time::sleep(Duration::from_millis(1500)).await;
    remove(&c, id, true).await;
    assert!(!folder.exists(), "files of the completed torrent left");

    // Seeding: remove with files deletes them.
    let id = c.add(env.torrent_req(&seeder), false, None).await.unwrap();
    watch(&c, id, "seeding", is_seeding).await;
    assert_files(&folder);
    remove(&c, id, true).await;
    assert!(!folder.exists(), "files of the seeding torrent left");
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(env.stored_files(id), Vec::<PathBuf>::new());
    env.shutdown(d, c).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn slots_ignore_choosing_and_seeding() {
    let a = Seeder::start(MULTI).await;
    let b = Seeder::start(MULTI_B).await;
    let cs = Seeder::start_with_limit(MULTI_C, SLOW).await;
    let env = Env::with_seeders(&[&a, &b, &cs]);
    let (d, c) = env.start().await;
    call_ok(&c, Request::SetMaxActive { n: 1 }).await;

    let ida = c
        .add(
            env.torrent_req_into(&a, env.dl_dir().join("a")),
            false,
            None,
        )
        .await
        .unwrap();
    watch(&c, ida, "seeding", is_seeding).await;
    // A magnet takes a slot to fetch its metadata, then gives it up to choose files.
    let req = AddRequest {
        source: Source::Magnet {
            uri: b.magnet.clone(),
        },
        output_dir: Some(env.dl_dir().join("b")),
        filename: None,
        context: RequestContext::default(),
        files: FileChoice::Ask,
        seed: None,
    };
    let idb = c.add(req, false, None).await.unwrap();
    watch(&c, idb, "choosing files", |s| {
        s.state == WireState::ChoosingFiles
    })
    .await;
    let idc = c
        .add(
            env.torrent_req_into(&cs, env.dl_dir().join("c")),
            false,
            None,
        )
        .await
        .unwrap();
    watch(&c, idc, "transferring", |s| {
        s.state == WireState::Active && s.done_bytes > 0
    })
    .await;
    assert!(is_seeding(&snapshot(&c, ida).await));
    assert_eq!(snapshot(&c, idb).await.state, WireState::ChoosingFiles);
    // The choice queues it at the front; it waits for the slot.
    call_ok(&c, select(idb, &[0])).await;
    let sb = snapshot(&c, idb).await;
    assert!(
        matches!(sb.state, WireState::Queued),
        "chosen while the slot is busy: {sb:?}"
    );
    assert_eq!(sb.queue_position, Some(0));
    call_ok(&c, Request::Pause { id: idc }).await;
    watch(&c, idb, "chosen torrent done", is_done).await;
    env.shutdown(d, c).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn restart_keeps_new_states() {
    let a = Seeder::start(MULTI).await;
    let b = Seeder::start(MULTI_B).await;
    let env = Env::with_seeders(&[&a, &b]);
    let (d, c) = env.start().await;
    let ida = c
        .add(
            env.torrent_req_into(&a, env.dl_dir().join("a")),
            false,
            None,
        )
        .await
        .unwrap();
    watch(&c, ida, "seeding", is_seeding).await;
    let idb = c
        .add(
            ask(env.torrent_req_into(&b, env.dl_dir().join("b"))),
            false,
            None,
        )
        .await
        .unwrap();
    watch(&c, idb, "choosing files", |s| {
        s.state == WireState::ChoosingFiles
    })
    .await;
    let policy = SeedPolicy {
        ratio: Some(5.0),
        time_secs: None,
    };
    call_ok(&c, Request::SetSeed { id: ida, policy }).await;
    env.shutdown(d, c).await;

    let state = env.state_json();
    let stored = |id: DownloadId| {
        state["downloads"]
            .as_array()
            .unwrap()
            .iter()
            .find(|d| d["id"] == id)
            .unwrap()
            .clone()
    };
    assert_eq!(stored(ida)["state"]["kind"], "seeding");
    assert_eq!(stored(idb)["state"]["kind"], "choosing_files");
    assert_eq!(stored(ida)["request"]["seed"]["ratio"], 5.0);

    let (d, c) = env.start().await;
    let events = Events::start(&env).await;
    // Seeding again at once (restored live), with the policy set before the restart.
    assert!(is_seeding(&snapshot(&c, ida).await));
    assert_eq!(snapshot(&c, idb).await.state, WireState::ChoosingFiles);
    let seen = watch(&c, ida, "restored seeding", |s| {
        matches!(
            s.state,
            WireState::Seeding {
                ratio_target: Some(r),
                ..
            } if r == 5.0
        )
    })
    .await;
    assert_eq!(seen.last().unwrap().done_bytes, total(MULTI));
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert!(is_seeding(&snapshot(&c, ida).await), "seeding goes on");
    assert_eq!(snapshot(&c, idb).await.state, WireState::ChoosingFiles);
    assert_eq!(list_files(&c, idb).await.len(), 2);
    call_ok(&c, select(idb, &[1])).await;
    watch(&c, idb, "chosen torrent done", is_done).await;
    // Policies still reach the restored torrent.
    call_ok(
        &c,
        Request::SetSeed {
            id: ida,
            policy: SeedPolicy {
                ratio: None,
                time_secs: Some(1),
            },
        },
    )
    .await;
    watch(&c, ida, "seeding stopped", |s| {
        s.state == WireState::Completed
    })
    .await;
    events
        .wait_for(
            "seeding finished",
            |e| matches!(e, Event::SeedingFinished { id } if *id == ida),
        )
        .await;
    assert_eq!(completed_count(&events.of(ida)), 0, "no second Completed");
    env.shutdown(d, c).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn failed_recheck_resumes_through_a_slot() {
    let a = Seeder::start(MULTI).await;
    let cs = Seeder::start_with_limit(MULTI_C, SLOW / 2).await;
    let env = Env::with_seeders(&[&a, &cs]);
    let (d, c) = env.start().await;
    call_ok(&c, Request::SetMaxActive { n: 1 }).await;
    let ida = c
        .add(
            env.torrent_req_into(&a, env.dl_dir().join("a")),
            false,
            None,
        )
        .await
        .unwrap();
    watch(&c, ida, "seeding", is_seeding).await;
    env.shutdown(d, c).await;
    // The data changes on disk while the daemon is down, and the seeder is gone.
    let file = env.dl_dir().join("a").join(MULTI_NAME).join("a.bin");
    let mut bytes = std::fs::read(&file).unwrap();
    bytes[..4096].fill(0);
    std::fs::write(&file, bytes).unwrap();
    drop(a);

    let (d, c) = env.start().await;
    watch(&c, ida, "needs attention", |s| {
        matches!(
            &s.state,
            WireState::NeedsAttention {
                reason: WireAttention::ValidationFailed { .. }
            }
        )
    })
    .await;
    let idc = c
        .add(
            env.torrent_req_into(&cs, env.dl_dir().join("c")),
            false,
            None,
        )
        .await
        .unwrap();
    watch(&c, idc, "transferring", |s| {
        s.state == WireState::Active && s.done_bytes > 0
    })
    .await;
    // Resumed like any download: it waits for the slot.
    call_ok(&c, Request::Resume { id: ida }).await;
    assert_eq!(snapshot(&c, ida).await.state, WireState::Queued);
    call_ok(&c, Request::Pause { id: idc }).await;
    watch(&c, ida, "downloading again", |s| {
        matches!(s.state, WireState::Starting | WireState::Active)
    })
    .await;
    env.shutdown(d, c).await;
}
