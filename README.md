# puppy

A modern terminal download manager written in Rust, meant as a fresh successor
to [aria2](https://aria2.github.io/). It downloads over HTTP(S) today, with a
full-screen dashboard; BitTorrent is in progress. A background daemon
owns every download and the queue, so downloads keep going when no client is
open; `puppy` talks to it over a per-user local socket. It targets Linux, macOS
and Windows, on x86_64 and aarch64.

## Status

Early development. Done so far:

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

In progress: BitTorrent support (.torrent files and magnet links).
Scriptable subcommands (`puppy add`, `puppy ls`, …) and browser integration
come later.

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
puppy status          # print a one-line summary, e.g. "3 downloads, 1 active"
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
| `L`                               | Global speed limits |
| `+` / `-`                         | Raise / lower the number of active downloads |
| `Shift+↑` / `Shift+↓` (`K` / `J`) | Move in the queue |
| `/`                               | Filter |
| `Esc`                             | Close popup / clear filter |
| `?`                               | Help |
| `q` / `Ctrl+C`                    | Quit |

The footer shows the keys that apply to the current selection.

### Configuration

Settings go in `config.toml` (where it lives: see below). Keys:
`download_dir`, `connections` (default 8), `max_active` (default 3),
`global_down_limit`, `global_up_limit` (bytes per second), `theme`
(`"auto"`, `"dark"` or `"light"`; `auto` follows `COLORFGBG`, else dark) and
`proxy`. `NO_COLOR` switches to monochrome; without `COLORTERM=truecolor` the
16 ANSI colors are used. `--theme` overrides the file.

A daemon that `puppy` starts on demand exits after 30 seconds without
downloads or clients. To keep downloads going across reboots without opening
puppy, run the daemon as a login service (a systemd user unit, a launchd
LaunchAgent, or a Windows log-on task): see [docs/service.md](docs/service.md)
and the files in [packaging/](packaging/).

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
| `crates/puppy-core`       | Download engines (HTTP so far) behind a common `Engine` trait.  |
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
