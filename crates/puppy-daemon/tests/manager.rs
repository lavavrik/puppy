use puppy_core::config::HttpConfig;
use puppy_core::engine::Engine;
use puppy_core::http::engine::HttpEngine;
use puppy_core::types::{AddRequest, DownloadId, DownloadState, RequestContext, Source};
use puppy_daemon::manager::{Manager, ManagerError};
use puppy_daemon::{Config, Store};
use puppy_proto::{DownloadSnapshot, ErrorCode, Event};
use puppy_testserver::{ServerCfg, TestServer, body_byte};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::broadcast;

const KIB: u64 = 1024;
const TIMEOUT: Duration = Duration::from_secs(30);

fn http_cfg(dir: &Path) -> HttpConfig {
    HttpConfig {
        backoff_base_ms: 10,
        backoff_max_ms: 100,
        flush_interval_ms: 100,
        read_timeout_ms: 2_000,
        validate_timeout_ms: 1_500,
        default_dir: dir.to_path_buf(),
        ..HttpConfig::default()
    }
}

struct Env {
    dir: tempfile::TempDir,
}

impl Env {
    fn new() -> Self {
        Env {
            dir: tempfile::tempdir().unwrap(),
        }
    }

    fn dl_dir(&self) -> PathBuf {
        self.dir.path().join("dl")
    }

    fn state_path(&self) -> PathBuf {
        self.dir.path().join("state.json")
    }

    async fn manager(&self, max_active: u16) -> Arc<Manager> {
        let engine: Arc<dyn Engine> = Arc::new(HttpEngine::new(http_cfg(&self.dl_dir())).unwrap());
        let cfg = Config {
            download_dir: self.dl_dir(),
            max_active,
            ..Config::default()
        };
        Manager::start(vec![engine], Store::new(self.state_path()), cfg)
            .await
            .unwrap()
    }

    fn req(&self, s: &TestServer, name: &str) -> AddRequest {
        AddRequest {
            source: Source::Http {
                url: s.url("/file.bin"),
            },
            output_dir: Some(self.dl_dir()),
            filename: Some(name.into()),
            context: RequestContext::default(),
        }
    }
}

async fn server(f: impl FnOnce(&mut ServerCfg)) -> TestServer {
    let mut c = ServerCfg::default();
    f(&mut c);
    TestServer::start(c).await
}

fn snap(m: &Manager, id: DownloadId) -> DownloadSnapshot {
    m.get(id).expect("download exists").0
}

/// Polls the snapshot until `pred` holds.
async fn wait_until(m: &Manager, what: &str, pred: impl Fn(&[DownloadSnapshot]) -> bool) {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        let (list, _) = m.snapshot();
        if pred(&list) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {what}: {list:#?}"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

async fn wait_state(m: &Manager, id: DownloadId, pred: impl Fn(&DownloadState) -> bool) {
    wait_until(m, &format!("state of {id}"), |list| {
        list.iter().any(|d| d.id == id && pred(&d.state))
    })
    .await;
}

async fn wait_completed(m: &Manager, id: DownloadId) {
    wait_state(m, id, |s| matches!(s, DownloadState::Completed)).await;
}

async fn wait_active_with_bytes(m: &Manager, id: DownloadId) {
    wait_until(m, &format!("{id} transferring"), |list| {
        list.iter()
            .any(|d| d.id == id && matches!(d.state, DownloadState::Active) && d.done_bytes > 0)
    })
    .await;
}

async fn next_event(rx: &mut broadcast::Receiver<Event>, pred: impl Fn(&Event) -> bool) -> Event {
    let deadline = tokio::time::Instant::now() + TIMEOUT;
    loop {
        let ev = tokio::time::timeout_at(deadline, rx.recv())
            .await
            .expect("timed out waiting for event")
            .expect("event channel");
        if pred(&ev) {
            return ev;
        }
    }
}

fn assert_file(path: &Path, len: u64, version: u32) {
    let data = std::fs::read(path).unwrap();
    assert_eq!(data.len() as u64, len, "{}", path.display());
    for (i, b) in data.iter().enumerate() {
        assert_eq!(*b, body_byte(i as u64, version), "byte {i}");
    }
}

fn active_count(list: &[DownloadSnapshot]) -> usize {
    list.iter()
        .filter(|d| matches!(d.state, DownloadState::Starting | DownloadState::Active))
        .count()
}

#[tokio::test]
async fn add_downloads_to_completion() {
    let env = Env::new();
    let s = server(|c| c.len = 256 * KIB).await;
    let m = env.manager(3).await;
    let mut rx = m.events();
    let id = m.add(env.req(&s, "a.bin"), false, None).await.unwrap();
    assert_eq!(
        next_event(&mut rx, |_| true).await,
        Event::Added { id },
        "Added comes first"
    );
    let ev = next_event(&mut rx, |e| matches!(e, Event::Completed { .. })).await;
    assert_eq!(
        ev,
        Event::Completed {
            id,
            name: "a.bin".into()
        }
    );
    wait_completed(&m, id).await;
    let d = snap(&m, id);
    let path = d.path.clone().expect("completed download has a path");
    assert_eq!(path, env.dl_dir().join("a.bin"));
    assert_eq!(d.done_bytes, 256 * KIB);
    assert_file(&path, 256 * KIB, 0);
    assert!(m.is_idle());
    m.shutdown().await;
}

#[tokio::test]
async fn max_active_limits_concurrency() {
    let env = Env::new();
    let s = server(|c| {
        c.len = 128 * KIB;
        c.bytes_per_sec = Some(256 * KIB);
    })
    .await;
    let m = env.manager(2).await;
    let mut ids = Vec::new();
    for i in 0..4 {
        ids.push(
            m.add(env.req(&s, &format!("f{i}.bin")), false, None)
                .await
                .unwrap(),
        );
    }
    let deadline = Instant::now() + TIMEOUT;
    let mut max_seen = 0;
    loop {
        let (list, totals) = m.snapshot();
        let active = active_count(&list);
        assert!(active <= 2, "too many active: {list:#?}");
        assert_eq!(totals.active, active);
        max_seen = max_seen.max(active);
        if list
            .iter()
            .all(|d| matches!(d.state, DownloadState::Completed))
        {
            break;
        }
        assert!(Instant::now() < deadline, "timed out: {list:#?}");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert_eq!(max_seen, 2, "two downloads ran concurrently");
    for (i, _) in ids.iter().enumerate() {
        assert_file(&env.dl_dir().join(format!("f{i}.bin")), 128 * KIB, 0);
    }
    m.shutdown().await;
}

#[tokio::test]
async fn queued_pause_and_resume() {
    let env = Env::new();
    let s = server(|c| {
        c.len = 128 * KIB;
        c.bytes_per_sec = Some(256 * KIB);
    })
    .await;
    let m = env.manager(1).await;
    let a = m.add(env.req(&s, "a.bin"), false, None).await.unwrap();
    let b = m.add(env.req(&s, "b.bin"), false, None).await.unwrap();
    let c = m.add(env.req(&s, "c.bin"), true, None).await.unwrap();
    assert!(matches!(snap(&m, b).state, DownloadState::Queued));
    assert_eq!(snap(&m, b).queue_position, Some(0));
    assert!(matches!(snap(&m, c).state, DownloadState::Paused));

    m.pause(b).await.unwrap();
    assert!(matches!(snap(&m, b).state, DownloadState::Paused));
    assert_eq!(snap(&m, b).queue_position, None);
    wait_completed(&m, a).await;
    // Nothing is queued any more: the paused downloads stay put.
    assert!(m.is_idle());
    assert!(matches!(snap(&m, b).state, DownloadState::Paused));
    assert!(matches!(snap(&m, c).state, DownloadState::Paused));
    let paths = s.paths_seen().len();

    m.resume(b).await.unwrap();
    m.resume(c).await.unwrap();
    wait_completed(&m, b).await;
    wait_completed(&m, c).await;
    assert!(s.paths_seen().len() > paths);
    assert_file(&env.dl_dir().join("b.bin"), 128 * KIB, 0);
    assert_file(&env.dl_dir().join("c.bin"), 128 * KIB, 0);
    assert!(matches!(
        m.pause(a).await,
        Err(ManagerError::InvalidState(_))
    ));
    m.shutdown().await;
}

#[tokio::test]
async fn pause_and_resume_active_download() {
    let env = Env::new();
    let s = server(|c| {
        c.len = 256 * KIB;
        c.bytes_per_sec = Some(256 * KIB);
    })
    .await;
    let m = env.manager(3).await;
    let a = m.add(env.req(&s, "a.bin"), false, None).await.unwrap();
    wait_active_with_bytes(&m, a).await;
    m.pause(a).await.unwrap();
    assert!(matches!(snap(&m, a).state, DownloadState::Paused));
    assert!(m.is_idle());
    m.resume(a).await.unwrap();
    wait_completed(&m, a).await;
    assert_file(&env.dl_dir().join("a.bin"), 256 * KIB, 0);
    m.shutdown().await;
}

#[tokio::test]
async fn move_reorders_queue() {
    let env = Env::new();
    let s = server(|c| {
        c.len = 128 * KIB;
        c.bytes_per_sec = Some(256 * KIB);
    })
    .await;
    let m = env.manager(1).await;
    let mut ids = Vec::new();
    for name in ["a", "b", "c", "d"] {
        ids.push(m.add(env.req(&s, name), false, None).await.unwrap());
    }
    let (a, b, c, d) = (ids[0], ids[1], ids[2], ids[3]);
    m.move_to(d, 0).await.unwrap();
    assert_eq!(snap(&m, d).queue_position, Some(0));
    assert_eq!(snap(&m, b).queue_position, Some(1));
    assert_eq!(snap(&m, c).queue_position, Some(2));
    let (list, _) = m.snapshot();
    assert_eq!(
        list.iter().map(|x| x.id).collect::<Vec<_>>(),
        vec![d, a, b, c]
    );
    // Once `a` is done, `d` is the next one started.
    wait_until(&m, "a successor", |list| {
        list.iter()
            .any(|x| x.id != a && !matches!(x.state, DownloadState::Queued))
    })
    .await;
    let (list, _) = m.snapshot();
    let started: Vec<_> = list
        .iter()
        .filter(|x| x.id != a && !matches!(x.state, DownloadState::Queued))
        .map(|x| x.id)
        .collect();
    assert_eq!(started, vec![d], "{list:#?}");
    assert!(matches!(
        m.move_to(99, 0).await,
        Err(ManagerError::NotFound(99))
    ));
    m.shutdown().await;
}

#[tokio::test]
async fn global_limit_divided() {
    let env = Env::new();
    let s = server(|c| c.len = 512 * KIB).await;
    let m = env.manager(3).await;
    m.set_global_limits(Some(256 * KIB), Some(7));
    let settings = m.settings();
    assert_eq!(settings.global_down_limit, Some(256 * KIB));
    assert_eq!(settings.global_up_limit, Some(7));
    let t0 = Instant::now();
    let a = m.add(env.req(&s, "a.bin"), false, None).await.unwrap();
    let b = m.add(env.req(&s, "b.bin"), false, None).await.unwrap();
    wait_completed(&m, a).await;
    wait_completed(&m, b).await;
    let took = t0.elapsed();
    assert!(took >= Duration::from_millis(3500), "took only {took:?}");
    // 0 means unlimited and is reported as such.
    m.set_global_limits(Some(0), None);
    assert_eq!(m.settings().global_down_limit, None);
    m.shutdown().await;
}

#[tokio::test]
async fn per_download_limit_is_applied_and_stored() {
    // The token bucket admits a chunk once a burst (limit/4) is available and takes the
    // rest as debt that is never waited out for the *last* chunk, so a transfer of `len`
    // bytes takes at least `(len - last_chunk) / limit`. Chunks are whatever hyper's read
    // buffer holds, which grows up to ~408 KiB under load; with a 1 MiB file at 512 KiB/s
    // (nominally 2 s) that still leaves >= 1.2 s, while an unlimited local transfer takes
    // a few milliseconds.
    const LEN: u64 = 1024 * KIB;
    const LIMIT: u64 = 512 * KIB;
    const MAX_CHUNK: u64 = 408 * KIB;
    let env = Env::new();
    let s = server(|c| c.len = LEN).await;
    let m = env.manager(3).await;
    let t0 = Instant::now();
    let a = m
        .add(env.req(&s, "a.bin"), false, Some(LIMIT))
        .await
        .unwrap();
    assert_eq!(snap(&m, a).limit, Some(LIMIT));
    wait_completed(&m, a).await;
    let min = Duration::from_secs_f64((LEN - MAX_CHUNK) as f64 / LIMIT as f64);
    assert!(t0.elapsed() >= min, "{:?} < {min:?}", t0.elapsed());
    m.set_limit(a, Some(0)).unwrap();
    assert_eq!(snap(&m, a).limit, None);
    assert!(matches!(
        m.set_limit(42, None),
        Err(ManagerError::NotFound(42))
    ));
    m.shutdown().await;
}

#[tokio::test]
async fn remove_completed_deletes_file() {
    let env = Env::new();
    let s = server(|c| c.len = 64 * KIB).await;
    let m = env.manager(3).await;
    let mut rx = m.events();
    let id = m.add(env.req(&s, "a.bin"), false, None).await.unwrap();
    wait_completed(&m, id).await;
    let path = snap(&m, id).path.unwrap();
    assert!(path.exists());
    m.remove(id, true).await.unwrap();
    assert_eq!(
        next_event(&mut rx, |e| matches!(e, Event::Removed { .. })).await,
        Event::Removed { id }
    );
    assert!(!path.exists());
    assert!(m.snapshot().0.is_empty());
    assert!(matches!(m.get(id), Err(ManagerError::NotFound(_))));
    assert_eq!(
        m.remove(id, true).await.unwrap_err().code(),
        ErrorCode::NotFound
    );
    m.shutdown().await;
}

#[tokio::test]
async fn remove_active_download_deletes_partial_files() {
    let env = Env::new();
    let s = server(|c| {
        c.len = 256 * KIB;
        c.bytes_per_sec = Some(128 * KIB);
    })
    .await;
    let m = env.manager(3).await;
    let id = m.add(env.req(&s, "a.bin"), false, None).await.unwrap();
    wait_active_with_bytes(&m, id).await;
    m.remove(id, true).await.unwrap();
    assert!(m.snapshot().0.is_empty());
    assert!(m.is_idle());
    let left: Vec<_> = std::fs::read_dir(env.dl_dir())
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    assert!(left.is_empty(), "{left:?}");
    m.shutdown().await;
}

#[tokio::test]
async fn restart_after_needs_attention() {
    let env = Env::new();
    let s = server(|c| {
        c.len = 256 * KIB;
        c.bytes_per_sec = Some(256 * KIB);
    })
    .await;
    let m = env.manager(3).await;
    let mut rx = m.events();
    let id = m.add(env.req(&s, "a.bin"), false, None).await.unwrap();
    wait_active_with_bytes(&m, id).await;
    m.pause(id).await.unwrap();
    s.set_version(1);
    m.resume(id).await.unwrap();
    wait_state(&m, id, |s| {
        matches!(s, DownloadState::NeedsAttention { .. })
    })
    .await;
    assert!(matches!(
        next_event(&mut rx, |e| matches!(e, Event::NeedsAttention { .. })).await,
        Event::NeedsAttention { id: got, .. } if got == id
    ));
    assert!(matches!(
        m.resume(id).await,
        Err(ManagerError::InvalidState(_))
    ));
    s.set_cfg(|c| c.bytes_per_sec = None);
    m.restart(id).await.unwrap();
    wait_completed(&m, id).await;
    let path = snap(&m, id).path.unwrap();
    assert_eq!(path, env.dl_dir().join("a.bin"));
    assert_file(&path, 256 * KIB, 1);
    assert!(matches!(
        m.restart(id).await,
        Err(ManagerError::InvalidState(_))
    ));
    m.shutdown().await;
}

#[tokio::test]
async fn unsupported_source_is_bad_request() {
    let env = Env::new();
    let m = env.manager(3).await;
    let req = AddRequest {
        source: Source::Magnet {
            uri: "magnet:?xt=urn:btih:0123456789abcdef0123456789abcdef01234567".into(),
        },
        output_dir: None,
        filename: None,
        context: RequestContext::default(),
    };
    let err = m.add(req, false, None).await.unwrap_err();
    assert!(matches!(err, ManagerError::BadRequest(_)), "{err:?}");
    assert_eq!(err.code(), ErrorCode::BadRequest);
    assert!(m.snapshot().0.is_empty());
    assert!(matches!(
        m.set_max_active(0),
        Err(ManagerError::BadRequest(_))
    ));
    m.set_max_active(5).unwrap();
    assert_eq!(m.settings().max_active, 5);
    m.shutdown().await;
}

#[tokio::test]
async fn failed_probe_is_failed_and_restartable() {
    let env = Env::new();
    let s = server(|c| c.len = 64 * KIB).await;
    let m = env.manager(3).await;
    let mut rx = m.events();
    let mut req = env.req(&s, "a.bin");
    req.source = Source::Http {
        url: s.url("/missing"),
    };
    let id = m.add(req, false, None).await.unwrap();
    wait_state(&m, id, |s| matches!(s, DownloadState::Failed { .. })).await;
    assert!(matches!(
        next_event(&mut rx, |e| matches!(e, Event::Failed { .. })).await,
        Event::Failed { id: got, .. } if got == id
    ));
    assert!(m.is_idle());
    m.restart(id).await.unwrap();
    wait_state(&m, id, |s| matches!(s, DownloadState::Failed { .. })).await;
    m.shutdown().await;
}

/// Drives one download of each state, restarts the manager on the same store and checks
/// that every state survives, that the paused/failed/needs-attention ones made no
/// network request while being restored, and that the active one resumes to completion.
#[tokio::test]
async fn restart_restores_every_state() {
    let env = Env::new();
    let slow = |c: &mut ServerCfg| {
        c.len = 512 * KIB;
        c.bytes_per_sec = Some(128 * KIB);
    };
    let s_done = server(|c| c.len = 64 * KIB).await;
    let s_paused = server(slow).await;
    let s_failed = server(slow).await;
    let s_na = server(slow).await;
    let s_active = server(slow).await;
    let s_queued = server(|c| c.len = 64 * KIB).await;

    let m = env.manager(1).await;
    let done = m
        .add(env.req(&s_done, "done.bin"), false, None)
        .await
        .unwrap();
    wait_completed(&m, done).await;

    let paused = m
        .add(env.req(&s_paused, "paused.bin"), false, None)
        .await
        .unwrap();
    wait_active_with_bytes(&m, paused).await;
    m.pause(paused).await.unwrap();

    let failed = m
        .add(env.req(&s_failed, "failed.bin"), false, None)
        .await
        .unwrap();
    wait_active_with_bytes(&m, failed).await;
    m.pause(failed).await.unwrap();
    s_failed.set_cfg(|c| c.stall_headers = true);
    m.resume(failed).await.unwrap();
    wait_state(&m, failed, |s| matches!(s, DownloadState::Failed { .. })).await;

    let na = m.add(env.req(&s_na, "na.bin"), false, None).await.unwrap();
    wait_active_with_bytes(&m, na).await;
    m.pause(na).await.unwrap();
    s_na.set_version(1);
    m.resume(na).await.unwrap();
    wait_state(&m, na, |s| {
        matches!(s, DownloadState::NeedsAttention { .. })
    })
    .await;

    let active = m
        .add(env.req(&s_active, "active.bin"), false, None)
        .await
        .unwrap();
    wait_active_with_bytes(&m, active).await;
    let queued = m
        .add(env.req(&s_queued, "queued.bin"), false, None)
        .await
        .unwrap();
    assert!(matches!(snap(&m, queued).state, DownloadState::Queued));

    m.shutdown().await;
    drop(m);

    let quiet = [&s_paused, &s_failed, &s_na];
    let before: Vec<usize> = quiet.iter().map(|s| s.paths_seen().len()).collect();
    let queued_before = s_queued.paths_seen().len();
    assert_eq!(queued_before, 0);
    s_active.set_cfg(|c| c.bytes_per_sec = None);

    let m = env.manager(1).await;
    let state = |id| snap(&m, id).state;
    assert!(matches!(state(done), DownloadState::Completed));
    assert!(
        matches!(state(paused), DownloadState::Paused),
        "{:?}",
        state(paused)
    );
    assert!(matches!(state(failed), DownloadState::Failed { .. }));
    assert!(matches!(state(na), DownloadState::NeedsAttention { .. }));
    assert!(matches!(state(queued), DownloadState::Queued));
    assert!(matches!(
        state(active),
        DownloadState::Queued
            | DownloadState::Starting
            | DownloadState::Active
            | DownloadState::Completed
    ));
    // The previously active download is ahead of the queued one.
    let (list, _) = m.snapshot();
    let pos = |id| list.iter().position(|d| d.id == id).unwrap();
    assert!(pos(active) < pos(queued), "{list:#?}");
    // Restored downloads keep their paths.
    assert_eq!(snap(&m, paused).path, Some(env.dl_dir().join("paused.bin")));
    assert_eq!(snap(&m, done).path, Some(env.dl_dir().join("done.bin")));

    wait_completed(&m, active).await;
    assert_file(&env.dl_dir().join("active.bin"), 512 * KIB, 0);
    wait_completed(&m, queued).await;
    assert_file(&env.dl_dir().join("queued.bin"), 64 * KIB, 0);
    let after: Vec<usize> = quiet.iter().map(|s| s.paths_seen().len()).collect();
    assert_eq!(before, after, "restoring made network requests");
    assert!(matches!(state(paused), DownloadState::Paused));
    assert!(matches!(state(failed), DownloadState::Failed { .. }));
    assert!(matches!(state(na), DownloadState::NeedsAttention { .. }));

    // The paused one still resumes fine afterwards.
    s_paused.set_cfg(|c| c.bytes_per_sec = None);
    m.resume(paused).await.unwrap();
    wait_completed(&m, paused).await;
    assert_file(&env.dl_dir().join("paused.bin"), 512 * KIB, 0);
    m.shutdown().await;
}

/// An `HttpEngine` whose `add` waits for a permit before starting (`before`) and before
/// returning (`after`), to race commands and events against a start.
struct Gated {
    inner: HttpEngine,
    before: tokio::sync::Semaphore,
    after: tokio::sync::Semaphore,
}

#[async_trait::async_trait]
impl Engine for Gated {
    fn kind(&self) -> puppy_core::engine::EngineKind {
        self.inner.kind()
    }
    fn accepts(&self, source: &Source) -> bool {
        self.inner.accepts(source)
    }
    async fn add(
        &self,
        id: DownloadId,
        req: AddRequest,
        mode: puppy_core::engine::StartMode,
        opts: puppy_core::engine::AddOptions,
    ) -> Result<puppy_core::engine::Added, puppy_core::engine::EngineError> {
        self.before.acquire().await.unwrap().forget();
        let r = self.inner.add(id, req, mode, opts).await;
        self.after.acquire().await.unwrap().forget();
        r
    }
    async fn pause(&self, id: DownloadId) -> Result<(), puppy_core::engine::EngineError> {
        self.inner.pause(id).await
    }
    async fn resume(&self, id: DownloadId) -> Result<(), puppy_core::engine::EngineError> {
        self.inner.resume(id).await
    }
    async fn remove(
        &self,
        id: DownloadId,
        delete_files: bool,
    ) -> Result<(), puppy_core::engine::EngineError> {
        self.inner.remove(id, delete_files).await
    }
    fn status(&self, id: DownloadId) -> Option<puppy_core::types::DownloadStatus> {
        self.inner.status(id)
    }
    fn detail(&self, id: DownloadId) -> Option<puppy_core::types::EngineDetail> {
        self.inner.detail(id)
    }
    fn set_download_limit(
        &self,
        id: DownloadId,
        bps: Option<u64>,
    ) -> Result<(), puppy_core::engine::EngineError> {
        self.inner.set_download_limit(id, bps)
    }
    fn set_global_download_budget(&self, bps: Option<u64>) {
        self.inner.set_global_download_budget(bps)
    }
    fn subscribe(&self) -> broadcast::Receiver<puppy_core::engine::EngineEvent> {
        self.inner.subscribe()
    }
}

impl Env {
    async fn gated_manager(&self) -> (Arc<Manager>, Arc<Gated>) {
        let gated = Arc::new(Gated {
            inner: HttpEngine::new(http_cfg(&self.dl_dir())).unwrap(),
            before: tokio::sync::Semaphore::new(0),
            after: tokio::sync::Semaphore::new(0),
        });
        let cfg = Config {
            download_dir: self.dl_dir(),
            ..Config::default()
        };
        let m = Manager::start(vec![gated.clone()], Store::new(self.state_path()), cfg)
            .await
            .unwrap();
        (m, gated)
    }
}

fn dir_entries(dir: &Path) -> Vec<PathBuf> {
    match std::fs::read_dir(dir) {
        Ok(rd) => rd.map(|e| e.unwrap().path()).collect(),
        Err(_) => Vec::new(),
    }
}

async fn wait_for(what: &str, cond: impl Fn() -> bool) {
    let deadline = Instant::now() + TIMEOUT;
    while !cond() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test]
async fn remove_while_starting_removes_once_add_returns() {
    let env = Env::new();
    let s = server(|c| {
        c.len = 256 * KIB;
        c.bytes_per_sec = Some(64 * KIB);
    })
    .await;
    let (m, gated) = env.gated_manager().await;
    let id = m.add(env.req(&s, "a.bin"), false, None).await.unwrap();
    wait_state(&m, id, |s| matches!(s, DownloadState::Starting)).await;
    m.remove(id, true).await.unwrap();
    assert!(m.snapshot().0.is_empty());
    assert!(m.is_idle());
    // The add now completes; the manager removes the download, files and all.
    gated.before.add_permits(1);
    gated.after.add_permits(1);
    wait_for("the engine to drop the download", || {
        gated.status(id).is_none() && gated.after.available_permits() == 0
    })
    .await;
    wait_for("the partial files to go", || {
        dir_entries(&env.dl_dir()).is_empty()
    })
    .await;
    assert!(m.snapshot().0.is_empty());
    m.shutdown().await;
}

#[tokio::test]
async fn pause_while_starting_pauses_once_add_returns() {
    let env = Env::new();
    let s = server(|c| {
        c.len = 256 * KIB;
        c.bytes_per_sec = Some(256 * KIB);
    })
    .await;
    let (m, gated) = env.gated_manager().await;
    let id = m.add(env.req(&s, "a.bin"), false, None).await.unwrap();
    wait_state(&m, id, |s| matches!(s, DownloadState::Starting)).await;
    m.pause(id).await.unwrap();
    gated.before.add_permits(1);
    gated.after.add_permits(1);
    wait_state(&m, id, |s| matches!(s, DownloadState::Paused)).await;
    assert!(m.is_idle());
    assert!(matches!(
        gated.status(id).unwrap().state,
        DownloadState::Paused
    ));
    assert_eq!(snap(&m, id).path, Some(env.dl_dir().join("a.bin")));
    m.resume(id).await.unwrap();
    wait_completed(&m, id).await;
    assert_file(&env.dl_dir().join("a.bin"), 256 * KIB, 0);
    m.shutdown().await;
}

/// The engine finishes the download (and broadcasts `Completed`) before `add` returns.
#[tokio::test]
async fn completion_before_add_returns_is_not_lost() {
    let env = Env::new();
    let s = server(|c| c.len = 64 * KIB).await;
    let (m, gated) = env.gated_manager().await;
    let mut rx = m.events();
    let id = m.add(env.req(&s, "a.bin"), false, None).await.unwrap();
    gated.before.add_permits(1);
    wait_for("the engine to complete", || {
        gated
            .status(id)
            .is_some_and(|st| matches!(st.state, DownloadState::Completed))
    })
    .await;
    assert!(matches!(snap(&m, id).state, DownloadState::Starting));
    gated.after.add_permits(1);
    wait_completed(&m, id).await;
    assert_eq!(
        next_event(&mut rx, |e| matches!(e, Event::Completed { .. })).await,
        Event::Completed {
            id,
            name: "a.bin".into()
        }
    );
    wait_for("the engine to drop it", || gated.status(id).is_none()).await;
    assert_file(&env.dl_dir().join("a.bin"), 64 * KIB, 0);
    assert!(m.is_idle());
    m.shutdown().await;
}
