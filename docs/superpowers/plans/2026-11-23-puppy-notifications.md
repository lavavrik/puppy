# puppy desktop notifications Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** The daemon shows a native desktop popup when downloads finish, fail permanently or a torrent waits for its files to be chosen, grouping bursts into one popup, configurable under `[notifications]`.

**Architecture:** A `notify` module in puppy-daemon holds the pure parts (config → event classification, a `Batcher` that groups per kind with explicit `now` timestamps, popup text) and a `Notifier` sink trait. A notifier task subscribes to the manager's `Event` broadcast, looks up names, batches, and hands finished popups to the sink. The real sink (`DesktopNotifier`, notify-rust) runs on its own worker thread behind a bounded queue, so the manager is never blocked; only the `puppy daemon` binary installs it, so in-process tests never pop anything.

**Tech Stack:** Rust 2024, tokio, notify-rust 4.18 (default `z` feature: zbus with async-io, pure Rust, no libdbus; `mac-notification-sys` on macOS; WinRT toasts on Windows).

**Spec:** `docs/superpowers/specs/2026-11-23-puppy-notifications-design.md` (binding).

## Global Constraints

- Defaults: `enabled = true`, `completed = true`, `failed = true`, `choose_files = true`, `seeding_finished = false`. Transient failures (`Failed { transient: true }`) never notify.
- Popup title is always `"puppy"`. Bodies: `Finished: <name>` / `<n> downloads finished` / `Failed: <name> — <reason>` / `Choose files: <name> — run puppy files <id> or press c in the dashboard`.
- Protocol changes are additive only: the new `Event::ChoosingFiles` relies on the existing `#[serde(other)] Unknown`; `PROTOCOL_VERSION` stays 1.
- A missing notification server (headless, SSH, no D-Bus session) is ignored after one `debug!` line; sending never blocks the manager.
- Tests never pop real notifications: only `crates/puppy/src/main.rs` constructs `DesktopNotifier`, and not when `PUPPY_NO_NOTIFY=1` or `PUPPY_TORRENT_TEST_MODE=1`.
- No click actions, sounds or command hooks (non-goals).
- Cross-platform: Linux, macOS, Windows on x86_64 and aarch64; notify-rust is a plain (non-target-gated) dependency.
- Verify locally only: `cargo test --workspace`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo fmt --all --check`. Never trigger GitHub Actions.

## Decisions the spec left open

- Grouping: per kind, a popup is sent once **3 s pass with no new event of that kind** (`QUIET`), or at the latest **10 s after the first** (`MAX_HOLD`), so a steady trickle cannot produce one popup per file. Pending popups are flushed when the daemon shuts down.
- "Choosing files" detection: a new additive `Event::ChoosingFiles { id, name }`, emitted by `Inner::transition` on entering `ChoosingFiles`, once per download (an `Entry::choose_files_sent` flag; set for downloads restored as `ChoosingFiles`, so an auto-spawned daemon restarting every idle cycle does not re-ask).
- Names: the event's `name` if non-empty, else `Manager::display_name(id)` (the snapshot's name, no engine detail), else `download <id>`.
- Failure reasons: first line only, at most 160 chars (`…` appended when cut).
- Config is read at daemon start; changes need a daemon restart (`puppy stop`, next command restarts it) — same as every other key.

## Review Focus

1. **Mixed burst** (3 downloads finish and 1 fails within a second) → one `3 downloads finished` popup and one `Failed: …` popup, never merged across kinds. Test: Task 1 `kinds_group_separately`.
2. **Steady trickle** (one completion per second for 30 s) → at most 3 popups whose counts add up to 30. Test: Task 1 `trickle_is_capped`.
3. **Daemon restart with a torrent still choosing files** (the auto-spawned daemon idle-exits and restarts often) → no repeated "Choose files" popup. Test: Task 2 `restored_choosing_files_does_not_notify`.
4. **No notification server / backend that hangs** → daemon keeps running, `notify` returns at once, later popups dropped quietly. Test: Task 3 `failing_backend_is_quiet` and `full_queue_drops_instead_of_blocking`.
5. **Download finishes and the daemon stops within the grouping window** (`puppy stop`, idle exit) → popup still delivered. Test: Task 2 `pending_notification_flushed_on_shutdown`.

---

## File Structure

```
Cargo.toml                                  workspace dep notify-rust = "4.18"
crates/puppy-proto/src/lib.rs               Event::ChoosingFiles { id, name }
crates/puppy-daemon/Cargo.toml              notify-rust.workspace = true
crates/puppy-daemon/src/config.rs           NotificationsConfig; Config.notifications
crates/puppy-daemon/src/notify/mod.rs       NotifyKind, Item, Notification, classify, render, Batcher, Notifier, RecordingNotifier, spawn/NotifyTask, desktop_wanted
crates/puppy-daemon/src/notify/desktop.rs   DesktopNotifier (worker thread + notify-rust)
crates/puppy-daemon/src/manager/{mod.rs, restore.rs, commands.rs, snapshot.rs}   ChoosingFiles emission, flag, display_name
crates/puppy-daemon/src/lifecycle.rs        DaemonOptions.notifier; start/finish the task
crates/puppy/src/main.rs                    installs DesktopNotifier
crates/puppy/tests/common/mod.rs            PUPPY_NO_NOTIFY=1
README.md                                   [notifications] section
```

---

### Task 1: Config, protocol event, grouping and popup text

**Files:**
- Modify: `crates/puppy-daemon/src/config.rs`, `crates/puppy-proto/src/lib.rs` (Event + its roundtrip test list near line 519), `crates/puppy-daemon/src/lib.rs` (`pub mod notify;`, re-export `NotificationsConfig`)
- Create: `crates/puppy-daemon/src/notify/mod.rs` (tests inline)

**Interfaces:**
- Produces:
```rust
// puppy-proto
Event::ChoosingFiles { id: DownloadId, name: String }   // placed before Unknown

// config.rs — #[derive(Serialize, Deserialize, Debug, Clone, PartialEq)] #[serde(default)]
pub struct NotificationsConfig { pub enabled: bool, pub completed: bool, pub failed: bool, pub choose_files: bool, pub seeding_finished: bool }
impl Default for NotificationsConfig // true, true, true, true, false
pub struct Config { …, pub notifications: NotificationsConfig }   // TOML table [notifications]

// notify/mod.rs
pub const TITLE: &str = "puppy";
pub const QUIET: Duration = Duration::from_secs(3);
pub const MAX_HOLD: Duration = Duration::from_secs(10);
pub const MAX_REASON: usize = 160;
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)] pub enum NotifyKind { Completed, Failed, ChooseFiles, SeedingFinished }
#[derive(Debug, Clone, PartialEq)] pub struct Item { pub id: DownloadId, pub name: String, pub detail: Option<String> }
#[derive(Debug, Clone, PartialEq)] pub struct Notification { pub kind: NotifyKind, pub title: String, pub body: String }
pub fn classify(ev: &Event, cfg: &NotificationsConfig) -> Option<(NotifyKind, Item)>; // name may be "" (caller fills)
pub fn render(kind: NotifyKind, items: &[Item]) -> Notification;                      // items non-empty
pub struct Batcher { … }
impl Batcher {
    pub fn new() -> Self;                                         // QUIET, MAX_HOLD
    pub fn push(&mut self, kind: NotifyKind, item: Item, now: tokio::time::Instant);
    pub fn due(&mut self, now: tokio::time::Instant) -> Vec<Notification>;
    pub fn drain_all(&mut self) -> Vec<Notification>;
    pub fn next_deadline(&self) -> Option<tokio::time::Instant>;
}
pub trait Notifier: Send + Sync + 'static {
    /// Must return at once.
    fn notify(&self, n: &Notification);
    /// Waits at most `within` for queued popups to be handed to the system (blocking; called from spawn_blocking).
    fn flush(&self, _within: Duration) {}
}
#[doc(hidden)] #[derive(Default, Clone)] pub struct RecordingNotifier(Arc<Mutex<Vec<Notification>>>);
impl RecordingNotifier { pub fn take(&self) -> Vec<Notification>; }  // + impl Notifier (records)
```

- [ ] **Step 1: Write the failing tests**

In `config.rs` tests:
```rust
#[test]
fn config_notifications_defaults_and_overrides() {
    let _env = env_lock();
    let d = tempfile::tempdir().unwrap();
    let p = d.path().join("c.toml");
    std::fs::write(&p, "max_active = 5\n").unwrap();
    let n = Config::load(&p).unwrap().notifications;
    assert!(n.enabled && n.completed && n.failed && n.choose_files && !n.seeding_finished);
    std::fs::write(&p, "[notifications]\nenabled = false\nseeding_finished = true\n").unwrap();
    let n = Config::load(&p).unwrap().notifications;
    assert!(!n.enabled && n.seeding_finished && n.completed);
    std::fs::write(&p, "[notifications]\ncompleted = \"yes\"\n").unwrap();
    assert!(Config::load(&p).unwrap_err().to_string().contains("line 2"));
}
```
In puppy-proto's roundtrip list add `Event::ChoosingFiles { id: 4, name: "Big.Show.S01".into() }`.

In `notify/mod.rs` tests (helpers `item(id, name)`, `t0 = Instant::now()`; all times are explicit, no sleeping):
```rust
#[test] fn single_item_bodies() {
    // render(Completed, [item(1,"ubuntu-24.04.iso")]) == Notification{kind: Completed, title: "puppy", body: "Finished: ubuntu-24.04.iso"}
    // Failed, detail Some("connection reset"), name "big.tar" → "Failed: big.tar — connection reset"
    // ChooseFiles, id 4, "Big.Show.S01" → "Choose files: Big.Show.S01 — run puppy files 4 or press c in the dashboard"
    // SeedingFinished, "x.iso" → "Seeding finished: x.iso"
}
#[test] fn group_bodies() {
    // 3 Completed → "3 downloads finished"; 2 Failed → "2 downloads failed"
    // 2 ChooseFiles → "2 torrents waiting for files — run puppy ls or press c in the dashboard"
    // 2 SeedingFinished → "2 torrents finished seeding"
}
#[test] fn long_reason_is_shortened() {
    // detail = "x".repeat(500) + "\nsecond line": body == format!("Failed: a — {}…", "x".repeat(160)); never contains "second line"
    // detail "short\nmore" → "Failed: a — short"
}
#[test] fn burst_within_quiet_window_is_one_popup() {
    // push Completed ids 1,2,3 at t0, t0+1s, t0+2s
    // due(t0+4900ms) is empty; next_deadline() == Some(t0+5s)
    // due(t0+5s) == [render(Completed, …3 items)] (body "3 downloads finished"); then next_deadline() == None
}
#[test] fn kinds_group_separately() {
    // Completed 1,2,3 and Failed 4 all within t0..t0+1s; due(t0+10s) yields exactly
    // two popups: "3 downloads finished" and "Failed: d4 — boom" (order: by first event)
}
#[test] fn trickle_is_capped() {
    // for s in 0..30: push(Completed, item(s, …), t0+s secs); popups.extend(due(t0+s secs))
    // popups.extend(due(t0+40s)); assert popups.len() <= 3 and the counts parsed from the
    // bodies ("N downloads finished", or 1 for "Finished: …") sum to 30
}
#[test] fn same_download_twice_counts_once() {
    // push(Completed, item(7,"a")) twice at t0 → due(t0+3s) body "Finished: a"
}
#[test] fn drain_all_flushes_everything_now() {
    // pending Completed + Failed → drain_all() returns both; next_deadline() == None
}
#[test] fn classify_respects_config() {
    let cfg = NotificationsConfig::default();
    // Completed{id:1,name:"a"} → Some((Completed, Item{id:1,name:"a",detail:None}))
    // Failed{transient:false, reason:"boom"} → Some((Failed, detail Some("boom")))
    // Failed{transient:true, ..} → None
    // ChoosingFiles{id:4,name:"b"} → Some((ChooseFiles, …))
    // SeedingFinished{id:2} → None by default; Some((SeedingFinished, name "")) with seeding_finished = true
    // completed = false ⇒ Completed → None; Added/Removed/NeedsAttention/Snapshot/Unknown → None
    // enabled = false ⇒ everything None
}
```

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test -p puppy-daemon notify:: config_notifications && cargo test -p puppy-proto`
Expected: compile errors (`NotificationsConfig`, `notify`, `Event::ChoosingFiles` undefined).

- [ ] **Step 3: Implement**

`Event::ChoosingFiles` with a doc comment ("a torrent waits for its files to be chosen"). `NotificationsConfig` + `Config.notifications` as in Interfaces. `classify`/`render`/`Batcher`/`Notifier`/`RecordingNotifier` in `notify/mod.rs`. Batcher keeps, per kind in first-seen order, `{ items: Vec<Item>, first: Instant, last: Instant }`; a kind is due at `min(last + QUIET, first + MAX_HOLD)`; a pushed id already pending for that kind is ignored. Reason shortening (first line, `MAX_REASON` chars, `…`) lives in `render`. Fix any exhaustive `match` on `Event` the compiler reports (none expected outside proto tests).

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p puppy-daemon && cargo test -p puppy-proto`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/puppy-proto/src/lib.rs crates/puppy-daemon/src/{config.rs,lib.rs,notify/mod.rs}
git commit -m "feat(notify): config, ChoosingFiles event, grouping and popup text"
```

---

### Task 2: Daemon wiring

**Files:**
- Modify: `crates/puppy-daemon/src/manager/mod.rs` (Entry flag, `transition`, unit tests), `manager/restore.rs`, `manager/commands.rs` (`select_files`, `restart`), `manager/snapshot.rs` (`display_name`), `notify/mod.rs` (`spawn`, `NotifyTask`, tests), `lifecycle.rs`
- Modify tests: `crates/puppy-daemon/tests/lifecycle.rs`, and every other `DaemonOptions { … }` literal (`crates/puppy-client/tests/{client.rs,torrent.rs}`) gets `notifier: None`

**Interfaces:**
- Consumes: Task 1's `classify`, `Batcher`, `Notifier`, `RecordingNotifier`, `NotificationsConfig`, `Event::ChoosingFiles`.
- Produces:
```rust
// manager/snapshot.rs
impl Manager { pub fn display_name(&self, id: DownloadId) -> Option<String>; } // snap(id, e, None, false).0.name; None if unknown id
// notify/mod.rs
pub fn spawn(
    rx: broadcast::Receiver<Event>,
    lookup: impl Fn(DownloadId) -> Option<String> + Send + 'static,
    cfg: NotificationsConfig,
    sink: Arc<dyn Notifier>,
) -> NotifyTask;
impl NotifyTask { pub async fn finish(self, within: Duration); } // stop, send pending, sink.flush(within) via spawn_blocking, bounded by `within`
// lifecycle.rs
pub struct DaemonOptions { …, /// None: no notifications (the default; tests). pub notifier: Option<Arc<dyn Notifier>> }
const NOTIFY_FLUSH: Duration = Duration::from_secs(1);
```

- [ ] **Step 1: Write the failing tests**

`manager/mod.rs` tests (reuse `entry`, `drain`, the `Manager::start(Vec::new(), …)` setup of `seeding_transitions_emit_completed_then_seeding_finished`):
```rust
#[tokio::test] async fn choosing_files_emits_once() {
    // e = entry(Active); e.rec.name = Some("Big.Show.S01")
    // transition(3, e, ChoosingFiles) → drain == [Event::ChoosingFiles{id:3, name:"Big.Show.S01"}]
    // transition(3, e, Starting) then transition(3, e, ChoosingFiles) → drain == []
}
#[tokio::test] async fn restored_choosing_files_does_not_notify() {
    // Store pre-written with one magnet download, StoredState::ChoosingFiles; Manager::start;
    // take its entry, transition(id, e, ChoosingFiles) → drain has no ChoosingFiles event
}
```
`notify/mod.rs` tests (`#[tokio::test(start_paused = true)]`, a `broadcast::channel(16)`, `RecordingNotifier`):
```rust
async fn task_groups_and_names() {
    // send Completed{1,"a"}, Completed{2,""} (lookup(2) = Some("b")), Failed{7, transient:false, "boom"} (lookup(7) = None)
    // advance(2s): rec.take() empty; advance(2s): rec.take() == ["2 downloads finished", "Failed: download 7 — boom"] (bodies)
}
async fn task_skips_transient_and_disabled() {
    // Failed{transient:true} and Added → nothing after advance(15s); cfg.enabled=false → nothing for Completed
}
```
`tests/lifecycle.rs` (`Env::opts` gains `notifier: None`; add `Env::opts_notify(&self, sink: RecordingNotifier) -> DaemonOptions`):
```rust
#[tokio::test] async fn pending_notification_flushed_on_shutdown() {
    // start with a RecordingNotifier; add env.req(&s, "a.bin"); wait until Completed (get()),
    // shutdown(&env, d) at once (well within QUIET)
    // rec.take() == [Notification{kind: Completed, title: "puppy", body: "Finished: a.bin"}]
}
#[tokio::test] async fn permanent_failure_notifies_and_config_disables() {
    // url s.url("/missing") → after shutdown: one popup, body starts with "Failed: a.bin — "
    // second Env with config_file() = "[notifications]\nenabled = false\n": same flow, rec.take() is empty
}
```

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test -p puppy-daemon`
Expected: compile errors (`display_name`, `spawn`, `DaemonOptions::notifier` undefined).

- [ ] **Step 3: Implement**

- `Entry::choose_files_sent: bool` (false in `Entry::new`). In `transition`, `DownloadState::ChoosingFiles` arm: if `!e.choose_files_sent`, set it and emit `Event::ChoosingFiles { id, name: e.rec.name.clone().unwrap_or_default() }`.
- `restore.rs`: set the flag for every record whose stored state is `StoredState::ChoosingFiles`.
- `commands.rs`: clear it where `select_files` moves the entry to `Queued`, and where `restart` resets the entry.
- `notify::spawn`: one tokio task; `select!` on `rx.recv()` (classify → name = event name if non-empty, else `lookup(id)`, else `format!("download {id}")` → `batcher.push(kind, item, Instant::now())`), on `sleep_until(next_deadline)` (pending when `None`) → `sink.notify` each due popup, and on its stop token. `Lagged(n)` ⇒ `debug!` and continue; `Closed` ⇒ treat as stop.
- `run_daemon`: clone `cfg.notifications` before `Manager::start`; after it, if `cfg.notifications.enabled` and `opts.notifier` is `Some`, `notify::spawn(manager.events(), {weak manager}.display_name, …)` (hold a `Weak<Manager>` in the lookup so the task does not keep the manager alive). Graceful path: after `manager.shutdown().await`, `task.finish(NOTIFY_FLUSH).await`; the forced path skips it. Add `notifier` to the `Debug` impl as `is_some()`.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --workspace`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/puppy-daemon crates/puppy-client/tests
git commit -m "feat(notify): daemon notifier task, ChoosingFiles event emission"
```

---

### Task 3: Desktop sink, binary wiring, README

**Files:**
- Modify: `Cargo.toml` (`notify-rust = "4.18"` under `[workspace.dependencies]`), `crates/puppy-daemon/Cargo.toml` (`notify-rust.workspace = true`), `crates/puppy-daemon/src/notify/mod.rs` (`mod desktop; pub use desktop::DesktopNotifier;`, `desktop_wanted`), `crates/puppy/src/main.rs` (`daemon()`), `crates/puppy/tests/common/mod.rs` (`("PUPPY_NO_NOTIFY", "1")` in `vars()`), `README.md` (Configuration section, after `[torrent]`)
- Create: `crates/puppy-daemon/src/notify/desktop.rs` (tests inline)

**Interfaces:**
- Consumes: Task 1's `Notifier`, `Notification`; Task 2's `DaemonOptions::notifier`.
- Produces:
```rust
pub struct DesktopNotifier { … }
impl DesktopNotifier {
    pub fn new() -> Self;   // backend: notify_rust::Notification::new().appname("puppy").summary(&n.title).body(&n.body).show()
    pub(crate) fn with_backend(send: impl Fn(&Notification) -> Result<(), String> + Send + 'static) -> Self;
}
impl Notifier for DesktopNotifier {}  // notify: try_send on a sync_channel(QUEUE = 16), dropped if full; flush: close the queue, wait ≤ within for the worker
pub fn desktop_wanted(var: impl Fn(&str) -> Option<String>) -> bool; // false if PUPPY_NO_NOTIFY == "1" or PUPPY_TORRENT_TEST_MODE == "1"
```

- [ ] **Step 1: Write the failing tests**

`notify/desktop.rs`:
```rust
#[test] fn failing_backend_is_quiet() {
    // backend always Err("no D-Bus session"), counts calls in Arc<AtomicUsize>
    // notify ×3, flush(1s) → calls == 3; no panic; the worker's "logged once" AtomicBool is set
}
#[test] fn full_queue_drops_instead_of_blocking() {
    // backend blocks on a std mpsc Receiver until the test sends; notify ×100 returns within 100 ms
    // release; flush(1s) → calls <= QUEUE + 1
}
#[test] fn flush_waits_for_queued_popups() {
    // backend records bodies, sleeps 50 ms each; notify ×3; flush(2s) → all 3 recorded
}
```
`notify/mod.rs`:
```rust
#[test] fn desktop_wanted_reads_env() {
    // no vars → true; PUPPY_NO_NOTIFY="1" → false; PUPPY_TORRENT_TEST_MODE="1" → false; PUPPY_NO_NOTIFY="0" → true
}
```

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test -p puppy-daemon notify::`
Expected: compile errors (`DesktopNotifier`, `desktop_wanted` undefined).

- [ ] **Step 3: Implement**

The worker is a named std thread (`puppy-notify`) draining the channel; the first backend `Err` is logged with `tracing::debug!("desktop notifications unavailable: {e}")`, later ones silently. `show()`'s return differs per OS: map it with `.map(|_| ())` and `.map_err(|e| e.to_string())`. `flush` takes the sender out of a `Mutex<Option<SyncSender>>` and waits on a done channel with `recv_timeout(within)`. In `main.rs::daemon()`, pass `notifier: desktop_wanted(|k| std::env::var(k).ok()).then(|| Arc::new(DesktopNotifier::new()) as Arc<dyn Notifier>)`. README: the `[notifications]` block exactly as in the spec, one sentence each on grouping (3 s, at most 10 s), restart needed (`puppy stop`), `PUPPY_NO_NOTIFY=1`, and that a headless box without a notification server just shows none.

- [ ] **Step 4: Run full local verification**

Run: `cargo test --workspace && cargo clippy --workspace --all-targets -- -D warnings && cargo fmt --all --check`
Expected: all pass; `Cargo.lock` shows `notify-rust 4.18.x` and `zbus 5.x`, and no `dbus`/`libdbus-sys` (`grep -c '"libdbus-sys"' Cargo.lock` → 0).

- [ ] **Step 5: Manual check (Linux desktop; report the outcome, do not block on it)**

```bash
cargo build --release -p puppy
busctl --user status org.freedesktop.Notifications >/dev/null && echo server-ok
./target/release/puppy stop; ./target/release/puppy add <small-public-url>
# expect within ~3 s of completion: popup "puppy" / "Finished: <name>"
for i in 1 2 3; do ./target/release/puppy add <small-public-url> -o /tmp/p$i; done
# expect one popup "3 downloads finished"
./target/release/puppy add --choose '<multi-file magnet>'
# expect "Choose files: <name> — run puppy files <id> or press c in the dashboard"
PUPPY_NO_NOTIFY=1 ./target/release/puppy daemon   # (after puppy stop) downloads finish with no popup
```
macOS/Windows: same `puppy add`, expect a popup under a generic app name/icon (accepted in the spec).

- [ ] **Step 6: Commit**

```bash
git add Cargo.toml Cargo.lock crates/puppy-daemon crates/puppy/src/main.rs crates/puppy/tests/common/mod.rs README.md
git commit -m "feat(notify): native desktop notifications via notify-rust"
```
