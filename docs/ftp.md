# FTP and FTPS

FTP URLs are `ftp://[user[:password]@]host[:port]/path`. Without a user name
puppy logs in as `anonymous` (password `anonymous@`). The path is relative to
the directory the server puts you in after login, as RFC 1738 says; most
servers put anonymous users at `/`, so `ftp://host/pub/a.iso` is `/pub/a.iso`.
Start the path with `%2F` for an absolute one (`ftp://host/%2Fsrv/a.iso`).
Percent-encoded characters in the path, user and password are decoded, and a
trailing `;type=i` is ignored (transfers are always binary). A URL ending in
`/` names a directory and is refused: puppy downloads files, not directories.

FTPS comes in two forms:

| URL | Port | TLS |
|-----|------|-----|
| `ftp://` | 21 | as `[ftp] tls` says (below); `try` by default |
| `ftpes://` | 21 | explicit (`AUTH TLS`), required |
| `ftps://` | 990 | implicit: TLS from the first byte |

The `tls` setting only applies to `ftp://` URLs:

```toml
[ftp]
tls = "try"       # "try" (the default), "require" or "off"
```

With `try`, puppy sends `AUTH TLS` before logging in. If the server refuses
it, the session goes on in plain FTP, user name and password
included. If the server agrees but its certificate does not verify, the
download fails: puppy never falls back to plain FTP over a failed handshake.
With `require` (and always for `ftpes://`), a server that refuses `AUTH TLS`
fails the download (`TLS: the server refused AUTH TLS (...), and TLS is
required`) before any credentials are sent. With `off`, puppy never asks. A
TLS session also protects the data connections (`PROT P`). Certificates are
verified as for HTTPS (the system's CA certificates, or puppy's bundled ones on
a system without any); there is no option to skip the check or to add a CA.

puppy uses passive mode only: `EPSV`, then `PASV` once a server refuses
`EPSV`. The address in a `PASV` reply is ignored and the data connection goes
to the server's own address, so a NATed server's private address (or a hostile
one) never matters. Active mode (`PORT`/`EPRT`) is not supported.

When the server tells the size (`SIZE`) and can restart transfers (its `FEAT`
lists `REST STREAM`, or it does not answer `FEAT`), a download is segmented as
over HTTP: up to `connections` sessions, each one control and one data
connection, each starting its range with `REST <offset>`, with the same work
stealing. A session whose range ends with the file is reused for the next
range; one whose range ends earlier logs out (`QUIT`), since its transfer is
cut short. A server that cannot tell the size or restart a transfer gets one
stream, which, as over HTTP, cannot be resumed and restarts from zero. A
server that turns a session away with 421 or 530 while others are logged in
(a per-client or per-user limit) caps the download at the sessions it got;
the refused range waits for one of them, and the cap holds until the
download is paused or retried.

Before every transfer puppy asks for the file's `SIZE` and `MDTM`
(modification time) again; a different answer means the file changed, and the
download needs attention (`remote changed`) without taking another byte of
it. Resuming (also after a restart) checks the same: the size must match, and
with a recorded `MDTM` that is enough. A server without `MDTM` gets the byte
comparison HTTP servers without validators get: up to three pieces of what is
downloaded are fetched again and compared with the partial file.

A password in an FTP URL (like one in an HTTP URL or a mirror) is kept only in
the daemon's state file, which only your user can read. It is shown as `***`
everywhere else: `puppy add`'s output, `ls`, `info` and their `--json` forms,
the dashboard. It is never written to the daemon's log or to the resume
sidecar next to the download. `~/.netrc` is not read.

FTP connections never go through the `proxy` setting, which is an HTTP proxy:
an FTP download connects to the server directly, and the daemon's log says so
when a proxy is set. Retries, backoff, speed limits, `--checksum`, pause,
resume and restart work as for HTTP; `info` and the dashboard show an FTP
download's connections and segment map as for HTTP.
