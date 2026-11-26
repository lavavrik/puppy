use puppy_core::config::HttpConfig;
use puppy_core::http::probe::build_client;
use puppy_core::http::worker::{SegmentShared, WorkerCtx, WorkerExit, run_segment};
use puppy_core::limiter::TokenBucket;
use puppy_core::types::RequestContext;
use puppy_testserver::{ServerCfg, TestServer, body_byte};
use std::fs::OpenOptions;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;

const LEN: u64 = 64 * 1024;

fn cfg() -> HttpConfig {
    HttpConfig {
        backoff_base_ms: 10,
        backoff_max_ms: 100,
        ..HttpConfig::default()
    }
}

fn seg(start: u64, end: u64) -> Arc<SegmentShared> {
    Arc::new(SegmentShared::new(start, end, 0))
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
        failover: false,
        got: Default::default(),
        ftp: None,
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
    let sh = seg(1000, 5000);
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
    let sh = seg(0, 20_000);
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
    let exit = run_segment(&ctx, 0, seg(0, 5000)).await;
    assert_eq!(exit, WorkerExit::RemoteChanged);
}

#[tokio::test]
async fn worker_rejects_200_for_range() {
    let s = server(|c| c.ranges_lie = true).await;
    let dir = tempfile::tempdir().unwrap();
    let ctx = make_ctx(&s, rw_file(&dir), None);
    let exit = run_segment(&ctx, 1000, seg(1000, 5000)).await;
    assert_eq!(exit, WorkerExit::RangeUnsupported);
    assert!(read_all(&dir).iter().all(|b| *b == 0));
}

#[tokio::test]
async fn worker_honours_shrunk_end() {
    const BIG: u64 = 1024 * 1024;
    let s = server(|c| {
        c.len = BIG;
        c.bytes_per_sec = Some(256 * 1024);
    })
    .await;
    let dir = tempfile::tempdir().unwrap();
    let f = rw_file(&dir);
    f.set_len(BIG).unwrap();
    std::fs::write(dir.path().join("f.part"), vec![0xEEu8; BIG as usize]).unwrap();
    let mut ctx = make_ctx(&s, f, None);
    ctx.expected_total = Some(BIG);
    let sh = seg(0, BIG);
    let stealer = {
        let sh = sh.clone();
        async move {
            while sh.done.load(Ordering::Acquire) < 64 * 1024 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            sh.try_steal(0, 16 * 1024).unwrap()
        }
    };
    let (exit, (mid, old_end)) = tokio::join!(run_segment(&ctx, 0, sh.clone()), stealer);
    assert_eq!(old_end, BIG);
    assert_eq!(exit, WorkerExit::Done);
    assert_eq!(sh.done.load(Ordering::Acquire), mid);
    assert_eq!(sh.end(), mid);
    let data = read_all(&dir);
    for i in 0..mid {
        assert_eq!(data[i as usize], body_byte(i, 0), "byte {i}");
    }
    assert!(data[mid as usize..].iter().all(|b| *b == 0xEE));
}

#[tokio::test]
async fn worker_end_already_satisfied() {
    let s = server(|_| {}).await;
    let dir = tempfile::tempdir().unwrap();
    let ctx = make_ctx(&s, rw_file(&dir), None);
    let sh = Arc::new(SegmentShared::new(1000, 5000, 4000));
    assert_eq!(run_segment(&ctx, 1000, sh).await, WorkerExit::Done);
    assert!(s.requests_seen().is_empty());
}

#[tokio::test]
async fn worker_wrong_total_is_remote_changed() {
    let s = server(|_| {}).await;
    let dir = tempfile::tempdir().unwrap();
    let mut ctx = make_ctx(&s, rw_file(&dir), None);
    ctx.expected_total = Some(LEN + 1);
    assert_eq!(
        run_segment(&ctx, 0, seg(0, 5000)).await,
        WorkerExit::RemoteChanged
    );
}

#[tokio::test]
async fn worker_retries_exhausted_is_transient() {
    let s = server(|_| {}).await;
    let dir = tempfile::tempdir().unwrap();
    let mut ctx = make_ctx(&s, rw_file(&dir), None);
    ctx.url = s.url("/status/503");
    ctx.cfg.max_retries = 2;
    let exit = run_segment(&ctx, 0, seg(0, 5000)).await;
    assert!(matches!(exit, WorkerExit::Exhausted(_)), "{exit:?}");
    assert_eq!(s.requests_seen().len(), 3);
}

#[tokio::test]
async fn worker_disk_error_is_fatal() {
    let s = server(|_| {}).await;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("f.part");
    std::fs::write(&path, vec![0u8; LEN as usize]).unwrap();
    let ro = OpenOptions::new().read(true).open(&path).unwrap();
    let ctx = make_ctx(&s, ro, None);
    let exit = run_segment(&ctx, 0, seg(0, 5000)).await;
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
            let e = run_segment(&ctx, 0, seg(0, LEN)).await;
            (e, Instant::now())
        },
        canceller
    );
    assert_eq!(exit.0, WorkerExit::Cancelled);
    assert!(exit.1.saturating_duration_since(cancelled_at) < Duration::from_millis(200));
}

#[tokio::test]
async fn worker_200_with_matching_validator_is_range_unsupported() {
    // The server ignores ranges but the file is unchanged (same ETag): not a remote change.
    let s = server(|c| c.ranges_lie = true).await;
    let dir = tempfile::tempdir().unwrap();
    let ctx = make_ctx(&s, rw_file(&dir), Some("\"v0\""));
    let exit = run_segment(&ctx, 1000, seg(1000, 5000)).await;
    assert_eq!(exit, WorkerExit::RangeUnsupported);
    assert!(read_all(&dir).iter().all(|b| *b == 0));
}

#[tokio::test]
async fn worker_200_with_changed_validator_is_remote_changed() {
    let s = server(|c| c.ranges_lie = true).await;
    s.set_version(1);
    let dir = tempfile::tempdir().unwrap();
    let ctx = make_ctx(&s, rw_file(&dir), Some("\"v0\""));
    let exit = run_segment(&ctx, 1000, seg(1000, 5000)).await;
    assert_eq!(exit, WorkerExit::RemoteChanged);
}

#[tokio::test]
async fn worker_retries_after_read_timeout() {
    // The first request stalls before sending headers; the read timeout turns it into a
    // retryable error and the next attempt (server no longer stalling) succeeds.
    let s = server(|c| c.stall_headers = true).await;
    let dir = tempfile::tempdir().unwrap();
    let mut ctx = make_ctx(&s, rw_file(&dir), Some("\"v0\""));
    ctx.cfg.read_timeout_ms = 200;
    ctx.client = build_client(&ctx.cfg).unwrap();
    let sh = seg(0, LEN);
    let t = Instant::now();
    let (exit, ()) = tokio::join!(run_segment(&ctx, 0, sh.clone()), async {
        while s.requests_seen().is_empty() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        s.set_cfg(|c| c.stall_headers = false);
    });
    assert_eq!(exit, WorkerExit::Done);
    assert!(s.requests_seen().len() >= 2, "no retry");
    assert!(t.elapsed() < Duration::from_secs(10), "{:?}", t.elapsed());
    let data = read_all(&dir);
    for (i, b) in data.iter().enumerate() {
        assert_eq!(*b, body_byte(i as u64, 0), "byte {i}");
    }
}

#[tokio::test]
async fn worker_416_mid_download_is_remote_changed() {
    // The file shrank below the segment: the server cannot satisfy the range.
    let s = server(|c| c.len = 1000).await;
    let dir = tempfile::tempdir().unwrap();
    let ctx = make_ctx(&s, rw_file(&dir), Some("\"v0\""));
    let exit = run_segment(&ctx, 2000, seg(2000, 5000)).await;
    assert_eq!(exit, WorkerExit::RemoteChanged);
}

#[tokio::test]
async fn worker_refuses_compressed_body() {
    let s = server(|c| c.content_encoding = Some("gzip".into())).await;
    let dir = tempfile::tempdir().unwrap();
    let ctx = make_ctx(&s, rw_file(&dir), Some("\"v0\""));
    let exit = run_segment(&ctx, 0, seg(0, 5000)).await;
    assert_eq!(
        exit,
        WorkerExit::Refused("server sent compressed content (gzip)".into())
    );
    assert!(
        read_all(&dir).iter().all(|b| *b == 0),
        "compressed bytes written"
    );
}
