# Metalink

A metalink lists one or more files, where to get each and how to check it.
puppy reads Metalink 4 (RFC 5854, `.meta4`, `application/metalink4+xml`) and
the older Metalink 3 (`.metalink`, `application/metalink+xml`), which
Fedora's MirrorManager still serves. Each `<file>` becomes its own download,
with its own id.

`puppy add` takes a metalink as:

- a local file ending in `.meta4` or `.metalink`, or any file whose content
  starts with a `<metalink>` element (a `.torrent` file never is one);
- an `http://` or `https://` URL whose path ends in `.meta4` or `.metalink`;
- a line of an input file (`-i`) that is either of these.

`--metalink` reads every source as a metalink: any HTTP(S) URL (for one that
serves a metalink under another name, such as
`https://mirrors.fedoraproject.org/metalink?repo=…`) and any file. A URL is
always fetched with `Accept: application/metalink4+xml, application/metalink+xml`,
so MirrorBrain and MirrorCache servers (openSUSE's `download.opensuse.org`)
answer even a file's own URL with its metalink. (`*/*` is deliberately not
accepted as well: MirrorCache then sends the file itself, even at its `.meta4`
URL.)
`--no-metalink` turns detection off: a `.meta4` source is then downloaded as a
file. Without either flag, an HTTP(S) URL that does not end in `.meta4` or
`.metalink` is downloaded as a file whatever it serves: puppy does not make an
extra request for every URL to look at its `Content-Type`. An `ftp://` or
`sftp://` URL is always downloaded as a file.

The metalink is read, or fetched, by `puppy add` itself, before anything is
queued, and each file goes to the daemon as an ordinary download (so a
daemon older than this works too). The fetch goes through the configured
`proxy`, follows up to 10 redirects and must finish within 60 s. `-H`,
`--cookie`, `--referer` and an entry's `header` and `referer` options are sent
only to the origin (scheme, host and port) of the URL you gave; a redirect to
any other origin gets the user agent alone. A redirect from HTTPS to plain HTTP
is refused, since the document supplies the hashes the files are checked
against. If it fails, or the document is not a usable metalink, the source
is reported like one that cannot be added (exit 1; `"failed"` in `--json`) and
none of its files is queued; the other sources are added. The dashboard's add
form takes a metalink path or URL as well (detected as above, without the
flags), and adds every file into the form's folder.

How a `<file>` is used:

- **URLs.** Its `<url>`s are the download's locations, best first: by
  `priority` (1 is best; Metalink 3: by `preference`, 100 is best), then in
  document order, URLs without one last. The first is the primary, the rest
  are mirrors (see [Mirrors](mirrors.md)), with their `location`. Only
  `http`, `https`, `ftp`, `ftps`, `ftpes` and `sftp` URLs are kept (the schemes
  puppy downloads from; others, such as `rsync`, are skipped), at most 16 per
  file: the engine probes every location when the download starts.
- **Hashes.** The strongest `<hash>` puppy supports becomes the download's
  checksum, in the order `sha-512`, `sha-384`, `sha-256`, `sha-224`, `sha-1`,
  `md5`; types it does not know (`sha3-256`, `ed2k`, …) are skipped. A hash of a
  known type that is not a valid digest refuses the metalink. A file without a
  usable hash is downloaded unchecked, with the usual warning when it has
  mirrors.
- **Size.** `<size>` is checked against every location when the download
  starts: one that reports another size is dropped, even the primary, and if
  every location that answers does, the download fails
  (`the file is N bytes, not the expected M`). A location that reports the
  size decides what the file is; one that does not report a size comes after
  it and is taken to have it (the totals of its range responses are checked).
  The finished file's length is checked too, before its hash: a download whose
  locations never said their size fails if it ends up with another length.
  Snapshots show the size as `"expected_size"`; a daemon too old to check it
  ignores it, and `add` warns.
- **Name.** `<name>` may contain subdirectories (`iso/x86_64/a.iso`), which
  are created below the output folder (`-o`, an entry's `dir`, or the daemon's
  download folder). The name comes from a document puppy does not control, so
  a metalink is refused, whole, if any of its names could leave that folder:
  an absolute path, a drive letter or UNC prefix, a `..` component (or one of
  dots and spaces only, which Windows reads as `.` or `..`), a control
  character (NUL included), an invisible Unicode formatting character (bidi
  overrides such as U+202E, zero-width characters, U+FEFF), more than 32
  components or more than 4096 bytes.
  Each component is then cleaned like any downloaded file name: `<>:"|?*`
  become `_`, trailing dots and spaces are dropped, Windows device names
  (`CON`, `nul.txt`, `COM1`, …) get a leading `_`, and components longer than
  200 bytes are shortened. Two files with the same path (ignoring case and
  Unicode normalization) refuse the metalink. A subdirectory that already
  exists below the output folder as a symbolic link (or as a file) refuses it
  too, so creating the folders never follows a link out of it. As for any download, an existing file is never overwritten:
  the new one gets a free name next to it.

Options:

- `-o`, `--paused`, `--limit` and `--wait` (and an input-file entry's `dir`,
  `pause` and `max-download-limit`) apply to every file.
- `--name`, `--checksum` and `--mirror` (an entry's `out` and `checksum`)
  apply to a metalink of one file only: `--name` replaces its name and
  subdirectories, `--checksum` its hash, `--mirror` adds locations. With
  several files they are usage errors (exit 2) and nothing is added.
- `-H`, `--cookie` and `--referer` (and an entry's `header` and `referer`)
  belong to the site of the metalink's URL. A file gets them only if its
  primary URL is on that origin and the metalink was served from there too,
  not after a redirect to another origin (that site wrote the list then).
  Any other file gets the user agent alone, and its mirrors are treated as in
  [Mirrors](mirrors.md). With a local metalink puppy cannot tell which of the
  hosts it lists they are meant for, so giving any of them (a `User-Agent`
  header excepted) is a usage error: add the metalink by its URL instead.
- TAB-separated mirrors on a metalink's input-file line, and torrent-only
  options, are usage errors.

`puppy add` prints `added ID PATH` for each file; `--json` lists each in
`"added"` with its `"name"` and `"metalink"`.

Ignored, by design or for now:

- `<metaurl>` (torrents) and Metalink 3 `type="bittorrent"` URLs: puppy
  downloads a metalink's files from their plain URLs. A file with only a
  torrent is refused (`no usable URL (puppy does not use a metalink's
  torrents)`).
- `<pieces>` (chunk hashes): the whole-file hash is checked once the file is
  complete, so a mirror serving bad bytes fails the download (keeping the
  file) rather than having its ranges fetched again elsewhere.
- `<signature>`: OpenPGP and other signatures are not verified. Check them
  yourself if you need to.
- `<origin dynamic="true">` (the metalink is not fetched again), `<os>`,
  `<language>`, `<version>`, `<publisher>`, `<description>` and the other
  descriptive elements, and elements of other namespaces (MirrorManager's
  `mm0:alternates`, the hashes of older versions of a file, are not the
  file's).
- Metalink/HTTP (RFC 6249: `Link: rel=duplicate` and `Digest` response
  headers) is not used.

The parser is a safe one for documents from the network: a metalink with a
`<!DOCTYPE>` is refused (Metalink has no use for one), so no DTD, entity
definition or external entity is ever read; only XML's five predefined
entities and character references are expanded. A document is at most 4 MiB
(a larger one is refused from its `Content-Length`, or once that many bytes
have been read), with at most 1000 files, 1000 URLs per file, nesting 32
deep and 250000 elements. It must be UTF-8.
