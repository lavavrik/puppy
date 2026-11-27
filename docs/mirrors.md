# Mirrors

`add --mirror URL` (repeatable) gives an HTTP(S), FTP(S) or SFTP download other
places to get the same file from; the source stays its primary URL. It needs
exactly one such source, like `--name`. Mirrors are `http://`, `https://`,
`ftp://`, `ftps://`, `ftpes://` or `sftp://` URLs, whatever the source's
protocol: an HTTP download can take ranges from an FTP or SFTP mirror and the
other way round. A torrent with
mirrors is refused.

When the download starts, puppy probes the primary and every mirror at once.
The file is what the primary says it is (its size and name); if the primary
does not answer, the first mirror that does decides. A location is dropped,
with its reason, if its probe fails, if it reports another size, or if it does
not honour range requests while others do. If no location honours ranges, or
the size is unknown (so nothing can be compared), one stream goes to the first
location that answered and the rest are dropped. The download fails only if
no location answers at all; it fails transiently (and is retried) if one of
them failed with an error that may go away.

The connection count (`connections`, 8 by default) is the total over all
mirrors. Each new connection goes to the usable mirror with the fewest
connections, so 8 connections over two mirrors are 4 and 4; between equally
loaded mirrors, the one that is faster per connection wins. Work stealing
then favours fast mirrors by itself: a connection that finishes its range
early takes half of the largest range still running, and the freed connection
goes back to the least loaded mirror, usually the fast one. A mirror that
becomes usable again (after its backoff, say) while every connection is taken
gets one moved to it from the busiest mirror. There is no
separate per-host limit: without mirrors puppy already opens all 8
connections to one host, and a download whose other mirrors failed keeps that.

A connection that fails on an error that may go away (network error, timeout,
5xx, 429, 408) hands its range back at once, and the range continues on
another usable mirror. The failing mirror is benched: it gets no new
connections for the usual retry backoff, which grows with each failure in a
row; its other connections carry on, and their failures during the bench
count as the same outage. A mirror that fails `max_retries` (8) times in a row
without progress, refuses the request (401, 403, 404, 410: an expired
redirect target is re-resolved first, for that mirror alone while the others
keep downloading, and again only after that mirror made progress), sends another file
(its `If-Range` no longer matches, or another size) or stops honouring ranges
is dropped. The download fails only when no mirror is left: transiently if
one of them kept failing on such errors, with a reason naming each mirror and
why it went (`no mirror works: a.example: HTTP 404; b.example: retries
exhausted: HTTP 503`), as needing attention if one serves a changed file, and
otherwise for good. `resume` (and the automatic retry of a transient failure)
tries every mirror again.

Each location is validated on its own, since mirrors have their own ETag and
Last-Modified (an FTP location: its size and `MDTM`, checked before each of
its transfers, see [FTP](ftp.md); an SFTP location: its size and modification
time, see [SFTP](sftp.md)). Every request to a location carries that
location's own validator as `If-Range`, and the resume sidecar records each location's
final URL and validators. On resume (also after a restart) every location is
checked against the partial file as a download without mirrors is: with a
strong ETag by `If-Range`, otherwise by its size and a byte comparison of
already downloaded parts, whichever mirror they came from. A location without
any validator recorded is probed first and then compared byte for byte. A
mirror whose file changed is dropped and never asked for another byte; the
download continues from the locations proven unchanged. If the primary's file
changed (and the primary has an ETag or Last-Modified, so this is certain),
the download needs attention (`remote changed`), as without mirrors, even if
a mirror still serves the old file; it does so too when every location
changed, at resume or during the download. A location that cannot be checked
because it does not answer for now is benched, and checked again before it
gets a range; if none can be checked, the resume fails transiently.

A location without validators (neither ETag nor Last-Modified; an FTP server
without `MDTM`, an SFTP server that does not report modification times) cannot be checked with `If-Range`. Before each range it is given, puppy compares the
last bytes (64 KiB) of the largest part already downloaded against what that
location serves there, and drops it if they differ. Until there are such
bytes it waits, unless no location has validators at all. What this cannot
catch is such a mirror changing its file in the middle of one response, or
serving different bytes only outside the compared window: give a
`--checksum` to be sure.

Mirrors that agree on the size may still serve different bytes. Give a
`--checksum` with mirrors: puppy warns on stderr when there is none, but adds
the download anyway. A daemon from before mirrors ignores them and downloads
from the primary only; `add` warns about that.

`-H`, `--cookie` and `--referer` (and a browser's cookies) belong to the
primary's site. A mirror on the primary's own origin (scheme, host and port)
gets them; a mirror on another origin gets none of them, no basic-auth
credentials and no headers at all, only the user agent.

`info` lists every location after `mirrors`: the primary first, then the
mirrors by priority, each with how it is used (`in use, 4 connections,
2.1 MiB/s, 30.0 MiB from it`, `failed, retried in 2 s: HTTP 503`, or
`dropped: its size (4194305 bytes) differs from the file's (4194304 bytes)`).
The dashboard's detail pane shows the same under `Mirrors`. In `--json`
snapshots, `"mirrors"` lists the mirror URLs as added; `info --json` has the
live state in the detail's `"mirrors"`: `{"url","primary","state":{"kind":
"in_use"|"benched"|"dropped",…},"connections","bps","done_bytes"}`. Both are
absent for a download without mirrors.

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
