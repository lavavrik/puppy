# puppy Plan 4 — BitTorrent engine (librqbit)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `.torrent` files and magnet links download through the daemon and show up in the dashboard like HTTP downloads (with peers, piece map, up speed), via a new `puppy-torrent` crate that wraps librqbit behind puppy's own `Engine` trait so it can be replaced by a home-grown engine later.

**Architecture:** `puppy-torrent::TorrentEngine` owns one `librqbit::Session` and maps puppy `DownloadId`s to librqbit torrent handles. A 500 ms poll task turns librqbit stats into puppy `DownloadState` transitions and `EngineEvent`s. Torrent sources are re-added on restore (librqbit's own session persistence stays **off**, so puppy's store remains the single source of truth); existing data is re-verified by librqbit's initial check, which now runs inside the daemon's non-blocking restore jobs. The daemon registers the engine next to `HttpEngine`; the dashboard already renders `[T]`, peers, up speed and `PieceMap`.

**Tech Stack:** librqbit 9.1 (`Session::new_with_opts`, `SessionOptions{dht, persistence: None, fastresume: false, listen, ratelimits, disable_trackers}`, `AddTorrent::{Url, TorrentFileBytes}`, `AddTorrentOptions{paused, output_folder, overwrite, ratelimits, initial_peers, disable_trackers}`, `Session::{pause, unpause, delete, get}`, `ManagedTorrent::{stats, live, info_hash, name, is_paused}`, `Session.ratelimits.{set_download_bps, set_upload_bps}`, `create_torrent` for tests), tokio, puppy-core, puppy-daemon.

**Spec:** `docs/superpowers/specs/2026-11-19-puppy-design.md` → "Torrent engine", "Rate limiting", "Engine abstraction".

**Builds on:** Plans 1–3 (`plan-3-tui`). Plan 2's final review recorded these preconditions, addressed here: upload budget in the `Engine` trait (Task 1), non-blocking restore (already done in Plan 2's fix wave), bounded shutdown (done), Completed vs seeding policy (Task 5), magnet display name (Task 3), `detail()` cost per tick (Task 4).

## Global Constraints

- librqbit is used **only** inside `crates/puppy-torrent`; no librqbit type appears in any public signature outside that crate.
- Seeding policy v1 (conservative, per spec): stop at completion — when a torrent finishes, pause it and report `Completed`; config `torrent.seed_ratio` (default `0.0`) reserved for later (documented, not implemented beyond 0).
- DHT enabled by default (config `torrent.dht = true`), trackers enabled, UPnP **off** by default (`torrent.upnp = false`), listen port range `torrent.listen_port` default `42069` (TCP+uTP per librqbit defaults; falls back to an OS-assigned port if busy).
- Per-download torrent limits (the dashboard already blocks `l` for started torrents and maps `ErrorCode::Unsupported` to a friendly toast): librqbit supports them only at add time (`AddTorrentOptions.ratelimits`). Runtime `set_download_limit` on a torrent ⇒ `EngineError::Unsupported("per-torrent limits can only be set before the torrent starts")`; the limit stored by the daemon is applied on the next (re)add/restore. The dashboard shows the reason in a toast.
- Global limits: download budget from the daemon's divider ⇒ `session.ratelimits.set_download_bps`; global upload limit ⇒ `set_upload_bps` (new trait method, Task 1).
- Piece map: librqbit does not expose the have-bitfield publicly. Ruling: `EngineDetail::Torrent.have` is synthesised from per-file progress (`stats().file_progress`) mapped onto piece ranges (a piece counts as had when the byte range it covers is within downloaded bytes of the files it overlaps, assuming in-order fill within each file); `piece_count` from torrent lengths. Documented as an approximation; a future own engine gives exact data.
- Cross-platform; tests must not touch the real network: DHT and trackers disabled in tests, peers given explicitly via `initial_peers` to a local seeder session created in the test.

## Review Focus

1. **Magnet with no reachable peers / no metadata** → stays `Starting` with name from `dn=` (or `magnet <first 8 hex of infohash>`), never blocks `add`, can be paused/removed while resolving. Test: Task 3 `magnet_without_peers_stays_starting_and_removable`.
2. **Remove with delete_files on a multi-file torrent** → removes the torrent's folder/files only (never the parent download dir). Test: Task 3 `remove_multi_file_deletes_only_its_files`.
3. **Daemon restart mid-torrent** → torrent restored paused-or-active as stored, existing data re-verified, continues to completion, no duplicate in librqbit. Test: Task 5 `restore_after_restart_resumes_torrent`.
4. **Same torrent added twice** (same info hash) → second add fails with a clear `Invalid("already downloading this torrent")`, the first is untouched. Test: Task 3 `duplicate_infohash_rejected`.
5. **Corrupt .torrent file / invalid magnet** → `add` fails fast with a readable error, no orphaned librqbit state. Test: Task 3 `invalid_sources_fail_cleanly`.

---

## File Structure

```
crates/puppy-core/src/engine.rs          + fn set_global_upload_budget(&self, bps: Option<u64>) (default no-op)
crates/puppy-daemon/src/manager/*        apply global up limit to all engines
crates/puppy-daemon/src/config.rs        + TorrentConfig { dht, upnp, listen_port, seed_ratio }
crates/puppy-torrent/Cargo.toml
crates/puppy-torrent/src/lib.rs          pub use TorrentEngine, TorrentConfig
crates/puppy-torrent/src/engine.rs       TorrentEngine: Engine impl
crates/puppy-torrent/src/map.rs          stats → DownloadStatus / DownloadState / EngineDetail (pure)
crates/puppy-torrent/src/source.rs       source resolution: magnet parse (dn, infohash), .torrent read/copy
crates/puppy-torrent/src/poll.rs         500 ms poll task → EngineEvents, completion ⇒ pause
crates/puppy-torrent/tests/support.rs    local seeder: temp files → create_torrent → seeding Session on 127.0.0.1
crates/puppy-torrent/tests/*.rs
crates/puppy-daemon/src/lifecycle.rs     register TorrentEngine
```

---

### Task 1: Upload budget in the Engine trait + torrent config

**Files:** `crates/puppy-core/src/engine.rs`, `crates/puppy-daemon/src/{config.rs, manager/commands.rs, manager/mod.rs}`.
**Produces:** `fn set_global_upload_budget(&self, bps: Option<u64>) {}` default method on `Engine`; the Manager calls it on every engine whenever the global up limit changes and at start. `TorrentConfig { dht: bool /*true*/, upnp: bool /*false*/, listen_port: u16 /*42069*/, seed_ratio: f32 /*0.0*/ }` as `Config.torrent` (`#[serde(default)]`, TOML table `[torrent]`).
- [ ] Step 0 (carry-over from Plan 3 re-review): the dashboard sparkline heading shows the real window displayed (`speed · last N s`, N = samples actually drawn); render test.
- [ ] Tests: `global_up_limit_reaches_engines` (fake engine records calls), `config_torrent_table_defaults_and_overrides`.
- [ ] Commit `feat: upload budget in Engine trait; torrent config`.

### Task 2: `puppy-torrent` scaffold, session setup, local seeder harness

**Files:** `crates/puppy-torrent/{Cargo.toml, src/lib.rs, src/engine.rs (skeleton), tests/support.rs, tests/harness.rs}`.
**Produces:** `pub struct TorrentEngine`; `impl TorrentEngine { pub async fn new(cfg: TorrentConfig, data_dir: PathBuf, default_dir: PathBuf, test_mode: bool) -> Result<Self, EngineError> }` — builds a librqbit `Session` with `persistence: None`, `fastresume: false`, DHT per cfg (always off in `test_mode`), trackers disabled in `test_mode`, listen on `127.0.0.1:0` in `test_mode` else `0.0.0.0:<listen_port>` (fallback port 0 on bind failure), UPnP per cfg, `cancellation_token` owned for shutdown. Test harness `support::Seeder::start(files: &[(&str, usize)]) -> Seeder { torrent_bytes: Bytes, magnet: String, addr: SocketAddr, dir: TempDir }` creating deterministic content, a torrent via `librqbit::create_torrent`, and a seeding session.
- [ ] Tests: `engine_starts_and_stops_cleanly`, `seeder_harness_serves_torrent` (a raw librqbit leecher session with `initial_peers=[seeder.addr]` completes) — proves the harness before the engine uses it.
- [ ] Commit `feat(torrent): crate scaffold, session setup, local seeder test harness`.

### Task 3: Add / pause / resume / remove / status

**Files:** `src/engine.rs`, `src/source.rs`, `src/map.rs`, `src/poll.rs`, `tests/engine.rs`.
**Behaviour:**
- `accepts()`: `Source::Magnet` and `Source::TorrentFile`.
- `add(id, req, Fresh, opts)`: TorrentFile ⇒ read bytes (≤ 16 MiB, else Invalid), copy to `<data_dir>/torrents/<id>.torrent` (for restore); Magnet ⇒ validate it parses (infohash present). Reject a duplicate info hash already in the session (Invalid). Call `session.add_torrent` with `AddTorrentOptions{ paused: opts.start_paused, output_folder: req.output_dir or default_dir (+ librqbit's multi-file sub-folder behaviour), overwrite: true, ratelimits: {download_bps: opts.limit}, initial_peers (test hook), ..}`. **Must return quickly:** do not wait for metadata — return `Added{ name: <torrent name | magnet dn | "magnet <8 hex>">, path: <output folder / file path when known>, total_bytes: None-until-known }`.
- `pause`/`resume` ⇒ `session.pause/unpause`. `remove(id, delete_files)` ⇒ `session.delete(id, delete_files)` + remove the stored `.torrent` copy.
- `status(id)` via `map::status(&stats, name, path)`: initializing ⇒ Starting; live ⇒ Active (down/up speed from live stats, eta); paused ⇒ Paused; error ⇒ Failed{reason, transient: false}; finished ⇒ Completed. `done_bytes = progress_bytes`, `total_bytes = Some(total)` once metadata known.
- `poll.rs`: every 500 ms compute each torrent's mapped state; emit `EngineEvent::StateChanged` on change and `Completed` once; on completion with `seed_ratio == 0.0` pause the torrent (stop seeding) but report Completed.
- [ ] Tests (local seeder, DHT/trackers off): `torrent_file_downloads_to_completion` (bytes verified), `magnet_with_initial_peer_downloads`, `pause_and_resume`, `remove_multi_file_deletes_only_its_files`, `duplicate_infohash_rejected`, `invalid_sources_fail_cleanly` (garbage .torrent, `magnet:?xt=bad`), `magnet_without_peers_stays_starting_and_removable`, `completion_stops_seeding` (seeder-side upload stops increasing after completion — or engine reports paused-internally while Completed), unit tests for `map::status` across states and for magnet `dn` parsing.
- [ ] Commit `feat(torrent): add, pause, resume, remove and status via librqbit`.

### Task 4: Detail, limits

**Files:** `src/map.rs`, `src/engine.rs`, `tests/detail.rs`.
- `detail(id)` ⇒ `EngineDetail::Torrent{ piece_count, have (synthesised per Global Constraints), peers (live peer_stats connected), seeds/leechers (best effort from peer stats; 0 if unknown), ratio = uploaded/total }`; cheap (no allocation beyond the bitfield; cache the bitfield per torrent, recompute at most every 1 s).
- `set_global_download_budget` / `set_global_upload_budget` ⇒ session ratelimits (0/None ⇒ unlimited).
- `set_download_limit(id, bps)` ⇒ `Unsupported(...)` (see Global Constraints) unless the torrent is still being added (store it for the add).
- [ ] Tests: `detail_reports_pieces_and_peers` (mid-download with a throttled seeder: have bits ⊂ piece_count, ≥1 peer), `have_bitfield_from_file_progress` (pure unit test, multi-file layouts, piece spanning files), `global_limits_apply_to_session`, `per_torrent_runtime_limit_unsupported`.
- [ ] Commit `feat(torrent): detail view, global and per-torrent limits`.

### Task 5: Restore + daemon integration

**Files:** `src/engine.rs`, `crates/puppy-daemon/src/lifecycle.rs`, `crates/puppy-daemon/Cargo.toml`, `crates/puppy-daemon/tests/torrent.rs`, `README.md`, `docs/`.
- `add(id, req, Restore{path}, opts)`: re-add from the stored `.torrent` copy (TorrentFile) or the magnet, with `output_folder` derived from `path`, `overwrite: true`, `paused: opts.start_paused`; librqbit verifies existing data during initializing. Missing stored copy for a TorrentFile ⇒ fall back to the original path if it still exists, else `NeedsAttention{ValidationFailed{"torrent file missing"}}`.
- `run_daemon`: build `TorrentEngine::new(cfg.torrent, data_dir, download_dir, false)` and pass both engines to the Manager; torrent engine shutdown cancels the session after the Manager's pause phase (within the 10 s bound).
- Manager snapshot: `peers` from torrent status/detail without allocating the bitfield each tick (add `peers` to `DownloadStatus`? — Ruling: add `#[serde(default)] pub peers: Option<u32>` to `puppy_core::types::DownloadStatus`, filled by the torrent engine; snapshot reads it instead of `detail()`).
- README: torrents supported; config `[torrent]` documented; note piece map approximation.
- [ ] Tests: `restore_after_restart_resumes_torrent` (engine-level: add, pause mid-way, drop engine, new engine + Restore ⇒ Completed, bytes ok), daemon integration `daemon_downloads_local_torrent` (run_daemon in-process with a test hook enabling `test_mode` + initial peer via a `PUPPY_TORRENT_TEST_PEER` env or DaemonOptions field — Ruling: `DaemonOptions.torrent_test_peer: Option<SocketAddr>` (doc-hidden) wiring `test_mode`), and TUI `render` test with a torrent snapshot already exists.
- [ ] Full workspace tests, clippy, fmt; push.
- [ ] Commit `feat: torrent engine wired into the daemon; restore; docs`.
