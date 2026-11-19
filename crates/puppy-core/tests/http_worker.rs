mod support;

use puppy_core::config::HttpConfig;
use puppy_core::http::probe::build_client;
use puppy_core::http::worker::{SegmentShared, WorkerCtx, WorkerExit, run_segment};
use puppy_core::limiter::TokenBucket;
use puppy_core::types::RequestContext;
use std::fs::OpenOptions;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};
use support::{ServerCfg, TestServer, body_byte};
use tokio_util::sync::CancellationToken;

const LEN: u64 = 64 * 1024;

fn cfg() -> HttpConfig {
    HttpConfig {
        backoff_base_ms: 10,
        backoff_max_ms: 100,
        ..HttpConfig::default()
    }
}

fn seg(end: u64) -> Arc<SegmentShared> {
    Arc::new(SegmentShared {
        done: AtomicU64::new(0),
        end: AtomicU64::new(end),
        bps: AtomicU64::new(0),
        active: AtomicBool::new(false),
    })
}

fn make_ctx(s: &TestServer, file: std::fs::File, if_range: Option<&str>) -> WorkerCtx {
    let cfg = cfg();
    WorkerCtx {
        client: build_client(&cfg).unwrap(),
        url: s.url("/file.bin"),
        context: RequestContext::default(),
        if_range: if_range.map(str::to_string),
        expected_total: Some(LEN),
        file: Arc::new(file),
        limiters: [TokenBucket::new(None), TokenBucket::new(None)],
        cfg,
        cancel: CancellationToken::new(),
    }
}

fn rw_file(dir: &tempfile::TempDir) -> std::fs::File {
    let f = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(dir.path().join("f.part"))
        .unwrap();
    f.set_len(LEN).unwrap();
    f
}

fn read_all(dir: &tempfile::TempDir) -> Vec<u8> {
    std::fs::read(dir.path().join("f.part")).unwrap()
}

fn server(f: impl FnOnce(&mut ServerCfg)) -> impl std::future::Future<Output = TestServer> {
    let mut c = ServerCfg {
        len: LEN,
        ..ServerCfg::default()
    };
    f(&mut c);
    TestServer::start(c)
}

#[tokio::test]
async fn worker_downloads_range() {
    let s = server(|_| {}).await;
    let dir = tempfile::tempdir().unwrap();
    let ctx = make_ctx(&s, rw_file(&dir), Some("\"v0\""));
    let sh = seg(5000);
    sh.done.store(0, Ordering::Relaxed);
    let exit = run_segment(&ctx, 1000, sh.clone()).await;
    // end is absolute (5000); done counts from start (1000).
    assert_eq!(exit, WorkerExit::Done);
    assert_eq!(sh.done.load(Ordering::Relaxed), 4000);
    let data = read_all(&dir);
    for i in 1000..5000u64 {
        assert_eq!(data[i as usize], body_byte(i, 0), "byte {i}");
    }
    assert_eq!(data[999], 0);
    assert_eq!(data[5000], 0);
    assert!(!sh.active.load(Ordering::Relaxed));
}

#[tokio::test]
async fn worker_retries_after_drop() {
    let s = server(|c| c.fail_after_bytes_once = Some(1500)).await;
    let dir = tempfile::tempdir().unwrap();
    let ctx = make_ctx(&s, rw_file(&dir), None);
    let sh = seg(20_000);
    let exit = run_segment(&ctx, 0, sh.clone()).await;
    assert_eq!(exit, WorkerExit::Done);
    let data = read_all(&dir);
    for i in 0..20_000u64 {
        assert_eq!(data[i as usize], body_byte(i, 0), "byte {i}");
    }
    assert!(s.range_requests() >= 2);
}

#[tokio::test]
async fn worker_detects_remote_change() {
    let s = server(|_| {}).await;
    s.set_version(1);
    let dir = tempfile::tempdir().unwrap();
    let ctx = make_ctx(&s, rw_file(&dir), Some("\"v0\""));
    let exit = run_segment(&ctx, 0, seg(5000)).await;
    assert_eq!(exit, WorkerExit::RemoteChanged);
}

#[tokio::test]
async fn worker_rejects_200_for_range() {
    let s = server(|c| c.ranges_lie = true).await;
    let dir = tempfile::tempdir().unwrap();
    let ctx = make_ctx(&s, rw_file(&dir), None);
    let exit = run_segment(&ctx, 1000, seg(5000)).await;
    assert_eq!(exit, WorkerExit::RangeUnsupported);
    assert!(read_all(&dir).iter().all(|b| *b == 0));
}

#[tokio::test]
async fn worker_honours_shrunk_end() {
    let s = server(|c| c.bytes_per_sec = Some(64 * 1024)).await;
    let dir = tempfile::tempdir().unwrap();
    let ctx = make_ctx(&s, rw_file(&dir), None);
    let sh = seg(LEN);
    let sh2 = sh.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        sh2.end.store(3000, Ordering::Relaxed);
    });
    let exit = run_segment(&ctx, 0, sh.clone()).await;
    assert_eq!(exit, WorkerExit::Done);
    assert_eq!(sh.done.load(Ordering::Relaxed), 3000);
    let data = read_all(&dir);
    assert_eq!(data[2999], body_byte(2999, 0));
    assert!(data[3000..].iter().all(|b| *b == 0));
}

#[tokio::test]
async fn worker_disk_error_is_fatal() {
    let s = server(|_| {}).await;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("f.part");
    std::fs::write(&path, vec![0u8; LEN as usize]).unwrap();
    let ro = OpenOptions::new().read(true).open(&path).unwrap();
    let ctx = make_ctx(&s, ro, None);
    let exit = run_segment(&ctx, 0, seg(5000)).await;
    assert!(matches!(exit, WorkerExit::Fatal(_)), "{exit:?}");
    assert_eq!(s.requests_seen().len(), 1);
}

#[tokio::test]
async fn worker_cancel() {
    let s = server(|c| c.bytes_per_sec = Some(16 * 1024)).await;
    let dir = tempfile::tempdir().unwrap();
    let ctx = make_ctx(&s, rw_file(&dir), None);
    let token = ctx.cancel.clone();
    let canceller = async move {
        tokio::time::sleep(Duration::from_millis(150)).await;
        token.cancel();
        Instant::now()
    };
    let (exit, cancelled_at) = tokio::join!(
        async {
            let e = run_segment(&ctx, 0, seg(LEN)).await;
            (e, Instant::now())
        },
        canceller
    );
    assert_eq!(exit.0, WorkerExit::Cancelled);
    assert!(exit.1.saturating_duration_since(cancelled_at) < Duration::from_millis(200));
}
