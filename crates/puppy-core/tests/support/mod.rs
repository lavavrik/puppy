//! Configurable axum test server shared by HTTP-engine integration tests.
#![allow(dead_code)]

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::response::Response;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub fn body_byte(i: u64, version: u32) -> u8 {
    ((i * 31 + 7 + version as u64 * 13) % 251) as u8
}

#[derive(Debug, Clone)]
pub struct ServerCfg {
    pub len: u64,
    pub ranges: bool,
    pub etag: bool,
    pub last_modified: bool,
    pub content_length: bool,
    pub head_allowed: bool,
    /// Answer HEAD with `Content-Length: 0` regardless of the real length.
    pub head_zero_length: bool,
    /// Answer the first GET with 503 (re-armed by `set_cfg`).
    pub get_503_once: bool,
    /// Advertise range support but answer every request with 200 and the full body.
    pub ranges_lie: bool,
    pub disposition: Option<String>,
    pub bytes_per_sec: Option<u64>,
    /// Drop the connection once, after this many body bytes of a response.
    pub fail_after_bytes_once: Option<u64>,
    /// Accept requests but never answer them (checked when a request arrives).
    pub stall_headers: bool,
    /// `Content-Encoding` sent with GET responses (the body stays uncompressed).
    pub content_encoding: Option<String>,
    /// Ranged responses starting exactly at this offset are throttled to
    /// `slow_bytes_per_sec` instead of `bytes_per_sec` (makes one segment lag).
    pub slow_range_start: Option<u64>,
    pub slow_bytes_per_sec: Option<u64>,
}

impl Default for ServerCfg {
    fn default() -> Self {
        Self {
            len: 4 * 1024 * 1024,
            ranges: true,
            etag: true,
            last_modified: true,
            content_length: true,
            head_allowed: true,
            head_zero_length: false,
            get_503_once: false,
            ranges_lie: false,
            disposition: None,
            bytes_per_sec: None,
            fail_after_bytes_once: None,
            stall_headers: false,
            content_encoding: None,
            slow_range_start: None,
            slow_bytes_per_sec: None,
        }
    }
}

struct Shared {
    cfg: Mutex<ServerCfg>,
    version: AtomicU32,
    fail_used: AtomicBool,
    get_503_used: AtomicBool,
    requests: Mutex<Vec<HeaderMap>>,
    /// Request paths, in order.
    paths: Mutex<Vec<String>>,
    range_requests: Mutex<usize>,
    /// The only valid `/signed/<n>` (see `/expiring`).
    signed: AtomicU32,
}

pub struct TestServer {
    shared: Arc<Shared>,
    addr: std::net::SocketAddr,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl TestServer {
    pub async fn start(cfg: ServerCfg) -> Self {
        let shared = Arc::new(Shared {
            cfg: Mutex::new(cfg),
            version: AtomicU32::new(0),
            fail_used: AtomicBool::new(false),
            get_503_used: AtomicBool::new(false),
            requests: Mutex::new(Vec::new()),
            paths: Mutex::new(Vec::new()),
            range_requests: Mutex::new(0),
            signed: AtomicU32::new(0),
        });
        let app = Router::new().fallback(handle).with_state(shared.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Self { shared, addr, task }
    }

    pub fn url(&self, path: &str) -> url::Url {
        url::Url::parse(&format!("http://{}{}", self.addr, path)).unwrap()
    }

    /// Changes the body, ETag and Last-Modified.
    pub fn set_version(&self, v: u32) {
        self.shared.version.store(v, Ordering::SeqCst);
    }

    /// Number of GET requests that carried a Range header.
    pub fn range_requests(&self) -> usize {
        *self.shared.range_requests.lock().unwrap()
    }

    /// Mutates the config; re-arms `fail_after_bytes_once`.
    pub fn set_cfg(&self, f: impl FnOnce(&mut ServerCfg)) {
        f(&mut self.shared.cfg.lock().unwrap());
        self.shared.fail_used.store(false, Ordering::SeqCst);
        self.shared.get_503_used.store(false, Ordering::SeqCst);
    }

    /// Expires the current `/signed/<n>` URL: `/expiring` now redirects to `<n + 1>` and
    /// the old one answers 403.
    pub fn rotate_signed(&self) {
        self.shared.signed.fetch_add(1, Ordering::SeqCst);
    }

    /// Paths of every request received, in order.
    pub fn paths_seen(&self) -> Vec<String> {
        self.shared.paths.lock().unwrap().clone()
    }

    /// Headers of every request received, in order.
    pub fn requests_seen(&self) -> Vec<http::HeaderMap> {
        self.shared.requests.lock().unwrap().clone()
    }
}

fn etag_for(v: u32) -> String {
    format!("\"v{v}\"")
}

fn last_modified_for(v: u32) -> String {
    let s = v % 3600;
    format!("Mon, 01 Jan 2024 00:{:02}:{:02} GMT", s / 60, s % 60)
}

fn plain(status: StatusCode) -> Response {
    Response::builder()
        .status(status)
        .body(Body::empty())
        .unwrap()
}

/// Parses a single `bytes=a-b`, `bytes=a-` or `bytes=-n`. `Err` means unsatisfiable.
fn parse_range(v: &str, len: u64) -> Option<Result<(u64, u64), ()>> {
    let spec = v.trim().strip_prefix("bytes=")?;
    if spec.contains(',') {
        return None; // multi-range: ignore, serve the full body
    }
    let (a, b) = spec.split_once('-')?;
    let (start, end) = if a.is_empty() {
        let n: u64 = b.parse().ok()?;
        if n == 0 || len == 0 {
            return Some(Err(()));
        }
        (len.saturating_sub(n), len - 1)
    } else {
        let start: u64 = a.parse().ok()?;
        let end = if b.is_empty() {
            len.saturating_sub(1)
        } else {
            b.parse::<u64>().ok()?.min(len.saturating_sub(1))
        };
        (start, end)
    };
    if start >= len || start > end {
        return Some(Err(()));
    }
    Some(Ok((start, end)))
}

async fn handle(State(sh): State<Arc<Shared>>, req: Request) -> Response {
    sh.requests.lock().unwrap().push(req.headers().clone());
    let path = req.uri().path().to_string();
    sh.paths.lock().unwrap().push(path.clone());
    // A signed, expiring CDN URL: `/expiring` redirects to the current `/signed/<n>`;
    // older ones are forbidden. Otherwise served like `/file.bin`.
    if path == "/expiring" {
        let n = sh.signed.load(Ordering::SeqCst);
        return Response::builder()
            .status(StatusCode::FOUND)
            .header(header::LOCATION, format!("/signed/{n}"))
            .body(Body::empty())
            .unwrap();
    }
    if let Some(n) = path.strip_prefix("/signed/") {
        if n.parse::<u32>().ok() != Some(sh.signed.load(Ordering::SeqCst)) {
            return plain(StatusCode::FORBIDDEN);
        }
    } else if path == "/redirect" {
        return Response::builder()
            .status(StatusCode::FOUND)
            .header(header::LOCATION, "/file.bin")
            .body(Body::empty())
            .unwrap();
    }
    if path == "/status/503" {
        return plain(StatusCode::SERVICE_UNAVAILABLE);
    }
    if path != "/file.bin" && !path.starts_with("/signed/") {
        return plain(StatusCode::NOT_FOUND);
    }
    let cfg = sh.cfg.lock().unwrap().clone();
    if cfg.stall_headers {
        std::future::pending::<()>().await;
    }
    let version = sh.version.load(Ordering::SeqCst);
    let is_head = match *req.method() {
        Method::GET => false,
        Method::HEAD if cfg.head_allowed => true,
        _ => return plain(StatusCode::METHOD_NOT_ALLOWED),
    };
    if !is_head && cfg.get_503_once && !sh.get_503_used.swap(true, Ordering::SeqCst) {
        return plain(StatusCode::SERVICE_UNAVAILABLE);
    }
    let range_hdr = req
        .headers()
        .get(header::RANGE)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    if !is_head && range_hdr.is_some() {
        *sh.range_requests.lock().unwrap() += 1;
    }

    let etag = etag_for(version);
    let lm = last_modified_for(version);
    let if_range_ok = match req
        .headers()
        .get(header::IF_RANGE)
        .and_then(|v| v.to_str().ok())
    {
        None => true,
        Some(v) => (cfg.etag && v == etag) || (cfg.last_modified && v == lm),
    };

    let mut builder = Response::builder();
    if cfg.etag {
        builder = builder.header(header::ETAG, &etag);
    }
    if cfg.last_modified {
        builder = builder.header(header::LAST_MODIFIED, &lm);
    }
    if let Some(d) = &cfg.disposition {
        builder = builder.header(
            header::CONTENT_DISPOSITION,
            HeaderValue::from_str(d).unwrap(),
        );
    }
    if cfg.ranges || cfg.ranges_lie {
        builder = builder.header(header::ACCEPT_RANGES, "bytes");
    }

    let mut status = StatusCode::OK;
    let (mut start, mut end) = (0, cfg.len.saturating_sub(1));
    if cfg.ranges
        && !cfg.ranges_lie
        && if_range_ok
        && let Some(parsed) = range_hdr.as_deref().and_then(|v| parse_range(v, cfg.len))
    {
        match parsed {
            Ok((s, e)) => {
                status = StatusCode::PARTIAL_CONTENT;
                (start, end) = (s, e);
                builder =
                    builder.header(header::CONTENT_RANGE, format!("bytes {s}-{e}/{}", cfg.len));
            }
            Err(()) => {
                return Response::builder()
                    .status(StatusCode::RANGE_NOT_SATISFIABLE)
                    .header(header::CONTENT_RANGE, format!("bytes */{}", cfg.len))
                    .body(Body::empty())
                    .unwrap();
            }
        }
    }
    let body_len = if cfg.len == 0 { 0 } else { end - start + 1 };
    if is_head && cfg.head_zero_length {
        builder = builder.header(header::CONTENT_LENGTH, 0);
    } else if cfg.content_length {
        builder = builder.header(header::CONTENT_LENGTH, body_len);
    }
    builder = builder.status(status);
    if is_head {
        // An unsized empty stream keeps hyper from inventing `Content-Length: 0`.
        let body = if cfg.content_length || cfg.head_zero_length {
            Body::empty()
        } else {
            Body::from_stream(futures_util::stream::empty::<Result<Bytes, std::io::Error>>())
        };
        return builder.body(body).unwrap();
    }

    if let Some(enc) = &cfg.content_encoding {
        builder = builder.header(header::CONTENT_ENCODING, enc.as_str());
    }
    let bps = match cfg.slow_range_start {
        Some(at) if status == StatusCode::PARTIAL_CONTENT && start == at => cfg.slow_bytes_per_sec,
        _ => cfg.bytes_per_sec,
    };
    let fail_at = match cfg.fail_after_bytes_once {
        Some(n) if body_len > n && !sh.fail_used.swap(true, Ordering::SeqCst) => Some(n),
        _ => None,
    };
    builder
        .body(Body::from_stream(body_stream(
            start, body_len, version, bps, fail_at,
        )))
        .unwrap()
}

fn body_stream(
    start: u64,
    len: u64,
    version: u32,
    bps: Option<u64>,
    fail_at: Option<u64>,
) -> impl futures_util::Stream<Item = Result<Bytes, std::io::Error>> {
    const CHUNK: u64 = 16 * 1024;
    futures_util::stream::unfold((0u64, false), move |(sent, done)| async move {
        if done || sent >= len {
            return None;
        }
        let mut n = CHUNK.min(len - sent);
        if let Some(f) = fail_at {
            if sent >= f {
                // Let hyper flush the bytes already queued before the connection is aborted.
                tokio::time::sleep(Duration::from_millis(50)).await;
                let err = std::io::Error::new(std::io::ErrorKind::ConnectionReset, "injected");
                return Some((Err(err), (sent, true)));
            }
            n = n.min(f - sent);
        }
        if let Some(bps) = bps.filter(|b| *b > 0) {
            tokio::time::sleep(Duration::from_secs_f64(n as f64 / bps as f64)).await;
        }
        let chunk: Vec<u8> = (0..n)
            .map(|i| body_byte(start + sent + i, version))
            .collect();
        Some((Ok(Bytes::from(chunk)), (sent + n, false)))
    })
}
