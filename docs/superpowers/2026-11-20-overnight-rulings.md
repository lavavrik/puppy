# Overnight run — decisions made on your behalf

Every judgment call taken while running plans 1–4 autonomously (2026-11-19/20), copied from the execution ledgers. Each line: what was decided — why — what it costs if wrong. Review and overrule anything; these are not user decisions.

## Plan 1

- T6 adds `tokio-util` dependency — needed for CancellationToken — trivial if wrong.
- T4 TestServer adds `pub fn requests_seen(&self) -> Vec<http::HeaderMap>` (headers of every request, in order) — needed by probe_sends_context — trivial.
- Single-stream downloads (T7) still write the sidecar (total None / resumable false) so restore can report NotResumable — spec: "needs-attention offering restart" — small rework if wrong.
- Unknown-length restore → NeedsAttention{NotResumable} (restart is a user action in the daemon/UI plans); Review Focus #2's "restarts from zero" is superseded — spec says never silently continue/needs-attention — low cost.
- T8 kill simulation = run the first download on a separate tokio runtime in a std thread and `shutdown_background()` it, then drop all handles — portable — test-only.
- git identity set repo-locally (lavavrik / account email) since none configured — user can change.
- fold into Task 4 (filename handling lives there) — apply on all OSes for portability — small rework if wrong.
- T2 implemented 059a4ea (DONE_WITH_CONCERNS: steal_split min_segment=0 edge; Haiku trailer — Ruling: accept agent's own-model trailer, accurate attribution)
- T3 HttpResumeState::load validates segment invariants (start<=end, done<=end-start, sorted, non-overlapping) → Invalid; T7 clamps min_segment to >=1 — corrupt state can't reach arithmetic — low cost.
- SegmentShared uses Mutex<Bounds{end, claimed}>; worker claims each chunk under lock; `SegmentShared::try_steal(start, min_segment) -> Option<(u64,u64)>` is the ONLY way end shrinks — T7 must use it instead of steal_split on live segments (steal_split stays for pure planning) — correctness of core promise; cost: T7 interface change.
- accept worker change — If-Range matched yet 200 ⇒ server ignores ranges, not a change — reviewer to verify.
- accept rename-then-delete-sidecar order; T8 restore must treat (final exists, no .part, sidecar exists) as Completed and clean the sidecar — data-safe order.
- accept file split (plan ruling 11 allowed it).
- keep HEAD 0 only if fallback confirms (416 or total 0); else total unknown ⇒ single stream — spec: never silently wrong — cost: an extra single-stream for odd servers.
- add HttpResumeState.single_stream (serde default) now — T8 depends on it.
- client connect 15s + read 30s timeouts (config), validate_timeout_ms 60s, validation cancellable without lock — also protects workers from stalled bodies — cost: slow servers >30s idle get retried.
- one fix wave per final-fix-wave.md (all 7 Important + minors 8–18) — rulings for each item recorded in that file (AddOptions param; sidecar v2 without secrets + context_hash; rename_no_replace + DownloadStatus.path; http1_only; manual per-read timeouts) — cost if wrong: API churn before Plan 2, cheap now.
- carry the 2 minors (post-rename sidecar removal non-fatal; Windows rename retry on ERROR_SHARING_VIOLATION 32 / LOCK_VIOLATION 33) into Plan 2 Task 1 — no second fix wave allowed — trivial cost.

## Plan 2

- | T10 | PUPPY_IDLE_EXIT env read by spawner for tests | T9 spawn.rs must honour it — Ruling below |
- spawn_daemon_detached passes `--idle-exit $PUPPY_IDLE_EXIT` if that env var is set, else 30 — T10 tests need it — trivial.
- Plan 1 carry-over minors folded into T1 Step 0 — no second fix wave in plan 1 — trivial.
- add cancel-safe MsgReader; #[serde(other)] Unknown on Event/ReplyData; lenient parse_request helper; write-side MAX_LINE; boundary tests — protocol is a public contract, cheapest now — low cost.
- later tasks' tests that bind unix sockets must use short paths (e.g. /tmp/puppy-<rand>.sock) for macOS sun_path limit — carry into T7–T10 dispatches.
- Store::load returns Result; unreadable or un-quarantinable ⇒ Err ⇒ daemon refuses to start (T8 must surface a clear error) — never overwrite user state — cost: daemon won't start until user fixes permissions.
- parked — debounce error-path window (save_soon in the gap leaves state pending w/o task) — Ruling: minor, flush on shutdown/any later save_soon rescues; no data loss — fix opportunistically in final review.
- T5 implemented c5f140e (a6059d050416ac7f0). Ruling: Manager normalises a global limit of 0 to None (core TokenBucket treats 0 as unlimited) — carry into T6.
- Manager::start returns Result<Arc<Manager>, StoreError> (Store::load is fallible now) — T6 dispatch.
- accept "paused without path restores as Paused" (deviates from brief: brief said Queued) — user paused it; queuing would start unrequested traffic — low cost.
- carry T6 minors #1 (job spawned after shutdown took JoinSet — closed flag), #3 (set_limit skips persist on engine error), #5 (concurrent shutdown() returns early — make it idempotent-await) into T8 dispatch (lifecycle wires shutdown) — cheap, load-bearing for T8.
- fix the flaky T6 test in T8's dispatch along with T6 minors — flaky CI is load-bearing for the overnight run.
- T9 client must verify the daemon pipe/socket owner (Windows: GetNamedPipeServerProcessId + token user == current user; Unix: peer uid via SO_PEERCRED/getpeereid == geteuid) before sending requests with cookies — impostor-pipe risk — carry into T9.
- accept DaemonOptions{idle_exit, paths, http} + Paths::with_home for test isolation (plan said env-only) — avoids process-global env races — low cost.
- carry into T9: (a) before unlinking an existing unix socket, try connecting; if something answers ⇒ refuse (AlreadyRunning-like error) instead of deleting; only unlink actual sockets (file type socket) — regular files at the socket path are refused, not deleted (supersedes the brief's stale-regular-file test: change it to expect refusal); (b) client connect_or_spawn retries through the daemon-exit window (AlreadyRunning/connection refused) for up to 5 s; spawn uses setsid.
- fold T9 minors into T10: (1) torn write ⇒ drop writer half; (2) unparseable reply ⇒ parse req_id leniently & fail that call with Protocol; (3) spawn failure error mentions daemon log path; (5) spawned daemon cwd = data dir (unix "/" fallback); (8) add Client::take_events() -> Option<mpsc::Receiver<Event>> so callers can share Client in Arc (Plan 3 needs it).
- T10 implemented 34bae14..176aaa2 (a865174203a960254, opus); CI 44661776105 green 6/6. Ruling: accept connect_or_spawn(paths, name, exe, extra_env) signature change + pid file — test isolation and daemon log hint.
- one fix wave per final-fix-wave.md (Important 1–4 + minors 5–15). Important 5 (Plan 3 doc) fixed by controller directly in the plan doc. Key decisions: WireState/WireAttention proto mirrors with Unknown; Failed{transient} in core + manager auto-retry 30s→15min; parallel restore jobs; Store::close; refuse newer state.json; limiter 16 KiB slices — cost if wrong: moderate API churn now vs. later.
- carry residual minors into Plan 3 Task 1 Step 0: retry backoff resets when a retried download reaches Active/makes progress; arm_retry floor ≥1 s; restart's stale-file delete skips paths owned by another entry; EngineDetail #[serde(other)] Unknown; Windows default pipe name derived from PUPPY_HOME (hash) when set — cheap, user-visible.

## Plan 3

- | T2 → T4–T7 | App, Msg, Cmd, Mode, Popup, TextInput, History | Popup forms (AddForm etc.) defined in T5 but Popup enum in T2 references them — Ruling below |
- | T4 → T6 | hits HitMap recorded by draw | HitMap type not specified — Ruling below |
- | T2 Msg::Connected(Settings, Vec<…>) ↔ T7 connect() returns daemon_version too | App.daemon_version | Ruling below |
- T2 defines the form structs (AddForm, LimitForm, GlobalLimitsForm) with their fields and keyboard editing; T5 adds rendering, validation (parse_source) and submit — T2 forms need to exist for the Popup enum — low cost.
- `HitMap` (T2) = `Vec<(Rect, HitTarget)>` with `enum HitTarget { ListRow(DownloadId), FooterKey(char-or-KeyCode label), PopupCheckbox, PopupInside }`; draw clears and refills it each frame — T6 relies on it — low cost.
- `Msg::Connected { version: String, settings: Settings, downloads: Vec<DownloadSnapshot> }` and `DaemonApi::connect` returns `(String, Settings, Vec<DownloadSnapshot>)` — daemon version shown in title bar — low cost.
- Plan 2 residual minors folded into T1 Step 0.
- dispatch a dedicated debug fix (systematic debugging) for the macOS low-limit stall in parallel with T1 review — CI must be green; touches puppy-core only.
- carry into T4: (a) ANSI-16 dim/KeyLabel on selected row must stay visible (use fg/reversed on selection in ANSI mode); (b) honour WT_SESSION ⇒ truecolor on Windows Terminal (Theme::detect); (c) sanitize control chars (\t,\n,\r etc → space/�) in displayed names before truncate.
- continue with local Linux verification only; keep pushing branches; no more CI watching/diag branches; Windows/macOS verification deferred until user fixes billing (morning item: fix billing or make repo public, then re-run latest runs on plan-3-tui / plan-4).
- proto Event::Failed gains #[serde(default)] transient; TUI suppresses toasts for transient failures + dedupes live toasts — additive wire change — low cost.
- carry into T6: Ctrl+Alt+char bypass only inside text inputs (Normal mode must not fire actions on Ctrl+Alt+letter).
- carry into T4: widgets `put` resets the cell before writing (no style leakage onto selected rows); test with a CJK label straddling fill boundary.
- T4 implemented a457119 (a36f8d94ea6c14dc9). Ruling: runtime (T7) sets app.now_unix before Tick; accept Theme::for_selection, filter in title bar. Ambiguous-width icons risk → deferred.
- list bars carry no inner label; percent in a right-aligned 4-col slot — scanability — low cost.
- carry T6 minors into T7: clear hits on Msg::Resize; reset last_click on selection change/mode change; Ctrl+Alt guard in ConfirmDelete/Help popups; fix vacuous detail-pane wheel test (render at 120 cols).
- panic hook restores on any thread; any background-task panic (exec or connection) ends the dashboard with an error on a clean terminal — a toast is impossible once the terminal is restored — cost: a bug in a background task quits the UI (daemon unaffected).
- one fix wave per final-fix-wave.md — settings refresh via GetSettings; immediate FetchDetail + LRU detail cache; paint palette bg in truecolor (skip OSC 11 query — fragile); minors 4–12 incl. ErrorCode::Unsupported + torrent limit only before start — cost if wrong: painted bg overrides user's terminal background colour in truecolor mode.
- carry into Plan 4 Task 1 Step 0: sparkline heading shows the actual seconds displayed (`last N s`).

## Plan 4

- | T2 → T3–T5 | TorrentEngine::new(cfg, data_dir, default_dir, test_mode) — T5 adds DaemonOptions.torrent_test_peer; T2's test_mode must allow injecting initial peers — Ruling below |
- | T4 ↔ T5 | DownloadStatus.peers added in T5 but T3 status mapping would benefit — Ruling: add `peers` field in T3 |
- | T3 Added.path for magnet unknown until metadata | path = output folder; Ruling: Added.path = output folder (dir) until name known; status().path updates later |
- TorrentEngine::new takes `TorrentEngineOptions { cfg, data_dir, default_dir, test_mode: bool, test_initial_peers: Vec<SocketAddr> }` so tests/daemon can inject peers — low cost.
- `DownloadStatus.peers: Option<u32>` (#[serde(default)]) added in T3, not T5 — snapshot uses it — low cost.
- magnet Added.path = output folder until metadata yields a name; status().path reflects the final path later.
- puppy-torrent must not depend on puppy-daemon; TorrentEngineOptions carries plain fields (dht, upnp, listen_port, seed_ratio, data_dir, default_dir, test_mode, test_initial_peers); daemon maps its TorrentConfig into it — avoids a dependency cycle risk.
- validate every torrent file path component on ALL platforms (Normal components, no ':', no control chars, no reserved names); magnets resolved list_only first and validated before writing if librqbit allows — Windows is a v1 target — cost: some odd-but-legit torrents rejected.
- carry into T5: invalidate detail cache when stats.finished and cached bitfield isn't all-ones.
- engine-side run intent flag + per-engine serialization of pause/resume/unstick; torrent shutdown inside the 10 s budget; PUPPY_TORRENT_TEST_MODE env for e2e — cost: small complexity.
- one fix wave per final-fix-wave.md — overwrite only on Restore + unique folder when target exists; Engine::delete_completed + Engine::forget trait hooks; persist magnet metadata; lazy session; std TcpListener port probe; minors incl. lsd config, transient "engine unavailable", Windows name hardening — cost: Engine trait grows two default methods.
- deviate from "no second fix wave" — one targeted fix (data-loss, core promise, small) + scoped re-review: layout decided from the recorded path/source kind (never fall back to the download dir); refuse to traverse symlinks (symlink_metadata on each component under the output folder); plus the cheap resume-from-Error fix (delete(files=false) + re-add from stored copy with overwrite=true). — cost: slightly longer run; documented deviation.

## Process deviations

- Plan 4 got a second fix round after its final review (normally not allowed): the re-review reproduced user-file deletion in `delete_completed`; fixed in 0dffa84 and re-reviewed clean.
- GitHub Actions ran a 6-OS matrix on every push for Plans 1–3 and exhausted the account's Actions quota (private repo; macOS minutes ×10). CI is now manual-dispatch only (1d32ad4); everything after ~05:05 was verified locally on Linux only.

## Known gaps

- Windows and macOS: Plans 3–4 never ran there (CI was out of quota from Plan 3 on). Plans 1–2 passed on all 6 OS/arch runners.
- One full `cargo test --workspace` run out of 9 had a single failure that didn't reproduce in 8 reruns; the test wasn't identified.
- `docs/demo.tape` (vhs) hasn't been run; the README shows a text render instead of the GIF.
