# puppy Plan 2 — Daemon, protocol & client

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A background daemon (`puppy daemon`) that owns all downloads via `puppy-core` engines — queue, scheduling, global rate-limit division, persistence, restore-on-start, single-instance lifecycle — served over a per-user local socket with a versioned newline-delimited JSON protocol, plus a client library that auto-spawns the daemon.

**Architecture:** New crates: `puppy-proto` (wire types + framing), `puppy-testserver` (the axum test server extracted from puppy-core tests, dev-only), `puppy-daemon` (library: config, store, manager/scheduler, server, lifecycle), `puppy-client` (library: connect/auto-spawn/typed calls/event stream), and the `puppy` binary (`puppy daemon …`; the dashboard arrives in Plan 3). The `Manager` is pure orchestration over `Arc<dyn Engine>` and is tested without sockets; the server is a thin dispatcher over it.

**Tech Stack:** Rust 1.99, tokio 1.54, interprocess 2.4 (`tokio` feature; Unix domain socket / Windows named pipe), serde/serde_json, toml 1.x, fs4 1.1 (lock file), clap 4.6 (derive), tracing + tracing-subscriber 0.3, dirs 6, puppy-core.

**Spec:** `docs/superpowers/specs/2026-11-19-puppy-design.md` (sections: Components, Daemon lifecycle, Daemon ↔ client protocol, Queue & scheduling, Persistence & config, Rate limiting).

**Builds on:** Plan 1 (`plan-1-core-http`): `Engine` trait (`add(id, req, StartMode, AddOptions)`, `pause`, `resume`, `remove`, `status`, `detail`, `set_download_limit`, `set_global_download_budget`, `subscribe`), `HttpEngine::new(HttpConfig)`, types `AddRequest`, `Source`, `DownloadState`, `DownloadStatus` (incl. `path`), `EngineDetail`, `AddOptions{start_paused, limit}`. Read `crates/puppy-core/src/engine.rs` docs: `add` blocks for probe/validation; restart after NeedsAttention = `remove(id, true)` + fresh `add`; dropping the engine does not stop downloads.

## Global Constraints

- Cross-platform: Linux, macOS, Windows × x86_64, aarch64. Platform code only in small `#[cfg]` functions (`paths.rs`, `spawn.rs`). CI matrix (existing `.github/workflows/ci.yml`) must stay green.
- Edition 2024; `cargo clippy --all-targets -- -D warnings`, `cargo fmt --check` clean at every commit.
- Protocol version **1**; every connection starts with `hello`; a version mismatch gets an error response and the connection is closed.
- Framing: one JSON object per line (`\n`), max line length **1 MiB**; over-long lines ⇒ protocol error, connection closed.
- Socket: Unix `$XDG_RUNTIME_DIR/puppy/puppy.sock` (dir mode 0700), else `<data_dir>/puppy.sock`; Windows named pipe `puppy-<username>`. Overridable by env `PUPPY_SOCKET` (tests). Socket file mode 0600 on Unix.
- Data dir: Linux `~/.local/share/puppy`, macOS `~/Library/Application Support/puppy`, Windows `%APPDATA%\puppy`; config dir: Linux `~/.config/puppy`, macOS/Windows same as data. Env `PUPPY_HOME` overrides both (tests) as `<PUPPY_HOME>/data` and `<PUPPY_HOME>/config`.
- State file `<data_dir>/state.json`, written atomically (`puppy_core::fsutil::atomic_write`, private=true ⇒ 0600 on Unix) — it holds request context (cookies/auth).
- Defaults: max active downloads **3**; connections per download **8** (HttpConfig); download dir = OS Downloads folder; global limits none.
- Queue states owned by the daemon: `Queued` (not yet in an engine or waiting for a slot). Engine states pass through.
- Snapshot tick default **250 ms** (4 Hz), client-selectable 100–1000 ms.
- Idle exit (daemon spawned by a client with `--idle-exit <secs>`, default 30 s): exits when no Active/Starting/Queued downloads and no connected clients for that long. Without the flag (login service) never idle-exits.
- Graceful shutdown (Shutdown request, Ctrl-C, SIGTERM): pause all active downloads (so sidecars flush), persist state, remove socket file, exit 0.

## Review Focus

1. **Daemon restart with downloads in every state** (queued never-started, active, paused, completed, failed, needs-attention) → every one comes back in the same state; actives resume (with validation); nothing touches the network for paused ones. Test: Task 6 `restart_restores_every_state`.
2. **Two daemons racing to start** (two clients auto-spawn at once) → exactly one wins the lock; the loser exits 0 quickly and the client connects to the winner. Test: Task 8 `second_daemon_exits_when_locked`, Task 9 `concurrent_autospawn_single_daemon`.
3. **Stale socket after crash** (socket file exists, no daemon) → new daemon removes it and binds; client's connect fails fast, then spawns. Test: Task 8 `stale_socket_is_replaced`.
4. **Client disconnects mid-stream / slow client** → daemon must not block other clients or the manager; a slow subscriber's events are dropped/coalesced (latest snapshot wins), never buffered unboundedly. Test: Task 7 `slow_subscriber_does_not_block`.
5. **Corrupt or future-version state.json** → daemon starts with an empty list, renames the bad file to `state.json.corrupt-<unix-ts>`, logs it, never crashes. Test: Task 4 `corrupt_state_is_quarantined`.

---

## File Structure

```
Cargo.toml                                  workspace: add new members + deps
crates/puppy-testserver/                    (publish = false) axum TestServer moved from puppy-core/tests/support
crates/puppy-core/tests/*.rs                use puppy_testserver instead of `mod support`
crates/puppy-proto/src/lib.rs               PROTOCOL_VERSION, Request, Response, ServerMsg, Event, DownloadSnapshot, Totals, Settings, ErrorCode
crates/puppy-proto/src/codec.rs             read_msg / write_msg (NDJSON, 1 MiB cap)
crates/puppy-daemon/src/lib.rs
crates/puppy-daemon/src/paths.rs            data/config dirs, socket name, PUPPY_HOME / PUPPY_SOCKET
crates/puppy-daemon/src/config.rs           Config (TOML) + defaults
crates/puppy-daemon/src/store.rs            StoredDownload, PersistedState, Store (load/quarantine/save debounced)
crates/puppy-daemon/src/budget.rs           divide global limit across engines by active count
crates/puppy-daemon/src/manager.rs          Manager: queue, scheduler, commands, engine-event handling, snapshots
crates/puppy-daemon/src/server.rs           local-socket accept loop, per-connection handler, subscriptions
crates/puppy-daemon/src/lifecycle.rs        run_daemon(): lock, stale socket, idle exit, signals, graceful shutdown
crates/puppy-client/src/lib.rs              Client: connect, connect_or_spawn, typed calls, subscribe stream
crates/puppy-client/src/spawn.rs            detached spawn of `<exe> daemon --idle-exit 30`
crates/puppy/src/main.rs                    clap: `puppy daemon [--idle-exit N]`, `puppy` (placeholder until Plan 3)
packaging/systemd/puppy.service             user unit
packaging/launchd/dev.puppy.daemon.plist    LaunchAgent
docs/service.md                             install steps for Linux/macOS/Windows (schtasks)
```

---

### Task 1: Extract the test server into `puppy-testserver`

**Files:** Create `crates/puppy-testserver/{Cargo.toml, src/lib.rs}` (content moved from `crates/puppy-core/tests/support/mod.rs`); modify every `crates/puppy-core/tests/*.rs` to `use puppy_testserver::{TestServer, ServerCfg, body_byte};`; delete `tests/support/`; add to workspace members; puppy-core `[dev-dependencies] puppy-testserver = { path = "../puppy-testserver" }`.

**Interfaces — Produces:** `puppy_testserver::{TestServer, ServerCfg, body_byte}` with exactly the current API (start, url, set_version, range_requests, set_cfg, rotate_signed, paths_seen, requests_seen, all ServerCfg fields). `publish = false`.

- [ ] **Step 0 (carry-over fixes from Plan 1's final re-review):**
  - `crates/puppy-core/src/http/download/files.rs` finalize: after a successful rename, failure to remove the sidecar must only `warn!` and the download still ends Completed (it currently `?`-fails). Test: make the sidecar undeletable is hard portably — instead unit-test via a seam or assert by code review; at minimum add a comment + a test that a missing sidecar at that point is fine.
  - `crates/puppy-core/src/fsutil.rs` Windows `rename_no_replace`: also retry on `raw_os_error() == Some(32)` (ERROR_SHARING_VIOLATION) and `Some(33)` (ERROR_LOCK_VIOLATION), not only `PermissionDenied`.
  - Commit separately: `fix(core): non-fatal sidecar cleanup after rename; retry Windows sharing violations`.
- [ ] **Step 1:** Move the module verbatim (remove `#[allow(dead_code)]` hacks that only existed for `mod support`).
- [ ] **Step 2:** `cargo test --workspace` → same test count as before the move (132), all pass; clippy/fmt clean.
- [ ] **Step 3:** Commit `refactor(test): extract axum test server into puppy-testserver crate`.

---

### Task 2: `puppy-proto` — wire types and NDJSON codec

**Files:** Create `crates/puppy-proto/{Cargo.toml, src/lib.rs, src/codec.rs}`.

**Interfaces — Produces:**
```rust
pub const PROTOCOL_VERSION: u32 = 1;
pub const MAX_LINE: usize = 1 << 20;

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(tag = "method", rename_all = "snake_case")]
pub enum Request {
    Hello { client: String, protocol: u32 },
    Add { request: AddRequest, #[serde(default)] start_paused: bool, #[serde(default)] limit: Option<u64> },
    List,
    Get { id: DownloadId },
    Pause { id: DownloadId }, Resume { id: DownloadId },
    Remove { id: DownloadId, delete_files: bool },
    Restart { id: DownloadId },                       // NeedsAttention/Failed → remove(true) + fresh add, same id
    Move { id: DownloadId, position: usize },         // queue reorder
    SetLimit { id: DownloadId, bps: Option<u64> },
    SetGlobalLimits { down: Option<u64>, up: Option<u64> },
    SetMaxActive { n: u16 },
    GetSettings,
    Subscribe { interval_ms: u64 },
    Unsubscribe,
    Shutdown,
}
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct Envelope<T> { pub req_id: u64, #[serde(flatten)] pub body: T }   // client → server: Envelope<Request>

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMsg {
    Response { req_id: u64, #[serde(flatten)] result: Reply },
    Event { event: Event },
}
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum Reply { Ok { data: ReplyData }, Err { code: ErrorCode, message: String } }
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ReplyData { Hello { daemon_version: String, protocol: u32 }, Added { id: DownloadId },
    Downloads { downloads: Vec<DownloadSnapshot> }, Download { download: DownloadSnapshot, detail: Option<EngineDetail> },
    Settings { settings: Settings }, Done }
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode { ProtocolMismatch, BadRequest, NotFound, InvalidState, Engine, Internal }
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Event {
    Snapshot { downloads: Vec<DownloadSnapshot>, totals: Totals },
    Added { id: DownloadId }, Completed { id: DownloadId, name: String },
    Failed { id: DownloadId, reason: String }, Removed { id: DownloadId },
    NeedsAttention { id: DownloadId, reason: AttentionReason },
}
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct DownloadSnapshot { pub id: DownloadId, pub kind: EngineKindTag /*http|torrent*/, pub source: String /*display: URL or magnet name or .torrent path*/,
    pub name: String, pub path: Option<PathBuf>, pub state: DownloadState, pub total_bytes: Option<u64>, pub done_bytes: u64,
    pub down_bps: u64, pub up_bps: u64, pub eta_secs: Option<u64>, pub limit: Option<u64>, pub queue_position: Option<usize>,
    pub connections: Option<u16> /*active segment count for http*/, pub peers: Option<u32> }
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)] #[serde(rename_all = "snake_case")]
pub enum EngineKindTag { Http, Torrent }
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Default)]
pub struct Totals { pub down_bps: u64, pub up_bps: u64, pub active: usize, pub queued: usize, pub paused: usize,
    pub completed: usize, pub failed: usize }
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct Settings { pub max_active: u16, pub global_down_limit: Option<u64>, pub global_up_limit: Option<u64>,
    pub download_dir: PathBuf, pub connections: u16, pub theme: String }

// codec.rs
pub async fn read_msg<R: AsyncBufRead + Unpin, T: DeserializeOwned>(r: &mut R) -> Result<Option<T>, CodecError>; // None on clean EOF
pub async fn write_msg<W: AsyncWrite + Unpin, T: Serialize>(w: &mut W, msg: &T) -> Result<(), CodecError>;    // json + '\n', flush
#[derive(thiserror::Error, Debug)] pub enum CodecError { TooLong, Json(#[from] serde_json::Error), Io(#[from] std::io::Error) }
```
`read_msg` must enforce `MAX_LINE` **while reading** (never buffer more than MAX_LINE+1 bytes for one line).

- [ ] **Step 1: Failing tests:**
  - `request_wire_format`: `Envelope{req_id:7, body: Request::Pause{id:3}}` → exactly `{"req_id":7,"method":"pause","id":3}` (the envelope uses `req_id` so it never collides with a request's own `id` field).
  - `response_ok_wire_format`: `{"type":"response","req_id":1,"status":"ok","data":{"kind":"added","id":5}}`.
  - `response_err_wire_format`: code `not_found`.
  - `event_roundtrip` for every `Event` variant.
  - `codec_roundtrip_multiple_msgs` over `tokio::io::duplex`.
  - `codec_rejects_too_long`: a 2 MiB line ⇒ `CodecError::TooLong`, and the reader never allocated > ~1 MiB (assert via a counting reader wrapper or by checking returned error before EOF of the 2 MiB stream).
  - `codec_eof_returns_none`.
- [ ] **Step 2:** FAIL. **Step 3:** Implement. **Step 4:** PASS.
- [ ] **Step 5:** Commit `feat(proto): versioned NDJSON protocol types and codec`.

---

### Task 3: Daemon paths and config

**Files:** Create `crates/puppy-daemon/{Cargo.toml, src/lib.rs, src/paths.rs, src/config.rs}`.

**Interfaces — Produces:**
```rust
pub struct Paths { pub data_dir: PathBuf, pub config_dir: PathBuf, pub socket: SocketName, pub lock_file: PathBuf, pub state_file: PathBuf, pub log_dir: PathBuf }
pub enum SocketName { Path(PathBuf) /*unix*/, Pipe(String) /*windows*/ }
impl Paths { pub fn resolve() -> std::io::Result<Self>; /* honours PUPPY_HOME, PUPPY_SOCKET; creates dirs (0700 on unix) */ }

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)] #[serde(default)]
pub struct Config { pub download_dir: PathBuf, pub connections: u16 /*8*/, pub max_active: u16 /*3*/,
    pub global_down_limit: Option<u64>, pub global_up_limit: Option<u64>, pub theme: String /*"auto"*/, pub proxy: Option<url::Url> }
impl Default for Config { /* download_dir = dirs::download_dir() or home/Downloads */ }
impl Config { pub fn load(path: &Path) -> Result<Self, ConfigError>; /* missing file ⇒ Default; invalid ⇒ Err with line info */
              pub fn http_config(&self) -> puppy_core::config::HttpConfig; }
```
`lock_file = data_dir/puppy.lock`, `state_file = data_dir/state.json`, `log_dir = data_dir/logs`. On Windows the pipe name is `puppy-<USERNAME>` (sanitised to `[A-Za-z0-9_-]`); with `PUPPY_SOCKET` set, Unix uses it as a path and Windows as the pipe name.

- [ ] **Step 1: Failing tests:** `puppy_home_overrides_dirs`, `puppy_socket_override`, `config_missing_is_default`, `config_partial_fills_defaults` (`max_active = 5` only ⇒ connections 8), `config_invalid_reports_error`, `http_config_maps_fields` (connections, proxy, download_dir → default_dir). Use a mutex to serialise env-var tests.
- [ ] **Step 2–4:** FAIL → implement → PASS.
- [ ] **Step 5:** Commit `feat(daemon): platform paths and TOML config`.

---

### Task 4: Persistent store

**Files:** Create `crates/puppy-daemon/src/store.rs`.

**Interfaces — Produces:**
```rust
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct StoredDownload { pub id: DownloadId, pub request: AddRequest, pub limit: Option<u64>,
    pub state: StoredState, pub name: Option<String>, pub path: Option<PathBuf>, pub total_bytes: Option<u64>,
    pub created_unix: u64, pub finished_unix: Option<u64> }
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)] #[serde(tag = "kind", rename_all = "snake_case")]
pub enum StoredState { Queued, Active, Paused, Completed, Failed { reason: String }, NeedsAttention { reason: AttentionReason } }
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Default)]
pub struct PersistedState { pub version: u32 /*1*/, pub next_id: DownloadId /*starts 1*/, pub order: Vec<DownloadId>,
    pub downloads: Vec<StoredDownload>, pub settings: Option<RuntimeSettings> }
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct RuntimeSettings { pub max_active: u16, pub global_down_limit: Option<u64>, pub global_up_limit: Option<u64> }
pub struct Store { /* path, debounce */ }
impl Store {
    pub fn new(path: PathBuf) -> Self;
    /// Missing ⇒ Default. Corrupt or version ≠ 1 ⇒ rename to `state.json.corrupt-<unix-ts>`, warn, Default.
    pub async fn load(&self) -> PersistedState;
    pub async fn save_now(&self, st: &PersistedState) -> std::io::Result<()>;   // atomic_write(private = true)
    /// Debounced: coalesces calls within 300 ms; the latest state wins. Spawns its own task.
    pub fn save_soon(&self, st: PersistedState);
    pub async fn flush(&self);   // awaits any pending debounced save
}
```
- [ ] **Step 1: Failing tests:** `missing_state_is_default`, `roundtrip`, `corrupt_state_is_quarantined` (garbage file ⇒ Default + a `state.json.corrupt-*` file exists), `future_version_is_quarantined`, `save_soon_coalesces` (10 calls in a burst ⇒ final file = last state; use paused tokio time or count writes via a test hook), `state_file_is_private` (`#[cfg(unix)]` mode 0600).
- [ ] **Step 2–4:** FAIL → implement → PASS.
- [ ] **Step 5:** Commit `feat(daemon): persistent store with quarantine and debounced saves`.

---

### Task 5: Global budget divider

**Files:** Create `crates/puppy-daemon/src/budget.rs`.

**Interfaces — Produces:**
```rust
/// Splits a global download limit across engines in proportion to their active download counts.
/// Engines with 0 active still get a small share so a newly started download isn't unlimited before the next rebalance.
pub fn divide(global: Option<u64>, active_per_engine: &[usize]) -> Vec<Option<u64>>;
```
Rules: `global = None` ⇒ all `None`. Otherwise total active `t`; if `t == 0` ⇒ each engine gets `Some(global)` (only one can start before the next rebalance, which the Manager runs on every state change and every 1 s). Else engine *i* gets `Some(max(1, global * a_i / t))` for `a_i > 0` and `Some(max(1, global / (t+1)))` for `a_i == 0`.
- [ ] **Step 1: Failing tests:** `unlimited`, `single_engine_gets_all`, `proportional_split` (global 900, actives [2,1] ⇒ [600, 300]), `idle_engine_gets_small_share`, `no_actives_each_gets_global`, `never_zero` (global 1, actives [3,3] ⇒ both ≥ 1).
- [ ] **Step 2–4:** FAIL → implement → PASS. **Step 5:** Commit `feat(daemon): global limit division across engines`.

---

### Task 6: Manager — queue, scheduler, commands, restore

**Files:** Create `crates/puppy-daemon/src/manager.rs`, `crates/puppy-daemon/tests/manager.rs`.

**Interfaces — Consumes:** `Engine`, `HttpEngine`, `Store`, `Config`, `budget::divide`, proto types.
**Produces:**
```rust
pub struct Manager { /* Arc inner */ }
impl Manager {
    pub async fn start(engines: Vec<Arc<dyn Engine>>, store: Store, cfg: Config) -> Arc<Self>;  // loads + restores (see below), spawns event loop + 1 s rebalance/schedule ticker
    pub async fn add(&self, req: AddRequest, start_paused: bool, limit: Option<u64>) -> Result<DownloadId, ManagerError>;
    pub async fn pause(&self, id: DownloadId) -> Result<(), ManagerError>;
    pub async fn resume(&self, id: DownloadId) -> Result<(), ManagerError>;
    pub async fn remove(&self, id: DownloadId, delete_files: bool) -> Result<(), ManagerError>;
    pub async fn restart(&self, id: DownloadId) -> Result<(), ManagerError>;
    pub async fn move_to(&self, id: DownloadId, position: usize) -> Result<(), ManagerError>;
    pub fn set_limit(&self, id: DownloadId, bps: Option<u64>) -> Result<(), ManagerError>;
    pub fn set_global_limits(&self, down: Option<u64>, up: Option<u64>);
    pub fn set_max_active(&self, n: u16) -> Result<(), ManagerError>;   // n == 0 ⇒ BadRequest
    pub fn settings(&self) -> Settings;
    pub fn snapshot(&self) -> (Vec<DownloadSnapshot>, Totals);          // ordered by queue order; completed last (by finished time desc)
    pub fn get(&self, id: DownloadId) -> Result<(DownloadSnapshot, Option<EngineDetail>), ManagerError>;
    pub fn events(&self) -> broadcast::Receiver<Event>;                 // discrete events (cap 256)
    pub fn is_idle(&self) -> bool;                                      // no Active/Starting/Queued
    pub async fn shutdown(&self);                                       // pause all engine-held downloads, persist, flush store
}
#[derive(thiserror::Error, Debug)] pub enum ManagerError { NotFound(DownloadId), InvalidState(String), BadRequest(String), Engine(#[from] EngineError) }
impl ManagerError { pub fn code(&self) -> ErrorCode; }
```
Behaviour:
- `add`: allocate `next_id`, pick engine by `accepts()` (none ⇒ BadRequest "unsupported source"), store as `Queued` (or `Paused` if `start_paused`), persist, emit `Event::Added`, then `schedule()`. Returns immediately — the engine probe happens in `schedule()` in a spawned task, never blocking the caller or the manager lock.
- `schedule()`: while active count (Starting+Active) < max_active, take the first `Queued` in order and start it: if it has a `path` (restored before) ⇒ `engine.add(id, req, Restore{path}, AddOptions{start_paused:false, limit})`; else `Fresh`. Mark `Starting`; on success record name/path/total and `Active`; on error ⇒ `Failed{reason}` + `Event::Failed`. Called after every command, every engine event and every 1 s tick.
- Engine `StateChanged`/`Completed` events update the stored state, persist (`save_soon`), emit the matching proto `Event`, free slots and `schedule()`. Completed: set `finished_unix`, and call `engine.remove(id, false)` so the engine drops it (record stays in the store as Completed).
- `pause`: Queued ⇒ Paused (daemon-level). Engine-held ⇒ `engine.pause`. Completed ⇒ InvalidState.
- `resume`: Paused/Failed ⇒ if engine holds it and a slot is free ⇒ `engine.resume`; else ⇒ `Queued` and `schedule()`. NeedsAttention ⇒ InvalidState("use restart").
- `restart`: NeedsAttention/Failed ⇒ `engine.remove(id, true)` (ignore NotFound), clear name/path/total, ⇒ Queued, `schedule()`.
- `remove`: engine-held ⇒ `engine.remove(id, delete_files)`; if Completed and delete_files ⇒ delete the final file (ignore NotFound); drop from store; emit `Removed`.
- `move_to`: reorder within `order` (clamped); only affects not-yet-finished items.
- Limits: per-download ⇒ store + `engine.set_download_limit` if engine-held. Global ⇒ store in RuntimeSettings, rebalance via `budget::divide` and `set_global_download_budget` per engine; `up` stored only (torrent engine will consume it in Plan 4).
- **Restore on start:** for each stored download: Completed ⇒ keep record only. Queued ⇒ stays Queued. Paused/Failed/NeedsAttention with a `path` ⇒ `engine.add(Restore{path}, start_paused: true)` (no network) and keep its stored state. Active with a `path` ⇒ restore paused, then mark Queued at the front of the queue in original order (schedule will resume them, which validates). Active/Paused without a path (never got past probe) ⇒ Queued. Restore errors ⇒ Failed{reason}.
- Snapshot fields: engine-held ⇒ from `engine.status()`/`detail()` (connections = number of segments with `active == true`); others from the store record. `queue_position` = index among Queued items.

- [ ] **Step 1: Failing tests** (`tests/manager.rs`, real `HttpEngine` + `puppy_testserver`, tempdirs, `PUPPY_HOME` not needed — construct Store/Config directly):
  - `add_downloads_to_completion` (Event Added then Completed; snapshot Completed with path; file correct).
  - `max_active_limits_concurrency` (max_active 2, 4 throttled downloads ⇒ never more than 2 Starting/Active; all complete).
  - `queued_pause_and_resume`.
  - `move_reorders_queue` (max_active 1; move last queued to 0 ⇒ it starts next).
  - `global_limit_divided` (global 256 KiB/s, 2 × 512 KiB ⇒ total time ≥ ~3.5 s).
  - `remove_completed_deletes_file`.
  - `restart_after_needs_attention` (set_version mid-download ⇒ NeedsAttention ⇒ restart ⇒ Completed with new-version bytes).
  - `unsupported_source_is_bad_request` (magnet).
  - `restart_restores_every_state`: build a store with one each of queued/active/paused/completed/failed/needs_attention (drive real downloads to those states, `shutdown()`, drop manager), start a new Manager on the same store ⇒ states preserved, paused ones made **zero** requests during start (server `paths_seen` count unchanged), active one resumes to Completed.
- [ ] **Step 2–4:** FAIL → implement → PASS (manager.rs may be split into `manager/{mod.rs, schedule.rs, restore.rs, commands.rs}` if over ~600 lines).
- [ ] **Step 5:** Commit `feat(daemon): manager with queue, scheduler, limits and restore`.

---

### Task 7: Server — local socket, dispatch, subscriptions

**Files:** Create `crates/puppy-daemon/src/server.rs`, `crates/puppy-daemon/tests/server.rs`.

**Interfaces — Produces:**
```rust
pub struct Server { /* listener, manager, client count, shutdown token */ }
impl Server {
    pub async fn bind(name: &SocketName, manager: Arc<Manager>) -> std::io::Result<Self>; // interprocess tokio local socket; unix: chmod 0600
    pub fn client_count(&self) -> Arc<AtomicUsize>;
    pub fn shutdown_requested(&self) -> CancellationToken;   // cancelled by a Shutdown request
    pub async fn run(self, stop: CancellationToken);          // accept loop; one task per connection
}
```
Per connection: first message must be `Hello` with `protocol == 1` ⇒ reply `Hello{daemon_version, protocol}`; otherwise reply `Err{ProtocolMismatch|BadRequest}` and close. Requests are handled concurrently per connection (spawned), replies carry `req_id`. `Subscribe{interval_ms}` (clamped 100–1000) starts a per-connection ticker sending `Event::Snapshot` plus forwarding manager discrete events; `Unsubscribe` stops it. Writes go through a per-connection bounded channel (cap 64) to a single writer task; snapshots use a `watch`-style latest-value slot so a slow client only ever gets the newest snapshot (discrete events may be dropped for that client with a warning when its channel is full — never block the manager). `Shutdown` replies `Done` then cancels `shutdown_requested`.

- [ ] **Step 1: Failing tests** (`tests/server.rs`, real Manager + HttpEngine + testserver, socket name from a tempdir path / unique pipe name):
  - `hello_handshake`, `protocol_mismatch_closes`, `request_before_hello_rejected`.
  - `add_and_list_over_socket`, `errors_have_codes` (Get unknown id ⇒ `not_found`).
  - `subscribe_receives_snapshots` (≥ 3 snapshots in 1 s at 250 ms; Completed event arrives).
  - `slow_subscriber_does_not_block`: client A subscribes at 100 ms and never reads; client B can still `List` within 200 ms repeatedly for 3 s, and the daemon's memory for A stays bounded (assert A's pending queue length ≤ 64 via a test-only accessor or by observing B's latency).
  - `oversized_line_closes_connection`.
  - `shutdown_request_cancels_token`.
- [ ] **Step 2–4:** FAIL → implement → PASS.
- [ ] **Step 5:** Commit `feat(daemon): local-socket server with subscriptions`.

---

### Task 8: Lifecycle — single instance, stale socket, idle exit, signals

**Files:** Create `crates/puppy-daemon/src/lifecycle.rs`, `crates/puppy-daemon/tests/lifecycle.rs`.

**Interfaces — Produces:**
```rust
pub struct DaemonOptions { pub idle_exit: Option<Duration> }
pub enum RunOutcome { Exited, AlreadyRunning }
/// Resolves Paths, sets up file logging (log_dir/puppyd.log, plus stderr when attached to a tty),
/// takes an exclusive non-blocking fs4 lock on lock_file (fail ⇒ AlreadyRunning),
/// removes a stale unix socket file, loads config/store, builds HttpEngine + Manager + Server, runs until:
/// Shutdown request | Ctrl-C | SIGTERM (unix) | idle_exit elapsed with manager.is_idle() && client_count == 0.
/// Then manager.shutdown(), remove socket file (unix), release lock.
pub async fn run_daemon(opts: DaemonOptions) -> anyhow::Result<RunOutcome>;
```
- [ ] **Step 1: Failing tests** (each with its own `PUPPY_HOME` tempdir + `PUPPY_SOCKET`; run daemon via `run_daemon` in a spawned task or via the binary in Task 10 — here in-process):
  - `second_daemon_exits_when_locked` ⇒ `AlreadyRunning` within 1 s.
  - `stale_socket_is_replaced` (`#[cfg(unix)]`: create a regular file at the socket path ⇒ daemon binds and serves Hello).
  - `idle_exit_when_no_work_and_no_clients` (idle_exit 500 ms ⇒ exits ≤ 2 s; with a connected client it does not exit until the client disconnects).
  - `no_idle_exit_while_downloading` (throttled download keeps it alive past idle_exit).
  - `shutdown_persists_and_pauses` (active download, Shutdown request ⇒ state.json has it as Active with a path; sidecar exists).
- [ ] **Step 2–4:** FAIL → implement → PASS (add `anyhow` workspace dep).
- [ ] **Step 5:** Commit `feat(daemon): single-instance lifecycle, idle exit, graceful shutdown`.

---

### Task 9: `puppy-client` — connect, auto-spawn, typed calls

**Files:** Create `crates/puppy-client/{Cargo.toml, src/lib.rs, src/spawn.rs}`, `crates/puppy-client/tests/client.rs`.

**Interfaces — Produces:**
```rust
pub struct Client { /* writer half behind mutex, pending: HashMap<req_id, oneshot>, events: mpsc::Receiver<Event> (cap 256) */ }
impl Client {
    pub async fn connect(name: &SocketName, client_name: &str) -> Result<Self, ClientError>;     // performs Hello
    /// connect; on failure spawn `exe daemon --idle-exit 30` detached and retry with backoff (50 ms → 500 ms) for up to 5 s.
    pub async fn connect_or_spawn(name: &SocketName, client_name: &str, exe: &Path) -> Result<Self, ClientError>;
    pub async fn call(&self, req: Request) -> Result<ReplyData, ClientError>;   // ClientError::Daemon{code,message} for Err replies
    pub async fn add(&self, req: AddRequest, start_paused: bool, limit: Option<u64>) -> Result<DownloadId, ClientError>;
    pub async fn list(&self) -> Result<Vec<DownloadSnapshot>, ClientError>;
    pub async fn subscribe(&self, interval_ms: u64) -> Result<(), ClientError>;
    pub async fn next_event(&mut self) -> Option<Event>;                        // None when disconnected
    pub fn is_connected(&self) -> bool;
}
// spawn.rs
pub fn spawn_daemon_detached(exe: &Path, extra_env: &[(&str, &str)]) -> std::io::Result<()>;
// unix: Command + pre_exec(setsid), stdin/stdout/stderr null. windows: creation_flags DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW, stdio null.
```
`puppy-daemon` re-exports `SocketName` + `Paths`; `puppy-client` depends on `puppy-daemon` for those (and its tests use `run_daemon` in-process).
- [ ] **Step 1: Failing tests** (in-process daemon via `run_daemon` for most; for spawn tests use the `puppy` binary path from `env!("CARGO_BIN_EXE_puppy")` — so these two tests live in `crates/puppy/tests/` and are written in Task 10; here test with a stub):
  - `call_roundtrip_add_list_get`, `concurrent_calls_matched_by_req_id` (20 parallel `List` calls), `daemon_error_mapped`, `events_after_subscribe`, `disconnect_detected` (daemon shutdown ⇒ `next_event` None, `is_connected` false, calls error).
- [ ] **Step 2–4:** FAIL → implement → PASS.
- [ ] **Step 5:** Commit `feat(client): daemon client with request matching and auto-spawn`.

---

### Task 10: `puppy` binary, service files, end-to-end

**Files:** Create `crates/puppy/{Cargo.toml, src/main.rs}`, `crates/puppy/tests/e2e.rs`, `packaging/systemd/puppy.service`, `packaging/launchd/dev.puppy.daemon.plist`, `docs/service.md`; modify root `README.md` (create if missing: project blurb, status, build, service install link).

**Interfaces — Produces:** binary `puppy`:
- `puppy daemon [--idle-exit <SECS>]` ⇒ `run_daemon`; exit code 0 for both outcomes.
- `puppy` (no args) ⇒ until Plan 3: `connect_or_spawn`, print a one-line summary (`N downloads, X active`) and exit 0. (Plan 3 replaces this with the dashboard.)
- `puppy --version`.
Service files: systemd `ExecStart=%h/.cargo/bin/puppy daemon`, `Restart=on-failure`, `WantedBy=default.target`; launchd `ProgramArguments [/usr/local/bin/puppy, daemon]`, `RunAtLoad`, `KeepAlive{SuccessfulExit=false}`; docs/service.md covers Linux (`systemctl --user enable --now puppy`), macOS (`launchctl bootstrap gui/$(id -u) …`), Windows (`schtasks /Create /SC ONLOGON /TN puppy /TR "\"%USERPROFILE%\.cargo\bin\puppy.exe\" daemon" /RL LIMITED`).
- [ ] **Step 1: Failing tests** (`crates/puppy/tests/e2e.rs`, `env!("CARGO_BIN_EXE_puppy")`, per-test `PUPPY_HOME`/`PUPPY_SOCKET`):
  - `autospawn_then_idle_exit`: `connect_or_spawn` with the real binary ⇒ Hello ok; add a small download from testserver ⇒ Completed; disconnect ⇒ daemon process exits within idle (pass `--idle-exit 1` via a `PUPPY_IDLE_EXIT` env read by the spawner for tests) — verify the socket is gone / lock is free.
  - `concurrent_autospawn_single_daemon`: 4 clients `connect_or_spawn` simultaneously ⇒ all connected, exactly one daemon process (check via lock file being held + all clients see the same download list after one adds).
  - `restart_daemon_resumes_download`: start daemon, add throttled download, `Shutdown`, start again (spawn), ⇒ download completes, bytes correct.
- [ ] **Step 2–4:** FAIL → implement → PASS. Run the e2e tests 5× in a row locally to check for flakiness.
- [ ] **Step 5:** Full `cargo test --workspace`, clippy, fmt.
- [ ] **Step 6:** Commit `feat(cli): puppy binary with daemon subcommand, service files, e2e tests`.
