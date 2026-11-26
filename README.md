# puppy

A modern terminal download manager written in Rust, meant as a fresh successor
to [aria2](https://aria2.github.io/). It downloads over HTTP(S) and BitTorrent
(magnet links and `.torrent` files), with a full-screen dashboard. A background daemon
owns every download and the queue, so downloads keep going when no client is
open; `puppy` talks to it over a per-user local socket. It targets Linux, macOS
and Windows, on x86_64 and aarch64.

## Status

Early development (v0.1.1). What works today:

- **HTTP(S) engine**: segmented multi-connection downloads (8 connections by
  default) with robust resume. A resumed file is revalidated against the
  server (ETag / Last-Modified, or a byte comparison when the server offers
  neither) and is never silently continued if the remote file changed.
  Retries with backoff, per-download and global speed limits, proxies.
- **Daemon**: a single instance per user owns the queue (3 active at a time by
  default), with atomically persisted state, a versioned JSON protocol,
  graceful shutdown, and an idle exit when started on demand.
- **Dashboard**: a full-screen terminal UI (`puppy`) to add, pause, resume,
  reorder, limit and delete downloads, with a live detail pane, dark and light
  themes, and monochrome / 16-color fallbacks.
- **BitTorrent**: magnet links and `.torrent` files (via
  [librqbit](https://github.com/ikatson/rqbit)), queued alongside HTTP
  downloads, with file selection, seeding limits, peers and a piece map. See
  [Torrents](#torrents).
- **Command line**: scriptable subcommands (`puppy add`, `ls`, `wait`, …) with
  `--json` output. See [Command line](#command-line).
- **Desktop notifications** when downloads finish or fail.
- **Browser extension** for Chrome-family browsers that hands their downloads
  to puppy. See [Browser downloads](#browser-downloads).

puppy is developed and tested on Linux. Release builds exist for macOS and
Windows, but they have seen little real use yet: reports are welcome.

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

## Install

Download the archive for your platform from the
[latest release](https://github.com/lavavrik/puppy/releases/latest):

| Platform              | Archive                                    |
|-----------------------|--------------------------------------------|
| Linux x86_64          | `puppy-<version>-x86_64-unknown-linux-gnu.tar.gz`  |
| Linux ARM64           | `puppy-<version>-aarch64-unknown-linux-gnu.tar.gz` |
| macOS Apple silicon   | `puppy-<version>-aarch64-apple-darwin.tar.gz`      |
| macOS Intel           | `puppy-<version>-x86_64-apple-darwin.tar.gz`       |
| Windows x86_64        | `puppy-<version>-x86_64-pc-windows-msvc.zip`       |
| Windows ARM64         | `puppy-<version>-aarch64-pc-windows-msvc.zip`      |

Each archive holds the `puppy` binary, the browser extension (`extension/`),
the service files (`packaging/`), this README and the licenses. The Linux
builds need glibc 2.35 or newer (Ubuntu 22.04, Debian 12, Fedora 36 and later).
HTTPS trusts the system's CA certificates; on a system without any (a minimal
container without `ca-certificates`), the daemon falls back to Mozilla's root
list built into puppy and says so in its log.
`SHA256SUMS` in the release lists every archive's checksum.

**Linux and macOS:**

```sh
curl -fsSL https://raw.githubusercontent.com/lavavrik/puppy/main/scripts/install.sh | sh
```

**Windows** (PowerShell):

```powershell
irm https://raw.githubusercontent.com/lavavrik/puppy/main/scripts/install.ps1 | iex
```

The scripts ([install.sh](scripts/install.sh), [install.ps1](scripts/install.ps1))
pick the archive for your machine from the latest release, check it against
its SHA-256 and install `puppy`:

| System  | Binary goes to                          | Archive unpacked in (extension, service files) |
|---------|-----------------------------------------|------------------------------------------------|
| Linux   | `~/.local/bin/puppy`                    | `~/.local/share/puppy-install/`                |
| macOS   | `~/.local/bin/puppy`                    | `~/.local/share/puppy-install/`                |
| Windows | `%LOCALAPPDATA%\Programs\puppy\puppy.exe`, added to your `PATH` | `%LOCALAPPDATA%\puppy-install\` |

Set `PUPPY_VERSION=0.1.0` to install a particular version. On Linux and macOS, if
`puppy` is not found afterwards, add `~/.local/bin` to your `PATH`.

**By hand:** download the archive from the release page, check it with
`shasum -a 256 -c <archive>.sha256`, unpack it and put `puppy` (`puppy.exe`)
anywhere on your `PATH`. The macOS and Windows binaries are not signed, so
when the archive comes from a browser, macOS refuses to run it at first (clear
the flag with `xattr -d com.apple.quarantine ~/.local/bin/puppy`) and
Windows SmartScreen may warn once. The install scripts download with
`curl` / PowerShell, which does not trigger either.

### Updates

puppy checks GitHub for a new release once a day and installs it by itself when
nothing is downloading and no dashboard, `puppy watch` or other client is open
(seeding does not count). Press Shift-U in the dashboard, or run `puppy update`,
to install right away; `puppy update --check` only looks. Your queue, settings
and downloads are kept, and a notification says "puppy updated to <version>".

- `[update]` in the config turns this off: `check = false` stops the daily
  check and automatic installs (puppy then only contacts GitHub when you run
  `puppy update`), `auto_install = false` only notifies (see Configuration).
- Development builds and `cargo install` builds do not update themselves; only
  the release binaries do.
- A folder puppy cannot write to (such as an older macOS install in
  `/usr/local/bin`) only gets a notification: run the install command again.

## Build from source

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
| `U`                               | Update puppy now (when the update banner shows) |
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
puppy add --checksum sha-256=9f86d0… URL    # check the finished file against this digest
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
puppy update --check                        # is a newer puppy out? (no install)
puppy update                                # install it; the daemon restarts into it
puppy stop                                  # stop the daemon
```

The text formats are for people and may change; scripts should use `--json`
(anywhere on the line), which prints the protocol's own types as exactly one
JSON value (`watch` excepted):

- `ls`: an array of download snapshots; `info`: `{"download":…,"detail":…}`;
  `limit --global`: the settings; `status`: `{"downloads":N,"active":N,"update":…}`;
  `update`: `{"status":…}` (the final update status).
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

`-H` and `--cookie` are sent to the URL you give and to redirect targets on
the same origin (scheme, host and port). A redirect to another origin gets no
cookies and no `Cookie`, `Authorization` or `Proxy-Authorization` header; the
referer, user agent and other headers still go along.

`ls --state` takes `queued`, `starting`, `active`, `verifying`, `paused`, `completed`
(completed only, not seeding), `done` (finished downloading: completed or
seeding), `choosing`, `seeding`, `failed` (for good), `retrying` and
`attention`. Piped `ls` output
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
ones `puppy files` prints. Every comma separates specs, even inside a glob, so
brace globs (`*.{mkv,srt}`) and names containing a comma cannot be written as
globs: use their numbers (or several globs). `0`, empty ranges (`5-2`) and
malformed ones (`1-3-5`, `-3`, `1-`; "not a number or range") are usage errors
(exit 2). A number past the last file: for `select`, and for `add` of a local
`.torrent`, the add or choice is refused (exit 2; `add` lists the download in
`failed` with `"code":2`); for a magnet the file list is not known yet when
adding, so the torrent waits in `choosing` with a note saying what did not fit,
for `puppy select`. `--files` and `--choose` cannot be
combined, and neither (nor `--seed-*`) applies to a non-torrent source. `puppy
files` on a download that is not a torrent, or one that already completed
(and so left the torrent engine), fails with exit 1 and the daemon's message.

`add --checksum ALGO=HEX` checks the finished file against a digest:
`md5`, `sha-1`, `sha-224`, `sha-256`, `sha-384` or `sha-512` (written as
aria2 and Metalink do, `sha-256=…`; `sha256=…` works too; algorithm and hex
digits in any case). It needs exactly one HTTP(S) source (also on stdin):
torrents are refused, since BitTorrent already checks every piece. A
malformed digest, an unknown algorithm or a wrong number of hex digits is a
usage error (exit 2). The dashboard's add form (`a`) has an optional
checksum field that takes the same spec.

Once every byte is downloaded, puppy hashes the whole file (also when it was
resumed, or finished before a restart) and shows the download as
`verifying` meanwhile, its progress counting the bytes hashed; it holds its
`max_active` slot until then. A match completes it. A mismatch fails it for
good (no automatic retry; `wait` exits 4) with the reason `checksum mismatch
(sha-256): expected <hex>, got <hex>`, and keeps the file where it is:
`resume` checks it again (for example after you replaced it), `restart`
deletes it and downloads it again, and `rm --delete-files` deletes it. If
the daemon stops before the check has passed, it checks the file again
after the next start, without the network. `pause` during the check stops it;
`resume` starts it over. `info` shows the digest (`checksum`) and the result
(`verified`: `yes`, `verifying (N%)`, `no` or `not yet`), the detail pane a
`Checksum` line; `--json` snapshots carry it as
`"checksum":{"algo":"sha-256","hex":"…"}` and the state `verifying`. A daemon
from before checksums cannot verify, so `add --checksum` against one
removes the download again and fails, asking you to `puppy stop` it.

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
| `4`  | waited download failed (including a checksum mismatch) |
| `5`  | waited download needs attention |
| `6`  | update not possible here (not a release build, or a folder puppy cannot write) |

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

**Upgrading:** `seed_time` is new and defaults to `2h`. A config written for
an older puppy with `seed_ratio = 0.0` (to stop at completion) therefore now
seeds for up to 2 hours; add `seed_time = 0` as well to keep stopping at
completion.

Desktop notifications come from the daemon, so they work with the dashboard and
terminal closed:

```toml
[notifications]
enabled = true
completed = true        # download finished (incl. a torrent entering seeding)
failed = true           # failed permanently (transient retries never notify)
choose_files = true     # torrent waiting for its files to be chosen
seeding_finished = false
```

Several popups of one kind are grouped into one ("3 downloads finished"): it is
sent after 3 seconds without another such event, and at most 10 seconds after the
first. Config is read when the daemon starts, so run `puppy stop` and let the next
command restart it. `PUPPY_NO_NOTIFY=1` turns popups off for a daemon. A headless
box or SSH session without a notification server simply shows none.

Updates are checked on GitHub and installed when puppy is idle:

```toml
[update]
check = true            # false: no daily check and no automatic install
auto_install = true     # install a found release when no download is running
```

With `check = false` puppy never contacts GitHub by itself, only when you run
`puppy update`, and never installs an update by itself, whatever `auto_install`
says.

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
  filters by path, `←`/`→` fold, `Enter` (or `y`) confirms; with a filter,
  `a`/`n` act on the shown files). `Esc` leaves it
  waiting; `c` reopens it. The folder layout is fixed when the torrent starts
  waiting: if a file of it appears on disk meanwhile, choosing fails and the
  torrent must be removed and added again.
- **Piece map**: the detail pane shows the exact set of verified pieces, read
  from librqbit while the torrent is live or paused. When librqbit has none
  (while it checks the data, or after an error): a complete torrent shows
  its selected pieces as had; otherwise the last exact map seen is shown;
  before there was one (after a restart, until the torrent goes live) the map
  is synthesized from per-file progress, assuming each file fills in order.
- **Per-torrent speed limits** can only be set while the torrent is queued
  or waiting for its files to be chosen, before it starts (librqbit takes a torrent's limit only when it is added);
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

## Browser downloads

An extension for Chrome, Chromium, Brave, Edge and Vivaldi hands the browser's
downloads to puppy. It talks to puppy through the browser's native messaging,
so no network port is opened. Firefox is not supported yet.

1. **Install the native host.** `puppy browser install` registers it for every
   browser found; `--browser chrome|chromium|brave|edge|vivaldi` picks one, and
   `--force` with it registers that browser even if it is not found.
   `puppy browser status` shows what is registered, and `puppy browser
   uninstall` removes it. Re-run `install` after moving the puppy binary
   (`status` then says "stale").
2. **Load the extension.** Open `chrome://extensions`, turn on Developer mode,
   choose Load unpacked and pick the `extension/` folder from the release
   archive (or from this repo). Keep the folder where it is: the browser loads
   it from there every time. The card must
   show the ID `cbfmabianfipnmdcdipmcpaggpejlaeg`.
3. **What happens.** Every download goes to puppy's download folder, sent with
   the browser's cookies, referrer and user agent. The browser's own copy is
   cancelled only after puppy accepted the download, and the browser keeps it
   if puppy does not answer within 10 s. If puppy accepts late, or the browser
   finishes first, puppy removes its own copy and keeps the browser's.
4. **Bypass and settings.** The toolbar popup has a "Let the browser take the
   next download" button; Alt+Shift+B does the same. It covers one download
   within 60 s. The options page sets a minimum size and the file types and
   sites to leave to the browser, and shows the connection status.
5. **Privacy.** A browser download's cookies are kept in puppy's private state
   file (mode 0600) and cleared when the download completes or is removed. They
   never reach logs, `puppy info`, `--json` output or the resume sidecar. An
   interrupted logged-in download resumes after a restart.

### Known limits

- `blob:`, `data:` and `file:` downloads stay in the browser.
- Incognito downloads and downloads started by other extensions stay in the
  browser.
- POST-backed (form) downloads can't be detected, and may fail in puppy. Use
  the bypass (toolbar button or Alt+Shift+B) for them.
- Cookies are read for the original URL only. If that URL redirects to another
  origin (a different scheme, host or port), the target gets none of them, and
  no basic-auth credentials or `Cookie`/`Authorization` headers either. A
  download whose redirect target needs a login fails in puppy.
- CHIPS partitioned cookies are not read below Chrome 119.
- "Ask where to save each file" is not mirrored: files go to puppy's folder.
- Re-downloading a completed download after a post-completion recheck failure
  goes without cookies.
- Flatpak and Snap browsers are not supported.
- On Windows, Vivaldi shares Chrome's native-messaging registration, and Brave
  is registered both under its own key and under Chrome's. Brave's location
  is unverified.
- The Windows registry code has been compile-checked only.

### Manual check (Chrome)

1. `cargo build --release -p puppy && ./target/release/puppy browser install && ./target/release/puppy browser status`: chrome shows `ok`.
2. Load the extension unpacked. The ID matches, and the options page status reads "Ready: ...".
3. Download a large public file. It leaves the browser's list, and `puppy ls` shows it under the same name.
4. Download something behind a login (for example a private GitHub release asset). It completes in puppy.
5. Press Alt+Shift+B, then download. It stays in the browser.
6. Run `puppy browser uninstall`, then download. It stays in the browser, and the popup lists it as failed with "not reachable".
7. Start a `blob:` download (for example "export" in a web app). It stays in the browser.
8. Add `pdf` to the file types. A PDF stays in the browser.
9. Midway through a logged-in download, run `puppy stop` then `puppy resume <id>`. It resumes.
10. Pausing during filename determination: start a download from a link that sets its name in a header (`Content-Disposition`). The browser shows no "Save as" and no partial file, and puppy gets the final name. With puppy unreachable the same download still completes in the browser.
11. Erase removes the entry: after puppy took a download, it is gone from `chrome://downloads` and the toolbar download bubble, not left as a cancelled entry.
12. Turn on "Ask where to save each file" in the browser settings and download. No save dialog flashes, and puppy gets the file.
13. Late accept: suspend the daemon (`kill -STOP <pid>`) so the browser's 10 s wait runs out, and download. The browser finishes the file itself. Resume the daemon (`kill -CONT <pid>`): puppy removes its own copy, keeping the browser's file.
14. Browser finished first: download a tiny file. The browser's copy completes and puppy ends up with no second copy of it (if puppy took one, it removes it).

## Project layout

| Crate                     | What it is                                                      |
|---------------------------|-----------------------------------------------------------------|
| `crates/puppy`            | The `puppy` binary: dashboard, subcommands and native host.     |
| `crates/puppy-tui`        | The full-screen dashboard (ratatui).                            |
| `crates/puppy-core`       | The `Engine` trait, shared types and the HTTP(S) engine.        |
| `crates/puppy-torrent`    | The BitTorrent engine (librqbit behind the `Engine` trait).     |
| `crates/puppy-daemon`     | The daemon: paths, config, queue manager, state store, server.  |
| `crates/puppy-proto`      | The daemon ↔ client protocol types and framing.                 |
| `crates/puppy-client`     | Client library: connecting, requests, events, auto-spawning.    |
| `crates/puppy-testserver` | An HTTP server for tests (ranges, throttling, failures).        |
| `extension/`              | The browser extension (MV3) that hands downloads to puppy.      |

## Releasing

1. Bump `version` in the root `Cargo.toml`.
2. Write the release notes in `docs/releases/v0.2.0.md` (what changed, how to
   install, known issues; [v0.1.1](docs/releases/v0.1.1.md) is the model).
   The release workflow refuses to run without this file. Commit both.
3. Tag and push: `git tag v0.2.0 && git push origin v0.2.0`.
4. The [Release workflow](.github/workflows/release.yml) runs the tests on
   every platform, builds the six archives and **publishes** the release with
   them, `SHA256SUMS` and the notes. Installs pick it up within a day. If any
   test or build fails, nothing is released: fix it and move the tag. A tag
   like `v0.2.0-rc.1` is published as a pre-release, which installs ignore.

The archives are built by `cargo build --profile dist` and
[`scripts/package.sh`](scripts/package.sh), which also run locally. The
workflow can be started from the Actions tab to build the archives without
making a release.

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in the work by you, as defined in the Apache-2.0 license, shall be
dual licensed as above, without any additional terms or conditions.
