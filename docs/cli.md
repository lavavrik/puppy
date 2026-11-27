# Command line

Every command talks to the daemon, starting one if none runs (except `stop`).
Text goes to stdout; errors, and the progress line of `wait` (only on a
terminal), go to stderr.

```sh
puppy add https://example.com/a.iso         # add a URL, magnet link or .torrent path
puppy add ftp://ftp.example.org/pub/a.iso   # FTP (anonymous); ftps:// and ftpes:// for FTPS
puppy add sftp://me@host.example/srv/a.iso  # SFTP (ssh-agent or ~/.ssh keys)
puppy ssh trust host.example:2222           # show an SSH host's key and trust it
puppy add -o ~/isos --limit 2M URL1 URL2    # into a folder, at most 2 MiB/s each
puppy add --name b.iso --paused URL         # under another name, without starting
puppy add -H "Authorization: Bearer x" --cookie "a=b" --referer URL URL
puppy add - < urls.txt                      # one source per line from stdin (# comments)
puppy add -i list.txt                       # an aria2-style input file, with per-download options
puppy add --wait URL                        # add, then wait like `puppy wait`
puppy add --checksum sha-256=9f86d0… URL    # check the finished file against this digest
puppy add --mirror URL2 --mirror URL3 URL   # one file from several places at once
puppy add -o ~/isos a.iso.meta4             # every file of a metalink (file or URL)
puppy add --metalink 'https://mirrors.example/metalink?repo=x'  # a URL that serves one
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
puppy completions zsh                       # print a shell completion script
```

The text formats are for people and may change; scripts should use `--json`
(anywhere on the line), which prints the protocol's own types as exactly one
JSON value (`watch` excepted):

- `ls`: an array of download snapshots; `info`: `{"download":…,"detail":…}`;
  `limit --global`: the settings; `status`: `{"downloads":N,"active":N,"update":…}`;
  `update`: `{"status":…}` (the final update status).
- `add`: `{"added":[{"id","source"}…],"failed":[{"source","error","code"}…]}`;
  with `--wait` also `"final"` (the snapshots once done) and `"missing"`
  (ids that no longer exist). A metalink's file has `"source"` (its primary
  URL), `"name"` (its path below the output folder) and `"metalink"` (the
  metalink as given).
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

`-H`, `--cookie` and `--referer` are sent to the URL you give and to redirect
targets on the same origin (scheme, host and port). A redirect to another origin
gets what a browser would send there:

- of the `-H` headers, only `Accept`, `Accept-Language` and `User-Agent`; any
  other (an API key, say) is not sent;
- no cookies, no basic-auth credentials, and no `Cookie`, `Authorization` or
  `Proxy-Authorization` header;
- the referer cut down to its origin (`https://example.com/`), or none when the
  redirect leads from HTTPS (the URL you gave, or the referer) to plain HTTP;
- the user agent.

This is judged against the URL you gave on every hop, not the previous one: a
redirect back to its origin gets everything again. The same goes for segment
requests, resume checks and the re-resolution of an expired redirect. It is
stricter than curl, which keeps custom headers on another host: a header that
a CDN needs after a redirect must be given with the final URL instead.

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
digits in any case). It needs exactly one HTTP(S), FTP(S) or SFTP source (also on stdin):
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

`add -i FILE` (`--input-file`) reads downloads from a file in aria2's
input-file format; `-i -` reads it from stdin (not together with a `-`
source). Each line that does not start with whitespace is one download: a
URL (`http`, `https`, `ftp`, `ftps`, `ftpes`), magnet link or `.torrent` path
(a relative one is relative to the current directory). The lines under it that start with a
space or a TAB are options for that download, written `key=value` as aria2's
options without the `--` (spaces around the key and the value are ignored; the
value may contain `=`). Lines starting with `#` are comments, blank lines are
skipped, and CRLF line endings, a BOM and UTF-16 work as for `add -`.

```text
# Two ISOs into one folder, the second under another name.
https://example.com/a.iso
  dir=~/isos
  checksum=sha-256=9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08
https://example.com/b.iso
  dir=~/isos
  out=b-latest.iso
  header=Authorization: Bearer x
  max-download-limit=2M
/home/me/debian.torrent
  select-file=1,3-5
  seed-ratio=2
  seed-time=90
```

| Option | Meaning |
|--------|---------|
| `dir` | the folder, as `-o`; a relative one is relative to the current directory |
| `out` | the file name, as `--name` (a name only: put folders in `dir`) |
| `checksum` | `ALGO=HEX`, as `--checksum` (HTTP(S), FTP(S) and SFTP only) |
| `header` | `Name: value`, as `-H` (repeatable) |
| `referer` | as `--referer` |
| `user-agent` | the `User-Agent` header |
| `max-download-limit` | as `--limit` (`0`: none) |
| `pause` | `true` adds it paused, as `--paused`; `false` starts it |
| `select-file` | torrent: the files to download, as `--files` (numbers, ranges, globs) |
| `seed-ratio` | torrent: as `--seed-ratio` |
| `seed-time` | torrent: a number is minutes, as in aria2; `90m`, `2h` and `default` work too |

A URI line may hold several URIs of the same file separated by TABs, as in
aria2: the first is the download's source and the others are its mirrors, as
with `--mirror` (see [Mirrors](mirrors.md)). Each mirror must be an HTTP(S),
FTP(S) or SFTP URL and the first URI one of these too; anything else is a usage error
for that line. `--mirror` on the command line adds its URLs after the
line's own.

The flags on the command line are defaults for every download of the file,
and an entry's options override them: `-o`, `--limit`, `--paused`, `--files`,
`--seed-ratio`/`--seed-time` and `--referer` are replaced, and an entry's
`header` replaces a `-H` of the same name and adds to the others. `--name` and
`--checksum` still need exactly one download in all, and the torrent-only
flags still refuse an HTTP(S), FTP(S) or SFTP download anywhere on the line. Sources given as
arguments are added first, then the file's, in order; `--json` and `--wait`
treat them as one list, as for several sources.

Any other aria2 option (`split`, `max-connection-per-server`, ...) is ignored
with a warning on stderr naming it and its line. Everything else wrong with the
file is a usage error (exit 2) that adds nothing at all, so a script never ends
up with half a list queued: an option line before the first URI, one without
`=`, an empty or bad value for one of the options above, an unknown source, `checksum` on a torrent, a
torrent option on an HTTP(S), FTP(S) or SFTP download, or `pause=true` with `--wait`. Every
problem is reported with its line (`puppy: list.txt: line 7: bad
max-download-limit "fast": ...`). `seed-time=0`, which in aria2 means "do not seed", is
refused as well, since puppy's `0` would mean the opposite (never stop). What
the daemon refuses once the file is valid (for example a `select-file` number
past a torrent's last file) fails that download only, as with several sources.
