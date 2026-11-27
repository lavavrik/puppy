# Configuration

Settings go in `config.toml` (where it lives: see below). Keys:
`download_dir`, `connections` (default 8), `max_active` (default 3),
`global_down_limit`, `global_up_limit` (bytes per second), `theme`
(`"auto"`, `"dark"` or `"light"`; `auto` follows `COLORFGBG`, else dark) and
`proxy`. `NO_COLOR` switches to monochrome; without `COLORTERM=truecolor` the
16 ANSI colors are used. `--theme` overrides the file. `[ftp]` holds the TLS
setting for `ftp://` URLs (see [FTP](ftp.md)), `[ssh]` the agent and key file for
`sftp://` ones (see [SFTP](sftp.md)).

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
LaunchAgent, or a Windows log-on task): see [docs/service.md](service.md)
and the files in [packaging/](../packaging/).
