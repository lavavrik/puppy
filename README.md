# puppy

A modern terminal download manager written in Rust: a fresh successor to
[aria2](https://aria2.github.io/), for HTTP(S) and BitTorrent. A background
daemon owns every download and the queue, so downloads keep going when the UI
is closed; `puppy` talks to it over a per-user local socket. It runs on Linux,
macOS and Windows, on x86_64 and aarch64.

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

In progress: the full-screen dashboard (until it lands, `puppy` prints a
one-line summary) and BitTorrent support (.torrent files and magnet links).
Scriptable subcommands (`puppy add`, `puppy ls`, …) and browser integration
come later.

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
puppy daemon   # run the daemon in the foreground (Ctrl-C stops it gracefully)
puppy          # connect to the daemon, starting one if needed, and print a summary
```

A daemon that `puppy` starts on demand exits after 30 seconds without
downloads or clients. To keep downloads going across reboots without opening
puppy, run the daemon as a login service (a systemd user unit, a launchd
LaunchAgent, or a Windows log-on task): see [docs/service.md](docs/service.md)
and the files in [packaging/](packaging/).

State lives in the platform data directory (`~/.local/share/puppy`,
`~/Library/Application Support/puppy` or `%APPDATA%\puppy`); the daemon's log
is `logs/puppyd.log` there. Settings go in `config.toml` in the config
directory (`~/.config/puppy` on Linux, the data directory elsewhere).

## Project layout

| Crate                     | What it is                                                      |
|---------------------------|-----------------------------------------------------------------|
| `crates/puppy`            | The `puppy` binary: the daemon subcommand and the client.       |
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
