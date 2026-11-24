//! A fake GitHub releases API plus asset downloads, for self-update tests.

use axum::Router;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::io::Write;
use std::sync::Arc;
use std::sync::atomic::{AtomicU16, AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;

/// What a fake release contains. Built with [`FakeRelease::new`] and the builder options.
#[derive(Debug, Clone)]
pub struct FakeRelease {
    version: String,
    target: String,
    checksum: Checksum,
    prints: Option<String>,
    no_binary: bool,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Checksum {
    Good,
    Bad,
    Missing,
}

impl FakeRelease {
    /// A release `v<version>` with an archive and checksum for `target`. The archive
    /// holds a stand-in `puppy` that prints `puppy <version>`.
    pub fn new(version: &str, target: &str) -> Self {
        Self {
            version: version.to_string(),
            target: target.to_string(),
            checksum: Checksum::Good,
            prints: None,
            no_binary: false,
        }
    }

    /// The archive holds everything but the `puppy` binary.
    pub fn without_binary(mut self) -> Self {
        self.no_binary = true;
        self
    }

    /// Publishes no `.sha256` asset.
    pub fn without_checksum(mut self) -> Self {
        self.checksum = Checksum::Missing;
        self
    }

    /// Publishes a `.sha256` that does not match the archive.
    pub fn bad_checksum(mut self) -> Self {
        self.checksum = Checksum::Bad;
        self
    }

    /// The stand-in binary prints `text` instead of `puppy <version>`.
    pub fn binary_prints(mut self, text: &str) -> Self {
        self.prints = Some(text.to_string());
        self
    }

    /// Whether this target gets a zip (Windows) rather than a tar.gz.
    fn is_zip(&self) -> bool {
        self.target.contains("windows")
    }

    fn archive_name(&self) -> String {
        let ext = if self.is_zip() { "zip" } else { "tar.gz" };
        format!("puppy-{}-{}.{ext}", self.version, self.target)
    }

    fn binary(&self) -> Vec<u8> {
        let text = self
            .prints
            .clone()
            .unwrap_or_else(|| format!("puppy {}", self.version));
        if self.is_zip() {
            // Not runnable; Windows tests needing a runnable stand-in are unix-only.
            format!("MZ {text}").into_bytes()
        } else {
            format!("#!/bin/sh\necho '{text}'\n").into_bytes()
        }
    }

    /// Files beside the binary, like the real packages, plus one hostile path.
    fn extras(&self) -> Vec<(&'static str, &'static [u8], bool)> {
        // (path relative to the release folder, body, hostile: path escapes the folder)
        vec![
            ("README.md", b"readme", false),
            ("LICENSE-MIT", b"license", false),
            ("extension/manifest.json", b"{}", false),
            ("packaging/systemd/puppy.service", b"[Service]", false),
            ("../evil", b"evil", true),
        ]
    }

    fn archive(&self) -> Vec<u8> {
        let dir = format!("puppy-{}-{}", self.version, self.target);
        let bin = self.binary();
        let mut files: Vec<(String, Vec<u8>, u32)> = Vec::new();
        for (path, body, hostile) in self.extras() {
            let full = if hostile {
                path.to_string()
            } else {
                format!("{dir}/{path}")
            };
            files.push((full, body.to_vec(), 0o644));
        }
        if !self.no_binary {
            let name = if self.is_zip() { "puppy.exe" } else { "puppy" };
            files.insert(0, (format!("{dir}/{name}"), bin, 0o755));
        }
        if self.is_zip() {
            let mut out = std::io::Cursor::new(Vec::new());
            let mut zip = zip::ZipWriter::new(&mut out);
            let opts = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Deflated);
            for (name, body, _) in &files {
                zip.start_file(name.as_str(), opts).unwrap();
                zip.write_all(body).unwrap();
            }
            zip.finish().unwrap();
            out.into_inner()
        } else {
            let gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
            let mut tar = tar::Builder::new(gz);
            for (name, body, mode) in &files {
                let mut header = tar::Header::new_gnu();
                header.set_size(body.len() as u64);
                header.set_mode(*mode);
                assert!(
                    name.len() <= 100,
                    "tar entry name too long for the fake ({} bytes): {name}",
                    name.len()
                );
                // Written raw: `set_path` refuses `..` components.
                let raw = &mut header.as_gnu_mut().unwrap().name;
                raw[..name.len()].copy_from_slice(name.as_bytes());
                header.set_cksum();
                tar.append(&header, body.as_slice()).unwrap();
            }
            tar.into_inner().unwrap().finish().unwrap()
        }
    }
}

struct Shared {
    status: AtomicU16,
    hits: AtomicUsize,
    delay_ms: AtomicU64,
    /// Archive downloads: how many started, and how long each takes to answer.
    downloads: AtomicUsize,
    download_delay_ms: AtomicU64,
    json: String,
    assets: HashMap<String, Vec<u8>>,
}

/// A running fake GitHub on 127.0.0.1.
pub struct FakeGitHub {
    shared: Arc<Shared>,
    base: url::Url,
    task: tokio::task::JoinHandle<()>,
}

impl FakeGitHub {
    pub async fn start(release: FakeRelease) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = url::Url::parse(&format!("http://{}", listener.local_addr().unwrap())).unwrap();

        let name = release.archive_name();
        let archive = release.archive();
        let mut assets = HashMap::new();
        let mut listed = vec![name.clone()];
        match release.checksum {
            Checksum::Missing => {}
            c => {
                let hex = if c == Checksum::Good {
                    hex(&Sha256::digest(&archive))
                } else {
                    "0".repeat(64)
                };
                let sum_name = format!("{name}.sha256");
                assets.insert(sum_name.clone(), format!("{hex}  {name}\n").into_bytes());
                listed.push(sum_name);
            }
        }
        assets.insert(name, archive);
        let json = serde_json::json!({
            "tag_name": format!("v{}", release.version),
            "assets": listed.iter().map(|n| serde_json::json!({
                "name": n,
                "browser_download_url": base.join(&format!("download/{n}")).unwrap(),
            })).collect::<Vec<_>>(),
        })
        .to_string();

        let shared = Arc::new(Shared {
            status: AtomicU16::new(200),
            hits: AtomicUsize::new(0),
            delay_ms: AtomicU64::new(0),
            downloads: AtomicUsize::new(0),
            download_delay_ms: AtomicU64::new(0),
            json,
            assets,
        });
        let app = Router::new()
            .route("/repos/lavavrik/puppy/releases/latest", get(latest))
            .route("/download/{name}", get(download))
            .with_state(shared.clone());
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Self { shared, base, task }
    }

    /// The URL to pass as the API base.
    pub fn api_base(&self) -> url::Url {
        self.base.clone()
    }

    /// How many release-lookup requests the server has seen.
    pub fn hits(&self) -> usize {
        self.shared.hits.load(Ordering::SeqCst)
    }

    /// Makes the release lookup answer with this status.
    pub fn set_status(&self, status: u16) {
        self.shared.status.store(status, Ordering::SeqCst);
    }

    /// Makes the release lookup answer only after `delay` (counted as a hit at once).
    pub fn set_delay(&self, delay: Duration) {
        self.shared
            .delay_ms
            .store(delay.as_millis() as u64, Ordering::SeqCst);
    }
}

impl FakeGitHub {
    /// How many downloads of the release archive (not its checksum) have started.
    pub fn downloads(&self) -> usize {
        self.shared.downloads.load(Ordering::SeqCst)
    }

    /// Makes archive downloads answer only after `delay` (counted at once).
    pub fn set_download_delay(&self, delay: Duration) {
        self.shared
            .download_delay_ms
            .store(delay.as_millis() as u64, Ordering::SeqCst);
    }
}

impl Drop for FakeGitHub {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

async fn latest(State(s): State<Arc<Shared>>, headers: HeaderMap) -> Response {
    s.hits.fetch_add(1, Ordering::SeqCst);
    let delay = s.delay_ms.load(Ordering::SeqCst);
    if delay > 0 {
        tokio::time::sleep(Duration::from_millis(delay)).await;
    }
    // GitHub rejects requests without a User-Agent.
    if !headers.contains_key(header::USER_AGENT) {
        return StatusCode::FORBIDDEN.into_response();
    }
    let status = StatusCode::from_u16(s.status.load(Ordering::SeqCst)).unwrap();
    if !status.is_success() {
        return (status, "{\"message\":\"nope\"}").into_response();
    }
    ([(header::CONTENT_TYPE, "application/json")], s.json.clone()).into_response()
}

async fn download(State(s): State<Arc<Shared>>, Path(name): Path<String>) -> Response {
    if !name.ends_with(".sha256") {
        s.downloads.fetch_add(1, Ordering::SeqCst);
        let delay = s.download_delay_ms.load(Ordering::SeqCst);
        if delay > 0 {
            tokio::time::sleep(Duration::from_millis(delay)).await;
        }
    }
    match s.assets.get(&name) {
        Some(body) => body.clone().into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}
