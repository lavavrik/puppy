# puppy — design

Date: 2026-11-19
Status: approved

## Intent

puppy is a modern terminal download manager written in Rust, positioned as a
fresh successor to aria2 (which is stable but barely maintained). It has two
jobs: be a tool the author actually uses daily, and be a strong portfolio
piece. Polish and visible craft matter, not just function.

## Goals (v1)

- HTTP/HTTPS downloads with multi-connection segmented transfer and robust
  resume.
- BitTorrent downloads (.torrent files and magnet links).
- A background daemon that owns all downloads and a queue.
- A polished full-screen TUI dashboard as the primary (and, in v1, only)
  user interface.

## Non-goals (v1, but the design must leave room for them)

- Scriptable CLI subcommands (`puppy add`, `puppy ls`, …). Planned next; the
  daemon API must be sufficient to implement them without changes.
- Browser download pickup (browser extension / native messaging). Will be
  another client of the daemon API.
- Other protocols (FTP, SFTP, metalink, …).
- A self-written BitTorrent engine. Planned as a later swap-in.

## Anti-goals

- A "conversational"/prompt-driven UI. The author explicitly prefers a
  btop/lazygit-style dashboard.
- Cramming everything into one process with no daemon — downloads must
  survive closing the UI.
- Silently corrupt output: a resumed or segmented file must never be wrong
  without the user being told.

## Constraints

- Language: Rust (stable).
- **Cross-platform (author requirement):** Linux, macOS and Windows, each on
  x86_64 and aarch64 — six release targets. No platform-specific code
  outside small `cfg`-gated modules; CI runs the test suite on all three OSes.
- Release binaries: built locally where possible and published to GitHub
  Releases; GitHub Actions is used for targets that can't reasonably be
  built locally (notably macOS). Exact release tooling is a later plan.
- Torrent engine: librqbit, used **only** behind puppy's own engine trait so
  it can be replaced by a home-grown engine later without touching the
  daemon, queue, or UI.
- TUI: ratatui (+ crossterm backend).

## Approaches considered

**Torrent engine**
1. Library only (librqbit) — fast to working torrents, but the flagship
   feature isn't the author's code.
2. Write our own — great portfolio value, weeks before reliable; magnet/DHT
   is large.
3. **Chosen: hybrid.** librqbit now, behind a clean `Engine` abstraction;
   own engine may replace it later. Effectively option 1 for v1.

**User interface**
- A. **Chosen: full-screen dashboard** (alternate screen, panels, hotkeys
  shown at the bottom, popups, mouse).
- B. Conversational inline UI with a prompt and slash commands (Claude
  Code-style) — rejected by the author.
- C. Dashboard-only with no future CLI — rejected; CLI commands come later.

**Process shape**
- **Chosen: daemon + clients over a local socket.** A single-process app
  would contradict the requirement that downloads outlive the UI; an
  HTTP/REST API would be heavier than a local tool needs.

## Design

### Components

1. **`puppyd` (daemon)** — owns engines, queue, scheduler, rate limiting,
   persistence. Single instance per user.
2. **`puppy` (TUI client)** — the dashboard. Running `puppy` with no
   arguments opens the dashboard; it auto-spawns the daemon if none is
   running. Later, `puppy <subcommand>` provides the scriptable CLI.
   (Whether these are one binary with a hidden `daemon` subcommand or two
   binaries is left to the builder.)
3. **Shared protocol crate** — request/response/event types used by daemon
   and all clients.

A Cargo workspace is the expected layout (e.g. `core`/engines, `daemon`,
`tui`, `proto`), exact split left to the builder.

### Daemon lifecycle (decided by the author)

- Preferred: run as a **login service** started at login and kept running,
  so downloads resume after reboot without opening the UI: a systemd user
  unit on Linux, a launchd LaunchAgent on macOS, and a per-user Startup /
  Task Scheduler entry on Windows. v1 ships these files and
  documents the manual install steps; a `puppy service install|uninstall`
  helper is a later nice-to-have.
- Fallback: if no daemon is running, the TUI spawns one detached. A daemon
  spawned this way **exits when idle** (no active or queued downloads and no
  connected clients for a short grace period). A daemon started as a login
  service never idle-exits.
- Single instance enforced with a lock file; stale sockets are detected and
  cleaned up on start.

### Daemon ↔ client protocol

- Transport: a per-user local socket — Unix domain socket on Linux/macOS
  (`$XDG_RUNTIME_DIR/puppy/puppy.sock`, else under the data dir), a named
  pipe on Windows (`\\.\pipe\puppy-<username>`). Use one cross-platform
  local-socket abstraction (e.g. the `interprocess` crate) so daemon and
  client code is identical across OSes. Access restricted to the current
  user (socket file 0600 / pipe default per-user ACL).
- Encoding: newline-delimited JSON messages (JSON-RPC-style requests with
  ids, plus server-pushed events).
- Requests (minimum): add (URL / magnet / .torrent path, optional output
  dir/filename, and for HTTP optional request context: extra headers,
  cookies, Referer, User-Agent, basic auth), list, get detail, pause, resume, remove (with/without
  deleting data), set global limit, set per-download limit, set queue
  concurrency, shutdown.
- Events: clients subscribe and receive state snapshots/deltas at a steady
  tick (~4–10 Hz) plus discrete events (added, completed, failed, removed).
- The protocol is the public contract for future CLI and browser
  integration — version it (a `hello` handshake with protocol version).

### Engine abstraction

A trait (`Engine`) implemented by `HttpEngine` and `TorrentEngine`
(librqbit). The daemon only talks to the trait. It must expose, per
download: lifecycle (start/pause/resume/cancel), status (state, total
size if known, downloaded bytes, speed, ETA), and an engine-specific
**detail view** used by the UI:

- HTTP: per-segment ranges and per-connection progress/speed.
- Torrent: piece availability bitfield (have/missing), peer count,
  seeds/leechers, upload speed, ratio.

Rate limiting (decided during builder check):
- **Global download limit** is one combined budget across all downloads,
  HTTP and torrent. The daemon divides it between its HTTP token bucket and
  librqbit's session limiter (re-balanced periodically by actual demand).
- **Global upload limit** is a separate setting, applying to torrents only.
- **Per-download limits**: required for HTTP. For torrents, supported only
  if librqbit exposes per-torrent limiting without wrapping its I/O;
  otherwise the UI shows per-download limit as unavailable for torrents
  (greyed out with a short reason). Keeping the `Engine` boundary thin
  matters more than this feature.

### HTTP engine

- Probe with HEAD (fall back to a ranged GET if HEAD is unsupported) for
  size, `Accept-Ranges`, `ETag`, `Last-Modified`, filename
  (`Content-Disposition`, else URL path).
- If ranges are supported and size is known: split into N segments
  (default **8** connections per download, configurable). Otherwise a single
  stream.
- Dynamic segmentation (stealing/splitting the largest remaining segment
  when a connection finishes early) is desirable; builder may ship static
  splitting first and add work-stealing after.
- Write into a preallocated `<name>.part` file at segment offsets; rename on
  completion.
- Resume: persist segment progress in a sidecar/state store. On resume,
  revalidate with `If-Range`/ETag/Last-Modified; if the remote changed,
  **do not silently continue** — mark the download as needing a restart and
  tell the user.
- No strong validator (no ETag/Last-Modified, or weak ETag only): on
  resume, and before splitting into segments, check that `Content-Length`
  is unchanged **and** re-fetch a small already-downloaded range (e.g. 64
  KiB at a few offsets) and compare bytes against the `.part` file. Match →
  resume normally. Mismatch → needs-attention, offering a restart. (Decided
  by the author.)
- Retries with exponential backoff per segment; a download fails only after
  a configurable retry budget.
- Redirects followed; HTTPS via rustls.
- Request context (headers, cookies, Referer, User-Agent, basic auth) from
  the `add` request is persisted with the download and replayed on every
  connection and on resume — needed for browser pickup later. State files
  holding it are created with 0600 permissions on Unix; on Windows they live in the
  per-user data dir whose default ACL is user-only. (Builder-check call.)
- Proxy: honour `HTTP(S)_PROXY`/`NO_PROXY` env vars and an optional
  `proxy` config key. (Builder-check call.)
- Crash safety: progress is flushed frequently enough that a crash loses
  at most a few seconds of work and never produces a file marked complete
  that isn't.

### Torrent engine

- librqbit session owned by the daemon; supports .torrent files and magnet
  links (DHT enabled).
- v1 behavior on completion: stop when done (seeding policy is a builder
  call, but default should be conservative — e.g. stop at completion or
  ratio 1.0).
- File selection within a multi-file torrent is **not** required in v1.

### Queue & scheduling

- Default max **3** concurrently active downloads; the rest wait in queue.
  Configurable at runtime from the UI.
- States: queued, active, paused, completed, failed (with reason),
  needs-attention (e.g. remote changed).
- Queue order is persisted; reordering from the UI is a nice-to-have.

### Persistence & config

- State in the platform data dir (Linux `~/.local/share/puppy`, macOS
  `~/Library/Application Support/puppy`, Windows `%APPDATA%\puppy`): the download list,
  queue order, per-download resume metadata. Storage format is a builder
  call (a single JSON/SQLite file are both acceptable) as long as writes are
  atomic.
- Config file `config.toml` in the platform config dir (Linux
  `~/.config/puppy`, macOS/Windows as for data): default download dir
  (default: the OS Downloads folder), connections per download, max active downloads,
  global speed limit, theme.
- Daemon restores all downloads on start; previously active downloads
  resume automatically.

### TUI dashboard

Full-screen (alternate screen), ratatui. Layout:

```
┌ Downloads ─────────────────────────────┬ Detail ──────────────────────┐
│▶1 ubuntu-24.04.iso  ███████████░░ 62%  │ ubuntu-24.04.iso             │
│    11.4 MB/s · 0:41                    │ 2.6 GB · 8 conns · 11.4 MB/s │
│ 2 debian-13 [T]     █░░░░░░░░░░░  8%   │ ▁▂▄▆▇▇█▆▇ speed (60s)        │
│    3.1 MB/s · 12:05 · 34 peers         │ segments:                    │
│ 3 big-file.zip      queued             │ ▮▮▮▮▮▮▯▯▮▮▮▮▯▯▯▯▮▮▮▮▮▮▮▮      │
│ ✓ archlinux.iso     done 1.1 GB        │ conn 1 ████████░ 1.6 MB/s    │
│                                        │ conn 2 █████░░░░ 1.4 MB/s    │
├────────────────────────────────────────┴──────────────────────────────┤
│ ↓14.5 MB/s ↑0.4 MB/s · 2 active · 1 queued · limit: none              │
│ a add  p pause  r resume  d delete  l limit  / filter  ? help  q quit │
└───────────────────────────────────────────────────────────────────────┘
```

- Download list: name, type badge (HTTP / [T]), smooth progress bar
  (sub-character precision), speed, ETA, status icon/color per state.
- Detail pane for the selected item: size, speed, ETA, connections or
  peers, a rolling speed sparkline/chart, and the segment strip (HTTP) or
  piece map (torrent).
- Footer: global throughput, counts, active limit, and context-sensitive
  hotkey hints.
- Popups/modals: add (paste URL, magnet, or .torrent path; optional output
  dir), set limit, confirm delete (with "also delete files" option), help.
- Keyboard-first (vim-style j/k plus arrows), mouse supported (select,
  scroll, click hotkey hints).
- Filter/search the list with `/`.
- Theme that reads well on both light and dark terminals; graceful
  degradation without truecolor; usable at 80×24, adapts to wide terminals
  (e.g. detail pane drops below the list when narrow).
- Shows a clear "connecting to daemon / daemon starting" state and recovers
  if the daemon restarts.
- Desktop notifications on completion are a nice-to-have.

### Quality bar

- Unit tests for segmentation, resume metadata, and state transitions.
- Integration tests for the HTTP engine against a local test server
  (ranges supported / unsupported, mid-download failure and resume, remote
  changed between sessions).
- Protocol round-trip tests.
- README with screenshots/asciinema-ready demo, since this is a portfolio
  piece.

## Left to the builder

- Exact crate split, async runtime (tokio expected — librqbit uses it).
- One binary vs two binaries for daemon and client.
- Storage format for state (JSON vs SQLite), given atomic writes.
- Static vs dynamic segmentation in the first milestone.
- Exact color palette and visual details within the layout above.
- Default seeding policy within the "conservative" guideline.
