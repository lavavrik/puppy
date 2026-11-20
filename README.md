# puppy

A modern terminal download manager written in Rust, meant as a fresh successor
to [aria2](https://aria2.github.io/). It downloads over HTTP(S) and BitTorrent
(magnet links and `.torrent` files), with a full-screen dashboard. A background daemon
owns every download and the queue, so downloads keep going when no client is
open; `puppy` talks to it over a per-user local socket. It targets Linux, macOS
and Windows, on x86_64 and aarch64.

## Status

Early development, but the first four milestones are done:

- **HTTP(S) engine**: segmented multi-connection downloads (8 connections by
  default) with robust resume. A resumed file is revalidated against the
  server (ETag / Last-Modified, or a byte comparison when the server offers
  neither) and is never silently continued if the remote file changed.
  Retries with backoff, per-download and global speed limits, proxies from
  the environment.
- **Daemon**: a single instance per user with a download queue (3 active at a
  time by default), atomically persisted state, a versioned newline-delimited
  JSON protocol, graceful shutdown, and an idle exit when started on demand.

- **Dashboard**: a full-screen terminal UI (`puppy`) to add, pause, resume,
  reorder, limit and delete downloads, with a live detail pane, dark and light
  themes, and monochrome / 16-color fallbacks.
- **BitTorrent**: magnet links and `.torrent` files (via
  [librqbit](https://github.com/ikatson/rqbit)), queued alongside HTTP
  downloads, with peers, ratio and a piece map in the detail pane, and resume
  across daemon restarts (existing data is re-checked). See
  [Torrents](#torrents) for what v1 does and does not do.

Tested locally on Linux; CI for Windows and macOS is pending. Scriptable
subcommands (`puppy add`, `puppy ls`, …) and browser integration come later.

## Demo

A text render of the dashboard (120x32, dark theme, sample data):

```text
 puppy                                                                                      daemon v0.1.0   ● connected
╭ Downloads 7 ──────────────────────────────────────────────────────╮╭ Details ────────────────────────────────────────╮
│▌ ▶ 1 ubuntu-24.04.3-desktop-amd64.iso   █████████             45% ││ ubuntu-24.04.3-desktop-amd64.iso                │
│▌     11.4 MiB/s · 0:41 · 461.0 MiB / 1.0 GiB · limit 20.0 MiB/s   ││ /home/me/Downloads                              │
│  ▶ 2 debian-13.2.0-amd64-DVD-1.iso [T]  █████████             45% ││                                                 │
│      ↓ 11.4 MiB/s  ↑ 400.0 KiB/s · 0:41 · 461.0 MiB / 1.0 GiB     ││ Downloaded 461.0 MiB / 1.0 GiB (45%)            │
│  … 3 archlinux-2026.11.01-x86_64.iso    █████                 25% ││ Speed      11.4 MiB/s                           │
│      queued #1 · 256.0 MiB / 1.0 GiB                              ││ ETA        0:41                                 │
│  ✓ 4 Big Buck Bunny 4K.mkv              ████████████████████ 100% ││ State      ▶ active                             │
│      completed · 1.0 GiB                                          ││ Limit      20.0 MiB/s                           │
│  ↻ 5 big-dataset.tar.zst                █████                 25% ││ Source     https://exa…4.04.3-desktop-amd64.iso │
│      retry in 1:23 · connection reset by peer                     ││                                                 │
│  ! 6 photos.zip                         █████                 25% ││ speed · last 60 s               peak 17.0 MiB/s │
│      remote file changed · R restarts                             ││ ▅ ▆ █    ▂ ▃ ▅ ▆ █    ▂ ▃ ▅ ▆ █    ▂ ▃ ▅ ▆ █    │
│  ⏸ 7 notes.pdf                          █████                 25% ││ █▄█▆█▇ █ █▁█▃█▄█▆█▇ █ █▁█▃█▄█▆█▇ █ █▁█▃█▄█▆█▇ █ │
│      paused · 256.0 MiB / 1.0 GiB                                 ││ ██████▇████████████▇████████████▇████████████▇█ │
│                                                                   ││                                                 │
│                                                                   ││ Segments              2 connections · resumable │
│                                                                   ││ █████████████████████████████▒░░░░░░░░░░░░░░░░░ │
│                                                                   ││                                                 │
│                                                                   ││ conn 1 ███████▌                       3.0 MiB/s │
│                                                                   ││                                                 │
│                                                                   ││                                                 │
│                                                                   ││                                                 │
│                                                                   ││                                                 │
│                                                                   ││                                                 │
│                                                                   ││                                                 │
│                                                                   ││                                                 │
│                                                                   ││                                                 │
╰───────────────────────────────────────────────────────────────────╯╰─────────────────────────────────────────────────╯
 ↓ 14.0 MiB/s  ↑ 400.0 KiB/s · 2 active · 1 queued · 1 paused · 1 done · 2 failed · limit: none · max 3   ▆▆▇▇█▄▄▅▅▆▆▇▇
 a add  p pause  d delete  l limit  / filter  ? help  q quit
```

Animated demo: run `vhs docs/demo.tape` (needs [vhs](https://github.com/charmbracelet/vhs),
`python3` and `puppy` on your `PATH`). It runs in throwaway directories and writes
`docs/demo.gif`.

## Build

Requires a recent stable Rust toolchain (edition 2024).

```sh
cargo build --release
```

The binary is `target/release/puppy`. To install it into `~/.cargo/bin`:

```sh
cargo install --path crates/puppy
```

## Run

```sh
puppy                 # open the dashboard, starting a daemon if none is running
puppy status          # print a one-line summary (more commands: "Command line" below)
puppy daemon          # run the daemon in the foreground (Ctrl-C stops it gracefully)
puppy --theme light   # override the theme: auto, dark or light
```

`puppy` needs an interactive terminal (it exits with status 2 otherwise). Quitting
the dashboard only disconnects; the daemon and its downloads keep running.

### Keys

| Key                               | Action |
|-----------------------------------|---------------------------------------------|
| `j` / `↓`, `k` / `↑`              | Move down / up |
| `g` / `Home`, `G` / `End`         | Jump to top / bottom |
| `a`                               | Add a download |
| `p`, `r`                          | Pause, resume |
| `d` / `Delete`                    | Delete (asks to confirm) |
| `R`                               | Restart (failed or needs-attention only) |
| `l`                               | Speed limit for the selected download |
| `c`                               | Choose a torrent's files (when it waits in `choosing`) |
| `s`                               | Seeding ratio / time for a torrent |
| `L`                               | Global speed limits |
| `+` / `-`                         | Raise / lower the number of active downloads |
| `Shift+↑` / `Shift+↓` (`K` / `J`) | Move in the queue |
| `/`                               | Filter |
| `Esc`                             | Close popup / clear filter |
| `?`                               | Help |
| `q` / `Ctrl+C`                    | Quit |

The footer shows the keys that apply to the current selection.

### Command line

Every command talks to the daemon, starting one if none runs (except `stop`).
Text goes to stdout; errors, and the progress line of `wait` (only on a
terminal), go to stderr.

```sh
puppy add https://example.com/a.iso         # add a URL, magnet link or .torrent path
puppy add -o ~/isos --limit 2M URL1 URL2    # into a folder, at most 2 MiB/s each
puppy add --name b.iso --paused URL         # under another name, without starting
puppy add -H "Authorization: Bearer x" --cookie "a=b" --referer URL URL
puppy add - < urls.txt                      # one source per line from stdin (# comments)
puppy add --wait URL                        # add, then wait like `puppy wait`
puppy add --files 1,3-5 --files '*.mkv' T   # torrent: only these files (1-based numbers, ranges, globs)
puppy add --choose T                        # torrent: wait for `puppy select` to pick the files
puppy add --seed-ratio 2 --seed-time 6h T   # torrent: seeding limits for this one (`default` inherits)
puppy files 3                               # a torrent's files: `#  SIZE  PATH`, numbered from 1
puppy select 3 2-4 'docs/*'                 # choose files of a torrent that is waiting for it
puppy seed 3 --ratio 3 --time 0             # change when torrent 3 stops seeding
puppy status                                # "3 downloads, 1 active"
puppy ls                                    # table of downloads
puppy ls --state active,queued              # only these states
puppy info 3                                # everything about download 3
puppy pause 3 4                             # pause downloads (ids, or `all`)
puppy resume all                            # resume paused or failed downloads
puppy restart 3                             # start a failed / needs-attention download over
puppy rm 3 --delete-files                   # remove, and delete its files
puppy move 5 0                              # move a queued download to the front
puppy limit 3 500K                          # speed limit for one download (`none` lifts it)
puppy limit --global --down 5M --up 1M      # global limits; `limit --global` shows them
puppy max-active 2                          # run at most 2 downloads at once
puppy wait 3 4                              # wait until they complete, fail or need attention
puppy watch                                 # print events as they happen, until Ctrl-C
puppy stop                                  # stop the daemon
```

The text formats are for people and may change; scripts should use `--json`
(anywhere on the line), which prints the protocol's own types as exactly one
JSON value (`watch` excepted):

- `ls`: an array of download snapshots; `info`: `{"download":…,"detail":…}`;
  `limit --global`: the settings; `status`: `{"downloads":N,"active":N}`.
- `add`: `{"added":[{"id","source"}…],"failed":[{"source","error","code"}…]}`;
  with `--wait` also `"final"` (the snapshots once done) and `"missing"`
  (ids that no longer exist).
- `wait`: `{"final":[…snapshots…],"missing":[ids…]}`.
- `pause`, `resume`, `restart`, `rm`, `move`, `limit <ID>`: one result per id,
  e.g. `[{"id":3,"ok":true},{"id":"abc","ok":false,"error":"…","code":2}]`.
- `files`: `[{"index","number","path","size"}…]` (`index` is the protocol's
  0-based one, `number` the 1-based one the CLI uses).
- `select`: `{"id","selection":{"indices","globs"}}` (0-based indices);
  `seed`: `{"id","policy":{"ratio","time_secs"}}`.
- `stop`: `{"stopped":true}` or `{"running":false}`.
- `watch --json` prints NDJSON: one event per line (`snapshot`, `added`,
  `completed`, `failed`, `removed`, `needs_attention`), snapshots included, so
  `puppy --json watch | jq` works; without `--json` it prints lines like
  `14:02:11 completed 3 a.iso` and leaves out snapshots.

`ls --state` takes `queued`, `starting`, `active`, `paused`, `completed` (or
`done`; completed only, not seeding), `choosing`, `seeding`, `failed` (for
good), `retrying` and `attention`. Piped `ls` output
(no terminal, no `COLUMNS`) never truncates names. `add -` reads UTF-8 or
UTF-16 (with a BOM) from stdin. If an incompatible puppy daemon (another
version) is running, commands say so; `puppy stop` stops it.

`wait` treats a transient failure (e.g. the server answering 503) as not yet
ended, since puppy retries it on its own: it keeps waiting and shows
`retrying in m:ss`. Ctrl-C stops waiting (exit 130); the downloads keep going.

A torrent that is waiting for you to pick its files (`add --choose`, or a
magnet whose files are not known until its metadata arrives) shows as
`choosing`; `wait` and `add --wait` end at once with exit 5 and the hint
`choose files: puppy select <id> …` (so `add --choose --wait` always exits 5).
A torrent that has finished downloading and is uploading shows as `seeding`
(100%), and counts as finished for `wait` (exit 0). A note puppy attaches to a
download (for example a file pattern that matched nothing) appears after the
name of a `choosing` row in `ls`, and as `note` in `info`.

File specs (`add --files`, `select`) are numbers (`3`), ranges (`2-5`) and
globs (`*.mkv`, `Season 1/*`), separated by commas (a space belongs to the
glob; `select` also takes several arguments); numbers are the 1-based
ones `puppy files` prints. `0`, empty ranges (`5-2`) and malformed ones (`1-3-5`, `-3`, `1-`; "not a
number or range") are usage errors (exit 2), as are numbers past the last
file; for a magnet the file list is not known when
adding, so the daemon rejects those (`add` then exits 2 too, with `"code":2` in `failed`). `--files` and `--choose` cannot be
combined, and neither (nor `--seed-*`) applies to a non-torrent source. `puppy
files` on a download that is not a torrent, or one that already completed
(and so left the torrent engine), fails with exit 1 and the daemon's message.

`puppy seed <id>` **replaces the torrent's whole seeding policy**: `--ratio R`
and `--time DUR` (`90m`, `2h`, ...) set a limit, `0` turns that limit off, and
`default` (or leaving the option out) goes back to the daemon's `seed_ratio` /
`seed_time`. At least one of the two options is required.

Exit codes (with several ids, the highest wins):

| Code | Meaning |
|------|---------|
| `0`  | ok |
| `1`  | error (daemon or operation) |
| `2`  | usage |
| `3`  | unknown id |
| `4`  | waited download failed |
| `5`  | waited download needs attention |

### Configuration

Settings go in `config.toml` (where it lives: see below). Keys:
`download_dir`, `connections` (default 8), `max_active` (default 3),
`global_down_limit`, `global_up_limit` (bytes per second), `theme`
(`"auto"`, `"dark"` or `"light"`; `auto` follows `COLORFGBG`, else dark) and
`proxy`. `NO_COLOR` switches to monochrome; without `COLORTERM=truecolor` the
16 ANSI colors are used. `--theme` overrides the file.

Torrents have their own section:

```toml
[torrent]
dht = true            # find peers through the DHT
upnp = false          # ask the router to forward listen_port (UPnP)
lsd = true            # find peers on the local network (local service discovery)
listen_port = 42069   # incoming peer connections; an OS-assigned port if busy
seed_ratio = 1.0      # stop seeding at this upload/download ratio; 0 = never
seed_time = "2h"      # ... or after seeding this long; 0 = never
```

A finished torrent keeps seeding until the ratio or the time is reached,
whichever comes first; with both at `0` it stops at completion, as it did
before seeding existed. Both can be set per torrent (`puppy add --seed-ratio
… --seed-time …`, `puppy seed`), and are shown by `puppy info`.

A daemon that `puppy` starts on demand exits after 30 seconds without
downloads or clients. To keep downloads going across reboots without opening
puppy, run the daemon as a login service (a systemd user unit, a launchd
LaunchAgent, or a Windows log-on task): see [docs/service.md](docs/service.md)
and the files in [packaging/](packaging/).

### Torrents

Add a magnet link or the path of a `.torrent` file in the dashboard's add
form (`a`). A `.torrent` is copied into the daemon's data directory
(`torrents/<id>.torrent`) so the download can be restored after a restart
even if the original is moved; a magnet's metadata is kept there too once it
has been fetched, so a restart needs no network to restore it. A single-file
torrent goes straight into the download directory; a multi-file torrent, or
any magnet, into its own folder there. Existing files are never overwritten:
a taken folder name gets a counter (`Show (1)`), and a single file whose name
is taken goes into a folder of its own (`file.iso (1)/file.iso`). The torrent
engine starts (and opens its ports) only once the first torrent is added.
Things to know about v1:

- **Seeding**: a finished torrent seeds (state `seeding`) until `seed_ratio`
  or `seed_time` is reached, then shows as completed. Seeding torrents never
  occupy one of the `max_active` download slots.
- **Choosing files**: `puppy add --files SPEC` downloads only some files of a
  torrent; `--choose` (or the file picker) waits for you in the `choosing`
  state first. See "Command line". Torrents added from the dashboard always
  ask: once a multi-file torrent's contents are known the file picker opens
  (a folder tree with checkboxes; `Space` toggles, `a`/`n` all/none, `/`
  filters, `←`/`→` fold, `Enter` on a file or `y` confirms). `Esc` leaves it
  waiting; `c` reopens it. The folder layout is fixed when the torrent starts
  waiting: if a file of it appears on disk meanwhile, choosing fails and the
  torrent must be removed and added again.
- **Piece map is an approximation**: the detail pane's piece map is derived
  from per-file progress (assuming each file fills in order), not from the
  exact set of verified pieces.
- **Per-torrent speed limits** can only be set while the torrent is queued,
  before it starts (librqbit takes a torrent's limit only when it is added);
  later the dashboard says so and the limit is not kept. The global limits
  apply to torrents at any time.
- **After a daemon restart** every unfinished torrent re-checks the data
  already on disk before it goes on (there is no fast resume); its progress
  shows 0 until the check is done, and a large library takes a while.
- **Deleting a torrent's files** (also once it has completed) deletes exactly
  the files the torrent lists, then its folders once empty; anything else in
  them stays. A torrent whose `.torrent` file is gone (shown as "torrent file
  missing") leaves its files on disk, even when asked to delete them.
- **Choosing only some files**: librqbit still creates every file of the
  torrent, so unselected files show up as empty placeholders. A placeholder
  also gets the bytes of a piece it shares with a selected file. Deleting
  removes the selected files and the placeholders puppy's download created, as
  long as they are unchanged; a file that was already there, or that you
  replaced or edited, stays. After a restart, a file that was already at an
  unselected path (one puppy did not create) may still get those shared-piece
  bytes written into it when the torrent goes on.
- **DHT is on by default, UPnP is off.** Trackers and local peer discovery are
  used too (`lsd = false` turns the latter off). If the torrent engine cannot
  start, torrents fail with "torrent engine unavailable (see daemon log)" and
  are retried automatically.

State lives in the platform data directory (`~/.local/share/puppy`,
`~/Library/Application Support/puppy` or `%APPDATA%\puppy`); the daemon's log
is `logs/puppyd.log` there. `config.toml` lives in the config directory
(`~/.config/puppy` on Linux, the data directory elsewhere). Setting
`PUPPY_HOME=<dir>` relocates both (config in `<dir>/config`, data in `<dir>/data`).

## Project layout

| Crate                     | What it is                                                      |
|---------------------------|-----------------------------------------------------------------|
| `crates/puppy`            | The `puppy` binary: dashboard, `status` and `daemon`.           |
| `crates/puppy-tui`        | The full-screen dashboard (ratatui).                            |
| `crates/puppy-core`       | The `Engine` trait, shared types and the HTTP(S) engine.        |
| `crates/puppy-torrent`    | The BitTorrent engine (librqbit behind the `Engine` trait).     |
| `crates/puppy-daemon`     | The daemon: paths, config, queue manager, state store, server.  |
| `crates/puppy-proto`      | The daemon ↔ client protocol types and framing.                 |
| `crates/puppy-client`     | Client library: connecting, requests, events, auto-spawning.    |
| `crates/puppy-testserver` | An HTTP server for tests (ranges, throttling, failures).        |

The design is in [docs/superpowers/specs/](docs/superpowers/specs/).

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in the work by you, as defined in the Apache-2.0 license, shall be
dual licensed as above, without any additional terms or conditions.
