# puppy — torrent file selection, seeding policy, exact piece map (design)

Date: 2026-11-20 · Status: approved in conversation, pending builder check

## Intent
Make torrents behave like a proper torrent client inside puppy: choose which files of a multi-file torrent to download, share back after finishing until a ratio or time limit, and show the real piece map. Both the dashboard and the CLI must support picking files.

## Goals
- File selection **once**, at the moment the torrent's contents are known (immediately for `.torrent`, after metadata for magnets); fixed afterwards.
- Seeding after completion, stopped by **ratio or time, whichever comes first**; global defaults + per-torrent override.
- Exact piece map from librqbit (`Api::api_dump_haves`) replacing the per-file estimate.

## Non-goals
Changing the selection after it's made; per-file priorities; streaming; seeding torrents puppy didn't download.

## Anti-goals
- A CLI-added torrent silently stuck waiting for a choice nobody will make.
- Seeding torrents taking download slots or hiding that a download is done (`wait` must succeed as soon as data is complete).
- Deleting or touching files that weren't selected / weren't created by puppy.

## Approaches
One sensible way: librqbit already supports `only_files` at add time and puppy already resolves magnets with `list_only` before adding, so selection slots in between "contents known" and "real add". Alternatives rejected: changing selection mid-download via librqbit's `update_only_files` (user explicitly wants a one-time choice; keeps deletion/restore simple); vendoring librqbit for the piece map (unneeded — `api_dump_haves` is public).

## Design

### File selection
- New download state **Choosing files** (`DownloadState::ChoosingFiles`, wire `choosing_files`; older clients see Unknown). Entered only for multi-file torrents when a choice is required; nothing is added to librqbit and nothing is written while choosing. Doesn't occupy an active slot.
- Who must choose:
  - Dashboard adds: always prompt (picker opens) for multi-file torrents.
  - CLI `add` without `--files`: download everything. `--choose` ⇒ wait in Choosing files. `--files SPEC` ⇒ pre-selected, no prompt.
  - `AddRequest` gains `#[serde(default)] files: FileChoice` where `FileChoice = All | Ask | Select(Selection)`; `Selection = { indices: Vec<usize>, globs: Vec<String> }` (union). Default `All` keeps older clients' behaviour.
- Protocol (additive): `Request::ListFiles{id}` ⇒ `ReplyData::Files{files: [TorrentFile{index, path, size}]}` (available once contents are known); `Request::SelectFiles{id, selection}` ⇒ Done (only valid in Choosing files; empty selection ⇒ BadRequest).
- After the choice: the real librqbit add uses `only_files`, layout/placement as today; the selection is persisted with the download (store + engine sidecar next to the stored `.torrent`), used by restore and by delete (only selected files are deleted).
- Restart while Choosing files: stays Choosing files (metainfo already stored).
- Size/progress shown are of the selected files only.
- Dashboard picker: popup with a folder tree, checkboxes, sizes, selected total, select-all/none, filter box (type to filter by name/extension), Enter to confirm, Esc to dismiss (torrent stays Choosing files; `c` reopens it). Mouse: click to toggle.
- CLI: `puppy add … --files 1,3-5 | --files "*.mkv"` (repeatable; numbers are 1-based indices shown by `puppy files`), `--choose`; `puppy files <id>` lists `#  SIZE  PATH`; `puppy select <id> <spec>...`. `--json` for both.

### Seeding
- Config `[torrent] seed_ratio = 1.0`, `seed_time = "2h"` (humantime-style durations; `0` disables that trigger; both 0 ⇒ stop immediately, today's behaviour). Replaces the reserved `seed_ratio = 0.0` semantics.
- Per torrent: `AddRequest.seed: Option<SeedPolicy{ratio: f32, time_secs: u64}>`; CLI `add --seed-ratio R --seed-time 2h`, `puppy seed <id> --ratio R --time T`; dashboard: a small popup (`s`) on a selected torrent; protocol `Request::SetSeed{id, ratio, time_secs}`.
- New state **Seeding** (`DownloadState::Seeding{ratio, seeded_secs, ratio_target, time_target_secs}` — or equivalent fields); counts as finished for `wait`, CLI (`done`-like exit 0) and the Completed event (emit `Completed` when data completes; emit a new `SeedingFinished{id}` event when seeding stops); does not occupy an active slot; subject to the global upload budget.
- Stop when `uploaded/selected_size ≥ ratio` (ratio > 0) or seeding time ≥ time (time > 0) ⇒ pause in librqbit, state Completed. User pause on a seeding torrent ⇒ Completed (stops seeding).
- Restart: Seeding torrents restore and continue seeding (after librqbit re-check) with the remaining time (seeded_secs persisted periodically).
- Ratio uses the max uploaded bytes seen across restarts (persisted), since librqbit resets on pause.

### Exact piece map
- TorrentEngine keeps a `librqbit::Api` over its session; `detail()` uses `api_dump_haves(id)` and converts to puppy's MSB-first `have` bitfield; for torrents with file selection, pieces outside the selection still show their real state. Drop the per-file synthesis (keep it only as fallback if the call fails).

### UI touches
- Dashboard list: icons `☐` (choosing files, warn) and `⇅` (seeding, ok); line 2 shows `choose files · c` / `↑ 1.2 MiB/s · ratio 0.42/1.0 · 1:12 left`. Footer hints include `c choose files` / `s seeding` when valid.
- CLI `ls` state words: `choosing`, `seeding`; `--state` filters for both.

## Constraints
- librqbit only inside puppy-torrent; protocol changes additive (new variants have catch-alls already); network-free tests with the local seeder harness; local verification only (no GitHub Actions).

## Left to the builder
Exact persistence format of the selection sidecar, picker tree rendering details, glob library choice (`globset` is fine), duration parsing (`humantime`).
