# Torrents

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
  before there was one (after a restart, until the check is done) the map
  is synthesized from per-file progress, assuming each file fills in order.
  After a fast resume the exact map is back within moments.
- **Per-torrent speed limits** can only be set while the torrent is queued
  or waiting for its files to be chosen, before it starts (librqbit takes a torrent's limit only when it is added);
  later the dashboard says so and the limit is not kept. The global limits
  apply to torrents at any time.
- **After a daemon restart** a torrent goes on from the pieces it had when
  the daemon stopped (fast resume). At a clean stop (`puppy stop`, or any
  normal shutdown) the daemon flushes each torrent's files to disk and keeps
  its piece map and the size and modification time of its files
  (`torrents/<id>.resume`). It keeps none for a torrent whose file is shorter
  than the pieces it had, or was modified after the torrent last wrote to it
  (for example a seeding torrent's file edited by another program). On
  restore, if the files are exactly as recorded, up to 64 of the recorded
  pieces are read back and their hashes checked; the torrent then shows its
  progress at once instead of re-checking all its data. Otherwise it checks
  the data already on disk in full before it goes on, showing 0 until the
  check is done (a large library takes a while): after the daemon was killed
  or crashed, when there was no record, when a file was changed, moved,
  truncated or deleted while the daemon was down, when a checked piece is
  bad, or when the `.torrent` changed. A record is used once: after a
  restart, only the next clean stop writes a new one.
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
