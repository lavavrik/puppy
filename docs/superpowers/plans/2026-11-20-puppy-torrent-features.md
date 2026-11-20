# puppy torrent features Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Choose files of multi-file torrents once their contents are known (dashboard + CLI), seed after completion until a ratio or time limit, and show the exact piece map.

**Architecture:** New core/proto types first (states, selection, seed policy, requests). The torrent engine owns selection and seeding mechanics (librqbit `only_files`, cumulative upload counter, seed clock, `Api::api_dump_haves`) behind two new `Engine` trait methods. The daemon Manager handles the new states in its queue/slot logic and routes the new requests; the CLI and dashboard add their front-ends.

**Tech Stack:** Rust, librqbit 9.1 (puppy-torrent only), globset, humantime, ratatui.

**Spec:** `docs/superpowers/specs/2026-11-20-puppy-torrent-features-design.md` (every *call* there is binding).

## Global Constraints

- librqbit types appear only inside `crates/puppy-torrent`.
- Protocol changes are additive; new enum variants rely on the existing `#[serde(other)] Unknown` catch-alls; new struct fields use `#[serde(default)]`.
- Defaults: `[torrent] seed_ratio = 1.0`, `seed_time = "2h"`; `0` disables a trigger; both 0 ⇒ stop at completion.
- Indices: 1-based in CLI/UI, 0-based on the wire.
- `wait`/`add --wait` on Choosing files ⇒ exit 5 with "choose files: puppy select <id> …"; Seeding counts as finished (exit 0).
- Choosing files and Seeding never occupy an active slot.
- Deleting removes only selected files and puppy-created directories; never follow symlinks; never `remove_dir_all`.
- Tests are network-free (local seeder harness, `PUPPY_TORRENT_TEST_MODE=1`); local verification only — never trigger GitHub Actions.

## Review Focus

1. **Multi-file torrent with nested folders and same-named files in different folders** → picker and `puppy files` show full relative paths; selection by glob `"*.mkv"` matches across folders; only those files appear on disk. Test: Task 2 `select_glob_across_folders`.
2. **Daemon restart in each new state** (Choosing files, Seeding mid-time, Seeding after ratio already met) → Choosing stays Choosing with no files written; Seeding resumes with the remaining time and cumulative ratio; a met target stops immediately as Completed. Test: Task 3 `restart_restores_seeding_and_choosing`.
3. **Pausing / removing a torrent while it is Choosing files or Seeding** → pause on Seeding ⇒ Completed; remove with delete_files on Choosing files deletes nothing (nothing was written) and cleans stored copies. Test: Task 4 `pause_remove_in_new_states`.
4. **Selecting zero files / every file / indices out of range** → zero ⇒ BadRequest; all ⇒ same as no selection; out of range ⇒ BadRequest naming the index. Test: Task 2 `selection_validation`.
5. **Dashboard picker with hundreds of files and long paths** → scrolls, filter narrows, totals update, no panics at 60×16. Test: Task 6 `picker_large_list_and_small_screen`.

---

## File Structure

```
crates/puppy-core/src/types.rs          DownloadState::{ChoosingFiles, Seeding{..}}, FileChoice, Selection, SeedPolicy, TorrentFile; AddRequest.files/.seed
crates/puppy-core/src/engine.rs         Engine::{list_files, select_files, set_seed} (default Unsupported)
crates/puppy-proto/src/lib.rs           WireState mirrors; Request::{ListFiles, SelectFiles, SetSeed}; ReplyData::Files; Event::SeedingFinished
crates/puppy-torrent/src/selection.rs   resolve Selection against file list (indices + globset)
crates/puppy-torrent/src/seeding.rs     SeedTracker (cumulative uploaded, seeded_secs, targets, stop decision) + persistence sidecar
crates/puppy-torrent/src/engine.rs      choosing/seeding states, only_files add, Api piece map
crates/puppy-daemon/src/{config.rs, manager/*}   seed defaults, new states in scheduling/persistence/routing
crates/puppy/src/cli/{add.rs, files.rs, seed.rs, list.rs, wait.rs}
crates/puppy-tui/src/{app/*, ui/popup.rs, ui/picker.rs, ui/list.rs}
```

---

### Task 1: Core and protocol types

**Files:** `crates/puppy-core/src/{types.rs, engine.rs}`, `crates/puppy-proto/src/lib.rs`, `crates/puppy-daemon/src/config.rs`; tests inline.
**Produces:**
```rust
// types.rs
pub enum DownloadState { …, ChoosingFiles, Seeding { uploaded: u64, ratio: f32, seeded_secs: u64, ratio_target: Option<f32>, time_target_secs: Option<u64> } }
#[serde(tag="kind", rename_all="snake_case")] pub enum FileChoice { #[default] All, Ask, Select { selection: Selection } }
pub struct Selection { #[serde(default)] pub indices: Vec<usize>, #[serde(default)] pub globs: Vec<String> }
pub struct SeedPolicy { #[serde(default)] pub ratio: Option<f32>, #[serde(default)] pub time_secs: Option<u64> }
pub struct TorrentFile { pub index: usize, pub path: String, pub size: u64 }
// AddRequest gains: #[serde(default)] pub files: FileChoice, #[serde(default)] pub seed: Option<SeedPolicy>
// engine.rs (default impls return Unsupported):
async fn list_files(&self, id: DownloadId) -> Result<Vec<TorrentFile>, EngineError>;
async fn select_files(&self, id: DownloadId, selection: Selection) -> Result<(), EngineError>;
async fn set_seed(&self, id: DownloadId, policy: SeedPolicy) -> Result<(), EngineError>;
// proto: WireState::{ChoosingFiles, Seeding{…same fields}}; Request::ListFiles{id}, SelectFiles{id, selection}, SetSeed{id, policy}; ReplyData::Files{files: Vec<TorrentFile>}; Event::SeedingFinished{id}
// config: TorrentConfig.seed_ratio: f32 = 1.0 (was 0.0), seed_time: humantime Duration = 2h (serde via humantime-serde or a "2h" string)
```
- [ ] Tests: serde roundtrips for every new type/variant; older JSON without `files`/`seed` decodes with defaults; an older client decoding `choosing_files`/`seeding` gets `WireState::Unknown`; config defaults and `seed_time = "30m"` / `0`.
- [ ] Commit `feat(core,proto): types for file selection and seeding`.

### Task 2: Torrent engine — file selection

**Files:** `crates/puppy-torrent/src/{selection.rs, engine.rs, source.rs}`, tests `crates/puppy-torrent/tests/selection.rs`.
**Behaviour:** on add, `FileChoice::All` ⇒ today's flow; `Select` ⇒ resolve and validate (for `.torrent` up front ⇒ `EngineError::Invalid` naming bad index/pattern; for magnets after list-only ⇒ invalid ⇒ ChoosingFiles with reason stored); `Ask` ⇒ multi-file ⇒ state ChoosingFiles (single-file ⇒ All). Entering ChoosingFiles writes the stored `.torrent` copy immediately and adds nothing to librqbit. `list_files` works once contents are known (Choosing or later). `select_files` only in ChoosingFiles (else `Invalid("not choosing files")`); empty ⇒ Invalid; then real add with `only_files` (0-based) and persist the selection in `<data_dir>/torrents/<id>.selection` (JSON indices). Restore honours the persisted selection; Choosing files restores as Choosing. `delete_completed`/remove delete only selected files. Status sizes/progress count selected files only.
`pub fn resolve(files: &[TorrentFile], sel: &Selection) -> Result<Vec<usize>, String>` — union of indices and globset matches on the relative path (case-sensitive; `*` doesn't cross `/` unless `**`).
- [ ] Tests: `resolve_indices_and_globs`, `selection_validation`, `select_glob_across_folders` (nested + same names), `ask_waits_without_writing` (no files on disk, no librqbit torrent), `select_then_downloads_only_selected`, `magnet_invalid_preselection_goes_choosing`, `delete_only_selected_files`, `restore_choosing_and_selected`.
- [ ] Commit `feat(torrent): choose files once contents are known`.

### Task 3: Torrent engine — seeding policy and exact piece map

**Files:** `crates/puppy-torrent/src/{seeding.rs, engine.rs, poll.rs, map.rs}`, tests `crates/puppy-torrent/tests/seeding.rs`.
**Behaviour:** `TorrentEngineOptions` gains `seed_ratio: f32, seed_time: Duration` (global defaults); per-torrent `SeedPolicy` fields override (None ⇒ live global). On data completion: emit `Completed`, state Seeding (unless both effective targets are 0 ⇒ pause, Completed). `SeedTracker` keeps `base_uploaded` (persisted) + current librqbit uploaded; `seeded_secs` advanced by the poll task while Seeding, persisted every 30 s and on shutdown in `<data_dir>/torrents/<id>.seed`. Stop when `ratio ≥ target` (target>0) or `seeded_secs ≥ time` (time>0) ⇒ pause in librqbit, state Completed, emit `SeedingFinished`. `pause` on Seeding ⇒ Completed. `set_seed` changes targets live (re-evaluated next tick). Restore: Seeding restores live and continues with persisted counters; a met target ⇒ Completed at once. Piece map per spec (live: `Api::api_dump_haves` → MSB-first; paused: last live map; completed: selected-file pieces had; pre-live after restart: synthesis).
- [ ] Tests: `seeds_until_ratio` (two leechers or a leecher harness peer downloading from puppy), `seeds_until_time` (seed_time 1 s), `both_zero_stops_immediately`, `pause_while_seeding_completes`, `ratio_cumulative_across_restart`, `restart_restores_seeding_and_choosing`, `exact_piece_map_matches_api` (mid-download: puppy `have` == api dump), `piece_map_fallbacks`.
- [ ] Commit `feat(torrent): seeding policy and exact piece map`.

### Task 4: Daemon integration

**Files:** `crates/puppy-daemon/src/{lifecycle.rs, manager/*}`, `crates/puppy-daemon/tests/manager.rs` (or puppy-client torrent tests).
**Behaviour:** pass seed defaults into TorrentEngineOptions; Manager: ChoosingFiles and Seeding don't count as active; on `select_files` success the entry goes to the front of the queue (Queued) and schedule runs; persist both new states (StoredState variants) and restore them (Choosing ⇒ engine Restore keeps Choosing; Seeding ⇒ engine Restore continues seeding); route `ListFiles`/`SelectFiles`/`SetSeed` to the owning engine (HTTP ⇒ Unsupported); `Completed` event on data completion and `SeedingFinished` forwarded; snapshots carry the new states; pause on Seeding ⇒ Completed; remove works in all states. Seeding torrents count as work for idle exit (the daemon stays alive while seeding).
- [ ] Tests (daemon + client harness with the seeder): `choose_then_download_via_daemon`, `seeding_then_completed_via_daemon` (seed_time 1 s), `pause_remove_in_new_states`, `slots_ignore_choosing_and_seeding` (max_active 1: a Seeding torrent doesn't block the next download), `restart_keeps_new_states`.
- [ ] Commit `feat(daemon): file selection and seeding states`.

### Task 5: CLI

**Files:** `crates/puppy/src/cli/{add.rs, files.rs (new), seed.rs (new), list.rs, wait.rs}`, `src/main.rs`, `tests/cli.rs`, `README.md`.
**Commands:** `add --files SPEC (repeatable; "1,3-5" ranges or globs; 1-based) | --choose`, `--seed-ratio R --seed-time DUR`; `puppy files <id>` (`#  SIZE  PATH`, 1-based; `--json` ⇒ Files with 0-based `index` plus `number` 1-based); `puppy select <id> <SPEC>...`; `puppy seed <id> [--ratio R|default] [--time DUR|default]`. `ls`: state words `choosing`, `seeding`; `--state choosing,seeding`. `wait`: Choosing ⇒ exit 5 with hint; Seeding ⇒ 0. README: commands + seeding config.
- [ ] Tests: `add_files_spec_and_files_listing`, `choose_then_select`, `wait_on_choosing_exits_5`, `wait_seeding_exits_0`, `seed_command_overrides`, `ls_new_states`.
- [ ] Commit `feat(cli): torrent file selection and seeding commands`.

### Task 6: Dashboard

**Files:** `crates/puppy-tui/src/{app/mod.rs, app/forms.rs, app/keys.rs, ui/picker.rs (new), ui/popup.rs, ui/list.rs, ui/detail.rs, api.rs}`, tests `crates/puppy-tui/tests/{picker.rs, render.rs}`.
**Behaviour:** dashboard adds send `files: Ask`. When a torrent enters ChoosingFiles (snapshot/event) and no popup is open, fetch `ListFiles` and open the picker; Esc dismisses (stays Choosing); `c` reopens. Picker: folder tree (collapsible, ←/→), checkbox per file and per folder (tri-state), sizes, selected total in the title, `a` all / `n` none, `/` filter (substring on path), Space toggle, Enter confirm ⇒ `SelectFiles` (0 selected ⇒ inline error), mouse click toggles. Seeding popup `s` on a Seeding/Completed-capable torrent: ratio and time fields (prefilled from snapshot; empty ⇒ default) ⇒ `SetSeed`. List: icons `☐` choosing (warn), `⇅` seeding (ok); line 2 per spec. Footer hints `c choose files`, `s seeding`. Detail: seeding line. `can()` updated (pause valid for Seeding).
- [ ] Tests: `picker_renders_tree_and_totals`, `picker_toggle_folder_tristate`, `picker_filter`, `picker_confirm_emits_select`, `picker_zero_selected_error`, `picker_large_list_and_small_screen`, `auto_open_on_choosing`, `seed_popup_emits_set_seed`, `render_seeding_and_choosing_rows`; extend `demo_render_print` with a picker render.
- [ ] Full workspace tests, clippy, fmt. Commit `feat(tui): file picker and seeding controls`.
