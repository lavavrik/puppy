# puppy — desktop notifications (design)

Status: approved in conversation 2026-11-23 ("1 alone is fine, proceed").

## Intent

Start a download, walk away, and get a system popup when it finishes — even with the
dashboard and terminal closed. "Download finish is probably the most important one."
The main spec (2026-11-19) lists completion notifications as a nice-to-have; this
makes them real.

## Goals

- The **daemon** shows native desktop notifications, so they work whenever downloads run.
- Linux (freedesktop notifications over D-Bus: KDE/GNOME/etc. on CachyOS), macOS, Windows;
  x86 and ARM.
- Default events (user approved the defaults):
  - **on:** download finished; failed permanently (not retrying); torrent waiting for
    files to be chosen.
  - **off:** seeding finished; transient failures that will be retried.
- Bursts are grouped: several of the same kind within a short window ⇒ one popup
  ("3 downloads finished").
- A `[notifications]` config section turns it off or picks events.

## Non-goals

- No "run this command" hook (user chose option 1 alone).
- No dashboard-side popups, no sounds config, no click actions (open folder etc.) in v1.

## Anti-goals

- Never spam: a 50-file batch must not produce 50 popups.
- Never break or slow the daemon: a missing notification server (headless box, SSH,
  no D-Bus session) is silently ignored after one debug log line; sending never blocks
  the manager.
- Tests never pop real notifications.

## Approaches considered

1. **Daemon shows them (picked).** Always running while downloads run. Cost: on macOS a
   CLI binary's notifications appear under a generic app name/icon.
2. Dashboard only — misses the walk-away case.
3. Command hook — flexible, but per-OS wiring is the user's job. Declined for now.

## Decided how

- Library: `notify-rust` (D-Bus via zbus on Linux, `mac-notification-sys` on macOS,
  WinRT toast on Windows). Builder's call on exact version/features; pure-Rust D-Bus
  (no libdbus C dep) preferred.
- Config, all optional, in the existing TOML config:
  ```toml
  [notifications]
  enabled = true
  completed = true        # download finished (incl. a torrent entering seeding)
  failed = true           # failed permanently (transient retries never notify)
  choose_files = true     # torrent waiting for its files to be chosen
  seeding_finished = false
  ```
- Popup text: title "puppy", body e.g. `Finished: ubuntu-24.04.iso` / `3 downloads
  finished` / `Failed: big.tar — connection reset` / `Choose files: Big.Show.S01 —
  run puppy files 4 or press c in the dashboard`.

## Left to the builder

- Grouping window (suggest ~3 s, per kind) and how names are looked up for events that
  carry only an id.
- How "waiting to choose files" is detected (no event exists today: add one, or detect
  the state transition) — additive protocol changes only.
- A `Notifier` sink trait so tests record instead of popping; real sink disabled in
  test mode and via `PUPPY_NO_NOTIFY=1`.
- Whether config changes need a restart (the existing config loading decides).
