# puppy Plan 1 — Core types, Engine trait & HTTP engine

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A tested Rust library crate (`puppy-core`) providing the shared download types, the `Engine` trait, and a complete multi-connection HTTP/HTTPS engine with crash-safe resume.

**Architecture:** Cargo workspace; this plan creates only `crates/puppy-core`. The HTTP engine is a set of small modules (probe → segment plan → per-segment workers → orchestrator) behind the `Engine` trait the daemon (Plan 2) will consume. Resume state lives in a sidecar file next to the `.part` file, owned by the engine.

**Tech Stack:** Rust 1.99 stable, tokio 1.54, reqwest 0.13 (default features = rustls; plus `stream`), async-trait, serde/serde_json, thiserror 2, url, tracing. Tests: axum 0.8 local server, tempfile 3.

**Spec:** `docs/superpowers/specs/2026-11-19-puppy-design.md`

**Plan series** (each produces working, testable software):
1. **This plan:** core + HTTP engine (library).
2. Daemon: protocol crate, local-socket server (Unix socket / Windows named pipe), queue, persistence, lifecycle, global rate-limit division.
3. TUI dashboard (ratatui) as a client of the daemon.
4. Torrent engine (librqbit) behind `Engine`.

## Global Constraints

- **Cross-platform:** Linux, macOS, Windows on x86_64 and aarch64. Platform-specific code only in small `#[cfg]`-gated functions (`fsutil.rs`); everything else is portable. Tests must pass on all three OSes (CI matrix, Task 1).
- Rust stable, edition 2024; `cargo clippy --all-targets -- -D warnings` and `cargo fmt --check` clean at every commit.
- HTTPS via rustls only (reqwest 0.13 default TLS); no native-tls / OpenSSL. reqwest's default rustls provider is aws-lc-rs (needs a C toolchain + cmake); if it fails to build on any of the six targets, switch to `default-features = false, features = ["rustls-no-provider", "http2", "system-proxy", "stream"]` with the `ring` provider installed at startup.
- Never request compressed transfer: send `Accept-Encoding: identity`; do not enable reqwest `gzip`/`brotli`/`deflate`/`zstd` features (byte ranges must address the raw file).
- Default connections per download: **8**. Retries with exponential backoff; fail only after a configurable retry budget.
- Download writes to a preallocated `<name>.part`; renamed to `<name>` on completion.
- Files holding request context (cookies/auth) are created with mode **0600** on Unix; on Windows no mode is applied (per-user data dirs carry a user-only ACL).
- Positional file writes go through one helper (`fsutil::write_at`): Unix `FileExt::write_all_at`, Windows `FileExt::seek_write` in a loop. Directory fsync is Unix-only (no-op on Windows).
- On Windows a file cannot be renamed or deleted while any handle is open: close every handle to `.part` (workers, flusher) before rename/delete.
- All state writes are atomic (write temp file → fsync → rename).
- A file is never marked complete unless all bytes are written and fsynced; a resumed file is never silently wrong — mismatch ⇒ `NeedsAttention`.
- Redirects are followed; the final URL is recorded and used for segment requests.
- Honour `HTTP_PROXY`/`HTTPS_PROXY`/`NO_PROXY` (reqwest `system-proxy` default) and an optional explicit proxy URL from config.

## Review Focus

1. **Server advertises `Accept-Ranges: bytes` but answers a ranged GET with `200` + full body** → engine must detect it (status ≠ 206 or missing/incorrect `Content-Range`) and fall back to a single stream from offset 0, never write a full body at a segment offset. Test: Task 6 `worker_rejects_200_for_range`, Task 7 `ranges_lied_falls_back_to_single`.
2. **Unknown size** (no `Content-Length`, chunked) → single stream, `total_bytes = None`, completes at EOF, not resumable (restore restarts from zero with a visible reason). Test: Task 7 `unknown_length_single_stream`.
3. **Hostile or odd filename** from `Content-Disposition`/URL (`../../.bashrc`, `/etc/passwd`, empty, `%2F`, overly long) → sanitised to a safe basename inside the output dir; name collisions get ` (1)`, ` (2)`… Test: Task 1 `sanitize_filename_*`, `unique_path_*`.
4. **Disk error mid-download** (write fails, e.g. ENOSPC) → download goes `Failed { reason }` promptly, is not retried as a network error. Test: Task 6 `worker_disk_error_is_fatal`.
5. **Crash between data write and sidecar flush** → sidecar `done` counters never exceed bytes actually fsynced to `.part` (fsync data before writing sidecar). Test: Task 3 `flush_order_data_before_sidecar` + Task 8 `restore_after_kill_completes_correctly`.
6. **Windows: rename/delete while a handle is open** → completion and `remove(delete_files=true)` must drop all `.part` handles first; otherwise the download fails only on Windows. Test: Task 7 `downloads_multi_segment` and Task 9 `engine_remove_deletes_files` (run on Windows CI).

---

## File Structure

```
Cargo.toml                         workspace
crates/puppy-core/
  Cargo.toml
  src/lib.rs                       module exports
  src/types.rs                     DownloadId, Source, AddRequest, RequestContext, DownloadState, DownloadStatus, EngineDetail, …
  src/engine.rs                    Engine trait, EngineEvent, EngineError, StartMode, Added
  src/config.rs                    HttpConfig (defaults)
  src/fsutil.rs                    sanitize_filename, unique_path, atomic_write
  src/limiter.rs                   TokenBucket (shared, async, adjustable rate)
  src/meter.rs                     SpeedMeter (EWMA bytes/s)
  src/http/mod.rs
  src/http/probe.rs                probe() → RemoteInfo
  src/http/segments.rs             plan_segments, steal_split
  src/http/resume.rs               HttpResumeState sidecar
  src/http/worker.rs               fetch one segment range with retries
  src/http/download.rs             HttpDownload orchestrator (one download)
  src/http/validate.rs             restore validation incl. spot check
  src/http/engine.rs               HttpEngine: impl Engine
  tests/support/mod.rs             axum TestServer
  tests/http_*.rs                  integration tests
```

---

### Task 1: Workspace, core types, filesystem utilities

**Files:**
- Create: `.github/workflows/ci.yml`, `Cargo.toml`, `.gitignore` (`/target`), `crates/puppy-core/Cargo.toml`, `src/lib.rs`, `src/types.rs`, `src/engine.rs`, `src/config.rs`, `src/fsutil.rs`
- Test: inline `#[cfg(test)]` in `fsutil.rs`, `types.rs`

**Interfaces — Produces:**
```rust
// types.rs  (all: Debug, Clone, Serialize, Deserialize; enums use #[serde(tag = "kind", rename_all = "snake_case")])
pub type DownloadId = u64;
pub enum Source { Http { url: url::Url }, Magnet { uri: String }, TorrentFile { path: PathBuf } }
pub struct RequestContext { pub headers: Vec<(String, String)>, pub cookies: Option<String>,
    pub referer: Option<String>, pub user_agent: Option<String>,
    pub basic_auth: Option<(String, Option<String>)> }            // Default
pub struct AddRequest { pub source: Source, pub output_dir: Option<PathBuf>,
    pub filename: Option<String>, pub context: RequestContext }
pub enum AttentionReason { RemoteChanged, NotResumable, ValidationFailed { detail: String } }
pub enum DownloadState { Queued, Starting, Active, Paused, Completed,
    Failed { reason: String }, NeedsAttention { reason: AttentionReason } }
pub struct DownloadStatus { pub state: DownloadState, pub name: String,
    pub total_bytes: Option<u64>, pub done_bytes: u64, pub down_bps: u64, pub up_bps: u64,
    pub eta_secs: Option<u64> }
pub struct SegmentView { pub start: u64, pub end: u64 /*exclusive*/, pub done: u64, pub bps: u64, pub active: bool }
pub enum EngineDetail {
    Http { segments: Vec<SegmentView>, connections: u16, resumable: bool },
    Torrent { piece_count: u32, have: Vec<u8> /*bitfield, MSB first*/, peers: u32, seeds: u32,
              leechers: u32, ratio: f32 } }

// engine.rs
pub enum EngineKind { Http, Torrent }
pub enum StartMode { Fresh, Restore { path: PathBuf } }
pub struct Added { pub name: String, pub path: PathBuf, pub total_bytes: Option<u64> }
pub enum EngineEvent { StateChanged { id: DownloadId, state: DownloadState }, Completed { id: DownloadId } }
#[derive(thiserror::Error)] pub enum EngineError { NotFound(DownloadId), Unsupported(&'static str),
    Network(String), Io(#[from] std::io::Error), Http { status: u16 }, Invalid(String) }
#[async_trait::async_trait]
pub trait Engine: Send + Sync + 'static {
    fn kind(&self) -> EngineKind;
    fn accepts(&self, source: &Source) -> bool;
    /// Probes/prepares, starts transferring in the background, returns once name/path are known.
    async fn add(&self, id: DownloadId, req: AddRequest, mode: StartMode) -> Result<Added, EngineError>;
    async fn pause(&self, id: DownloadId) -> Result<(), EngineError>;
    async fn resume(&self, id: DownloadId) -> Result<(), EngineError>;
    async fn remove(&self, id: DownloadId, delete_files: bool) -> Result<(), EngineError>;
    fn status(&self, id: DownloadId) -> Option<DownloadStatus>;
    fn detail(&self, id: DownloadId) -> Option<EngineDetail>;
    fn set_download_limit(&self, id: DownloadId, bps: Option<u64>) -> Result<(), EngineError>;
    /// Engine-wide download budget assigned by the daemon's global limiter.
    fn set_global_download_budget(&self, bps: Option<u64>);
    fn subscribe(&self) -> tokio::sync::broadcast::Receiver<EngineEvent>;
}

// config.rs
pub struct HttpConfig { pub connections: u16 /*8*/, pub min_segment_bytes: u64 /*1 MiB*/,
    pub max_retries: u32 /*8*/, pub backoff_base_ms: u64 /*500*/, pub backoff_max_ms: u64 /*30_000*/,
    pub flush_interval_ms: u64 /*1000*/, pub spot_check_bytes: u64 /*64 KiB*/,
    pub proxy: Option<url::Url>, pub default_dir: PathBuf }   // impl Default (default_dir = dirs::download_dir() or ~/Downloads)

// fsutil.rs
pub fn sanitize_filename(raw: &str) -> String;              // basename only, strip control chars and '/', '\\', leading dots-only names → "download"; max 200 bytes (char-boundary safe)
pub fn unique_path(dir: &Path, name: &str) -> PathBuf;      // avoids existing `name` and `name.part`; "a.iso" → "a (1).iso"
pub async fn atomic_write(path: &Path, bytes: &[u8], private: bool) -> std::io::Result<()>; // tmp in same dir, fsync, rename (replace), fsync dir on Unix; private ⇒ 0600 on Unix
pub fn write_at(file: &std::fs::File, buf: &[u8], offset: u64) -> std::io::Result<()>;     // cfg(unix) write_all_at / cfg(windows) seek_write loop
```

- [ ] **Step 1: Write failing tests** in `fsutil.rs`:
  - `sanitize_filename_strips_traversal`: `"../../.bashrc"` → `".bashrc"`; `"/etc/passwd"` → `"passwd"`; `"a%2Fb"` (already-decoded `"a/b"`) → `"b"`.
  - `sanitize_filename_empty_and_dots`: `""`, `"."`, `".."` → `"download"`.
  - `sanitize_filename_truncates`: 300×`"é"` + `".iso"` → ≤200 bytes, valid UTF-8, ends with `".iso"`.
  - `unique_path_appends_counter`: with `a.iso` existing → `a (1).iso`; with `a (1).iso.part` also existing → `a (2).iso`.
  - `atomic_write_replaces`: writing twice leaves the second contents (exercises replace-on-rename on Windows).
  - `atomic_write_sets_mode` (`#[cfg(unix)]`): `private=true` ⇒ mode `0o600`.
  - `write_at_offsets`: writes `b"bb"` at 2 then `b"aa"` at 0 into a 4-byte file → `b"aabb"`.
  - In `types.rs`: `download_state_serde_roundtrip` for `NeedsAttention { RemoteChanged }` → JSON `{"kind":"needs_attention","reason":{"kind":"remote_changed"}}`.
- [ ] **Step 2:** `cargo test -p puppy-core` → FAIL (unresolved items).
- [ ] **Step 3:** Implement the types, trait, config and fsutil above. Add deps: tokio (full), reqwest (`stream`), async-trait, serde (derive), serde_json, thiserror, url (serde), tracing, dirs; dev: axum, tempfile, tokio (test-util).
- [ ] **Step 4:** `cargo test -p puppy-core` → PASS; clippy/fmt clean.
- [ ] **Step 5:** Write `.github/workflows/ci.yml`: on push/PR, matrix `os: [ubuntu-latest, ubuntu-24.04-arm, macos-latest, macos-15-intel (or the current Intel macOS runner label), windows-latest, windows-11-arm]`, steps: checkout, `dtolnay/rust-toolchain@stable` (components clippy, rustfmt), `Swatinem/rust-cache@v2`, `cargo fmt --check` (ubuntu only), `cargo clippy --all-targets -- -D warnings`, `cargo test --workspace`. Not run locally; validated when the repo is pushed.
- [ ] **Step 6:** `git add -A && git commit -m "feat(core): workspace, core types, Engine trait, fs utils, CI matrix"`

---

### Task 2: Segment planning and work-stealing split

**Files:** Create `src/http/mod.rs`, `src/http/segments.rs` (inline tests)

**Interfaces — Produces:**
```rust
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Segment { pub start: u64, pub end: u64 /*exclusive*/, pub done: u64 /*bytes completed from start*/ }
impl Segment { pub fn remaining(&self) -> u64; pub fn is_complete(&self) -> bool; pub fn cursor(&self) -> u64 /*start+done*/ }
pub fn plan_segments(total: u64, connections: u16, min_segment: u64) -> Vec<Segment>;
/// Shrinks `victim` and returns a new segment covering the back half of its remaining range,
/// or None if remaining < 2*min_segment.
pub fn steal_split(victim: &mut Segment, min_segment: u64) -> Option<Segment>;
```

- [ ] **Step 1: Failing tests:**
  - `plan_even_split`: `plan_segments(800, 8, 1)` → 8 segments of 100, contiguous, last `end == 800`.
  - `plan_respects_min_segment`: `plan_segments(3 MiB, 8, 1 MiB)` → 3 segments.
  - `plan_small_file_single`: `plan_segments(10, 8, 1 MiB)` → 1 segment `0..10`.
  - `plan_zero_length`: `plan_segments(0, 8, 1)` → empty vec.
  - `plan_remainder_goes_to_last`: `plan_segments(803, 8, 1)` covers exactly `0..803`, no gaps/overlap.
  - `steal_split_halves_remaining`: victim `0..1000, done 200`, min 100 → victim `0..600`, new `600..1000 done 0`.
  - `steal_split_refuses_small`: victim remaining 150, min 100 → None, victim unchanged.
- [ ] **Step 2:** `cargo test -p puppy-core segments` → FAIL.
- [ ] **Step 3:** Implement. Split midpoint = `cursor + remaining/2`.
- [ ] **Step 4:** Tests PASS.
- [ ] **Step 5:** Commit `feat(http): segment planning and work-stealing split`.

---

### Task 3: Resume sidecar

**Files:** Create `src/http/resume.rs` (inline tests)

**Interfaces — Produces:**
```rust
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HttpResumeState { pub version: u32 /*1*/, pub request: AddRequest, pub final_url: url::Url,
    pub total: Option<u64>, pub etag: Option<String>, pub etag_strong: bool,
    pub last_modified: Option<String>, pub final_path: PathBuf, pub segments: Vec<Segment> }
pub fn part_path(final_path: &Path) -> PathBuf;       // "<final>.part"
pub fn sidecar_path(final_path: &Path) -> PathBuf;    // "<final>.part.puppy"
impl HttpResumeState {
    pub async fn load(final_path: &Path) -> Result<Self, EngineError>;          // Invalid on unknown version/corrupt
    /// fsyncs `part_file` first, then atomic_write(sidecar, json, private = true).
    pub async fn flush(&self, part_file: &std::fs::File) -> Result<(), EngineError>;
}
```

- [ ] **Step 1: Failing tests:**
  - `sidecar_roundtrip`: flush then load returns equal state; on Unix sidecar mode `0o600`.
  - `load_rejects_unknown_version`: version 99 → `EngineError::Invalid`.
  - `load_rejects_corrupt_json`: garbage → `Invalid`.
  - `flush_order_data_before_sidecar`: implement `flush` via a private `flush_with(&self, sync: impl FnOnce() -> Fut, write: …)` seam; test records call order → `["sync_data", "write_sidecar"]`.
- [ ] **Step 2:** FAIL. **Step 3:** Implement. **Step 4:** PASS.
- [ ] **Step 5:** Commit `feat(http): crash-safe resume sidecar`.

---

### Task 4: Test server + probe

**Files:** Create `tests/support/mod.rs`, `src/http/probe.rs`, `tests/http_probe.rs`

**Interfaces — Produces:**
```rust
// tests/support/mod.rs (test-only)
pub fn body_byte(i: u64, version: u32) -> u8 { ((i * 31 + 7 + version as u64 * 13) % 251) as u8 }
pub struct ServerCfg { pub len: u64, pub ranges: bool, pub etag: bool, pub last_modified: bool,
    pub content_length: bool, pub head_allowed: bool, pub ranges_lie: bool /*advertise but send 200*/,
    pub disposition: Option<String>, pub bytes_per_sec: Option<u64>,
    pub fail_after_bytes_once: Option<u64> /*drop connection once*/ }   // Default: len 4 MiB, ranges/etag/lm/cl/head true
pub struct TestServer { /* axum on 127.0.0.1:0 */ }
impl TestServer { pub async fn start(cfg: ServerCfg) -> Self; pub fn url(&self, path: &str) -> url::Url;
    pub fn set_version(&self, v: u32) /*changes body + ETag + Last-Modified*/;
    pub fn range_requests(&self) -> usize; pub fn set_cfg(&self, f: impl FnOnce(&mut ServerCfg)); }
// Route "/file.bin" serves the body; "/redirect" → 302 to "/file.bin". Strong ETag = "\"v{version}\"".

// src/http/probe.rs
pub struct RemoteInfo { pub final_url: url::Url, pub total: Option<u64>, pub accepts_ranges: bool,
    pub etag: Option<String>, pub etag_strong: bool, pub last_modified: Option<String>, pub filename: String }
pub fn build_client(cfg: &HttpConfig) -> Result<reqwest::Client, EngineError>;   // identity encoding, redirect limit 10, proxy, UA "puppy/<version>"
pub fn apply_context(rb: reqwest::RequestBuilder, ctx: &RequestContext) -> reqwest::RequestBuilder;
pub async fn probe(client: &reqwest::Client, url: &url::Url, ctx: &RequestContext) -> Result<RemoteInfo, EngineError>;
```
Probe: HEAD; if HEAD returns 405/501/error or lacks length, fall back to `GET` with `Range: bytes=0-0` and read `Content-Range: bytes 0-0/<total>` (206 ⇒ ranges ok). Filename precedence: `Content-Disposition` (`filename*` RFC 5987 over `filename`) → last non-empty URL path segment (percent-decoded) → `"download"`; always passed through `sanitize_filename`. `etag_strong` = ETag present and not prefixed `W/`.

- [ ] **Step 1: Failing tests** in `tests/http_probe.rs`:
  - `probe_basic`: total `4 MiB`, `accepts_ranges`, strong etag `"\"v0\""`, filename `"file.bin"`.
  - `probe_follows_redirect`: `/redirect` → `final_url` path `/file.bin`.
  - `probe_head_not_allowed_falls_back`: `head_allowed=false` → same total and ranges via range GET.
  - `probe_no_ranges`: `ranges=false` → `accepts_ranges == false`.
  - `probe_disposition_filename`: `disposition = Some("attachment; filename*=UTF-8''r%C3%A9sum%C3%A9.pdf")` → `"résumé.pdf"`; `"attachment; filename=\"../../x.sh\""` → `"x.sh"`.
  - `probe_sends_context`: context with header `X-Test: 1`, cookie `a=b`, referer → server records them on the request.
- [ ] **Step 2:** FAIL. **Step 3:** Implement server + probe. **Step 4:** PASS.
- [ ] **Step 5:** Commit `feat(http): probe with HEAD fallback and test server`.

---

### Task 5: Token-bucket limiter and speed meter

**Files:** Create `src/limiter.rs`, `src/meter.rs` (inline tests, `tokio::time::pause`)

**Interfaces — Produces:**
```rust
#[derive(Clone)] pub struct TokenBucket(/* Arc<Mutex<..>> */);
impl TokenBucket { pub fn new(bps: Option<u64>) -> Self /*None = unlimited*/;
    pub fn set_rate(&self, bps: Option<u64>);
    pub async fn acquire(&self, n: u64); }               // burst capacity = max(bps / 4, 16 KiB)
pub struct SpeedMeter { /* EWMA, tau = 2 s */ }
impl SpeedMeter { pub fn new() -> Self; pub fn record(&mut self, bytes: u64, now: Instant); pub fn bps(&self, now: Instant) -> u64; }
```
Each chunk is acquired against **both** the per-download bucket and the engine-global bucket (Task 7).

- [ ] **Step 1: Failing tests:**
  - `unlimited_never_waits`: 100 MB acquired with no virtual time elapsing.
  - `limits_rate`: rate 1000 B/s, acquire 5000 total in 500-byte chunks → elapsed virtual time ∈ [3.75 s, 5.25 s].
  - `set_rate_takes_effect`: switching from 1000 to unlimited mid-wait unblocks promptly.
  - `meter_converges`: 1000 B every 100 ms for 10 s → `bps` within 10% of 10_000; after 10 s idle → < 500.
- [ ] **Step 2:** FAIL. **Step 3:** Implement. **Step 4:** PASS.
- [ ] **Step 5:** Commit `feat(core): token bucket limiter and speed meter`.

---

### Task 6: Segment worker

**Files:** Create `src/http/worker.rs`, `tests/http_worker.rs`

**Interfaces — Consumes:** `Segment`, `apply_context`, `TokenBucket`, `HttpConfig`.
**Produces:**
```rust
pub struct SegmentShared { pub done: AtomicU64, pub end: AtomicU64 /*may shrink via steal*/ , pub bps: AtomicU64, pub active: AtomicBool }
pub struct WorkerCtx { pub client: reqwest::Client, pub url: url::Url, pub context: RequestContext,
    pub if_range: Option<String> /*strong ETag or Last-Modified*/, pub expected_total: Option<u64>,
    pub file: Arc<std::fs::File> /* positional writes via fsutil::write_at inside spawn_blocking */,
    pub limiters: [TokenBucket; 2], pub cfg: HttpConfig, pub cancel: tokio_util::sync::CancellationToken }
pub enum WorkerExit { Done, Cancelled, RemoteChanged, RangeUnsupported, Fatal(String) }
pub async fn run_segment(ctx: &WorkerCtx, start: u64, seg: Arc<SegmentShared>) -> WorkerExit;
```
Behaviour: request `Range: bytes=<start+done>-<end-1>` (+ `If-Range`). Response 206 with `Content-Range` start matching and total == `expected_total` → stream, writing each chunk at its offset, bump `done`; stop when `start+done >= end` (re-read `end` each chunk — stealing may shrink it; truncate the final chunk to `end`). 200 when `if_range` was sent ⇒ `RemoteChanged`. 200 without `if_range`, or wrong `Content-Range` ⇒ `RangeUnsupported`. 206 with different total ⇒ `RemoteChanged`. Network errors / 5xx / 429 ⇒ retry with backoff `min(base * 2^attempt, max)` + jitter up to `max_retries`, then `Fatal`. 4xx (except 408/429) ⇒ `Fatal("HTTP <code>")`. Any file write error ⇒ `Fatal(io error text)` immediately, no retry.

- [ ] **Step 1: Failing tests:**
  - `worker_downloads_range`: segment `1000..5000` → file bytes 1000..5000 equal `body_byte(i,0)`; `done == 4000`; `Done`.
  - `worker_retries_after_drop`: `fail_after_bytes_once = Some(1500)` → `Done`, bytes correct, ≥2 range requests.
  - `worker_detects_remote_change`: `if_range = "\"v0\""`, server at version 1 → `RemoteChanged`.
  - `worker_rejects_200_for_range`: `ranges_lie = true` → `RangeUnsupported`, nothing written past range.
  - `worker_honours_shrunk_end`: set `seg.end` to 3000 while running (slow server 64 KiB/s) → stops at 3000.
  - `worker_disk_error_is_fatal`: file opened read-only → `Fatal(_)` with 1 request (no retries).
  - `worker_cancel`: cancel token mid-transfer → `Cancelled` within 200 ms.
- [ ] **Step 2:** FAIL. **Step 3:** Implement. **Step 4:** PASS.
- [ ] **Step 5:** Commit `feat(http): segment worker with retries and change detection`.

---

### Task 7: Download orchestrator (fresh downloads, pause/resume, stealing, fallback)

**Files:** Create `src/http/download.rs`, `tests/http_download.rs`

**Interfaces — Consumes:** `probe`, `plan_segments`, `steal_split`, `run_segment`, `HttpResumeState`, `SpeedMeter`, `TokenBucket`, `unique_path`.
**Produces:**
```rust
pub struct HttpDownload { /* Arc inner */ }
impl HttpDownload {
    pub async fn start_fresh(id: DownloadId, req: AddRequest, client: reqwest::Client, cfg: HttpConfig,
        global: TokenBucket, events: broadcast::Sender<EngineEvent>) -> Result<(Self, Added), EngineError>;
    pub async fn pause(&self);          // cancel workers, await them, flush sidecar, state Paused
    pub async fn resume(&self) -> Result<(), EngineError>;   // re-runs remaining segments (validation in Task 8)
    pub async fn cancel(&self, delete_files: bool);
    pub fn status(&self) -> DownloadStatus;
    pub fn detail(&self) -> EngineDetail;
    pub fn set_limit(&self, bps: Option<u64>);
}
```
Flow: probe → resolve `final_path = unique_path(output_dir or cfg.default_dir, filename override or probed)` → create `.part`, `set_len(total)` when known → multi-segment if `accepts_ranges && total.is_some()` else single stream (segment `0..u64::MAX` semantics: write sequentially, `resumable=false`). Spawn one worker per segment, max `cfg.connections` concurrent; when a worker finishes `Done` and others remain, `steal_split` the segment with the largest remaining and start a worker on the new piece. Background flusher writes the sidecar every `flush_interval_ms` and on pause. Any worker `RangeUnsupported` ⇒ cancel all, truncate, restart once as single stream. `RemoteChanged` ⇒ cancel all, flush, `NeedsAttention{RemoteChanged}`. `Fatal` ⇒ `Failed{reason}`. All segments done ⇒ fsync, delete sidecar, rename `.part` → final, `Completed`, emit `EngineEvent::Completed`. ETA = remaining / bps (None if bps == 0 or total unknown). Each chunk acquires per-download and global buckets.

- [ ] **Step 1: Failing tests** (`tests/http_download.rs`, helper `wait_state(&dl, pred, timeout)`):
  - `downloads_multi_segment`: 4 MiB, 8 conns → `Completed`, final file bytes match, no `.part`/`.part.puppy` left, `range_requests() >= 4`.
  - `downloads_no_ranges_single_stream`: `ranges=false` → `Completed`, detail `segments.len()==1`, `resumable==false`.
  - `ranges_lied_falls_back_to_single`: `ranges_lie=true` → `Completed`, bytes correct.
  - `unknown_length_single_stream`: `content_length=false, ranges=false` → `total_bytes==None` while active, `Completed`, bytes correct.
  - `pause_and_resume_same_session`: throttled 512 KiB/s; pause at ≥1 MiB → `Paused`, sidecar exists, `done_bytes` stable for 300 ms; resume → `Completed`, bytes correct.
  - `per_download_limit`: limit 256 KiB/s on 1 MiB file → takes ≥ 3 s (real time OK, or use `bytes_per_sec` + virtual time).
  - `remote_change_mid_download_needs_attention`: throttle, `set_version(1)` mid-way → `NeedsAttention{RemoteChanged}`, final file not created.
  - `filename_collision`: existing `file.bin` in dir → result path `file (1).bin`.
- [ ] **Step 2:** FAIL. **Step 3:** Implement. **Step 4:** PASS.
- [ ] **Step 5:** Commit `feat(http): download orchestrator with stealing, fallback, pause/resume`.

---

### Task 8: Restore from sidecar and validation (incl. spot check)

**Files:** Create `src/http/validate.rs`, `tests/http_restore.rs`; Modify `src/http/download.rs` (add `restore`, use validation in `resume`)

**Interfaces — Produces:**
```rust
pub enum Validation { Ok, Changed, NotResumable }
/// Strong validator present: HEAD/range-GET with If-Range; 206 ⇒ Ok, 200 ⇒ Changed.
/// No strong validator: total must equal recorded total AND `cfg.spot_check_bytes` fetched at up to
/// 3 offsets inside completed regions (start, middle, end of completed bytes) must byte-equal the `.part` file.
/// Unknown total ⇒ NotResumable.
pub async fn validate_remote(client: &reqwest::Client, state: &HttpResumeState, part: &Path, cfg: &HttpConfig)
    -> Result<Validation, EngineError>;
impl HttpDownload {
    pub async fn restore(id: DownloadId, final_path: &Path, client: reqwest::Client, cfg: HttpConfig,
        global: TokenBucket, events: broadcast::Sender<EngineEvent>) -> Result<(Self, Added), EngineError>;
}
```
`restore`: load sidecar → validate → `Ok` ⇒ continue remaining segments; `Changed` ⇒ `NeedsAttention{RemoteChanged}`; `NotResumable` ⇒ `NeedsAttention{NotResumable}`; missing `.part` or sidecar ⇒ `NeedsAttention{ValidationFailed{detail}}`. Weak ETags count as "no strong validator" (Last-Modified alone is used with `If-Range` only if no ETag at all).

- [ ] **Step 1: Failing tests:**
  - `restore_after_kill_completes_correctly`: start, wait ≥1 MiB, drop the download's runtime task abruptly (abort, no pause), then `restore` → `Completed`, bytes correct.
  - `restore_strong_etag_changed`: pause, `set_version(1)`, restore → `NeedsAttention{RemoteChanged}`.
  - `restore_no_validator_spot_check_ok`: `etag=false, last_modified=false`, pause, restore → `Completed`, bytes correct.
  - `restore_no_validator_spot_check_detects_change`: same, `set_version(1)` → `NeedsAttention{RemoteChanged}`.
  - `restore_no_validator_length_changed`: `set_cfg(len = 5 MiB)` → `NeedsAttention{RemoteChanged}`.
  - `restore_unknown_length_not_resumable`: → `NeedsAttention{NotResumable}`.
  - `restore_missing_sidecar`: delete sidecar → `NeedsAttention{ValidationFailed{..}}`.
- [ ] **Step 2:** FAIL. **Step 3:** Implement. **Step 4:** PASS.
- [ ] **Step 5:** Commit `feat(http): restore with validator and spot-check validation`.

---

### Task 9: `HttpEngine` — the `Engine` implementation

**Files:** Create `src/http/engine.rs`, `tests/http_engine.rs`; Modify `src/lib.rs` (re-export `HttpEngine`)

**Interfaces — Produces:**
```rust
pub struct HttpEngine { /* client, cfg, global TokenBucket, DashMap/Mutex<HashMap<DownloadId, HttpDownload>>, broadcast::Sender<EngineEvent> (cap 256) */ }
impl HttpEngine { pub fn new(cfg: HttpConfig) -> Result<Self, EngineError>; }
impl Engine for HttpEngine { /* kind() = Http; accepts() = Source::Http with http/https scheme */ }
```
`add` with `StartMode::Fresh` ⇒ `start_fresh`; `Restore{path}` ⇒ `restore`. `add` on a non-HTTP source ⇒ `Unsupported`; duplicate id ⇒ `Invalid`. `set_global_download_budget` ⇒ `global.set_rate`. `remove(delete_files=true)` deletes `.part`, sidecar, and the final file if completed.

- [ ] **Step 1: Failing tests:**
  - `engine_add_and_complete_emits_event`: subscribe, add → receives `Completed{id}`; `status(id).state == Completed`.
  - `engine_rejects_magnet`: `accepts(Magnet)` false; `add` → `Unsupported`.
  - `engine_global_budget_applies_across_downloads`: two concurrent 512 KiB downloads, budget 256 KiB/s → both done no sooner than ~3.5 s.
  - `engine_remove_deletes_files`: pause mid-way, `remove(id, true)` → no `.part`/sidecar; `status(id)` is None.
  - `engine_restore_mode`: add Fresh, pause, construct a **new** `HttpEngine`, `add(id, req, Restore{path})` → `Completed`.
- [ ] **Step 2:** FAIL. **Step 3:** Implement. **Step 4:** PASS.
- [ ] **Step 5:** Run full suite: `cargo test --workspace && cargo clippy --all-targets -- -D warnings && cargo fmt --check` → all green.
- [ ] **Step 6:** Commit `feat(http): HttpEngine implementing Engine`.
