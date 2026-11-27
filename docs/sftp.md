# SFTP

SFTP URLs are `sftp://[user[:password]@]host[:port]/path`, port 22 by default.
The path is absolute (`sftp://host/srv/a.iso` is `/srv/a.iso`); start it with
`/~/` for one relative to your home directory on the server
(`sftp://host/~/a.iso`), as curl does. Without a user name puppy logs in as
your local user, as `ssh` does. Percent-encoded characters are decoded. A URL
ending in `/` names a directory and is refused. `scp://` URLs are not
supported (neither are they in aria2): use `sftp://`, which every OpenSSH
server offers.

puppy logs in without asking anything, since the daemon has no terminal to ask
on. It tries, in this order, what the server accepts:

1. The keys of your ssh-agent. On Unix that is the agent at `SSH_AUTH_SOCK`;
   on Windows the OpenSSH agent's pipe (`\\.\pipe\openssh-ssh-agent`), then
   Pageant.
2. Private key files: `[ssh] identity_file` if set, then `~/.ssh/id_ed25519`,
   `~/.ssh/id_ecdsa` and `~/.ssh/id_rsa`. Only unencrypted ones can be used. An
   encrypted key needs its passphrase, which puppy cannot ask for: if nothing
   else works, the download fails saying so (`~/.ssh/id_ed25519 is encrypted
   and puppy cannot ask for a passphrase: load it into ssh-agent (ssh-add
   ~/.ssh/id_ed25519) ...`). Load the key into your agent with `ssh-add`.
3. The password in the URL, with the `password` method or, if the server only
   offers that, `keyboard-interactive` (answering its password prompt).

A login that fails says what was tried and what the server accepts. It is not
retried: fix the cause and `puppy resume` the download. Agent forwarding is
never requested, and channels the server tries to open (forwarding requests)
are refused. A password is sent only to the host in its own URL, never to a
mirror's.

**The agent and the daemon.** The daemon signs logins with an agent, so it
needs to reach one. An SFTP download added with `puppy add` or the dashboard
remembers the `SSH_AUTH_SOCK` of the command that added it. Every `puppy`
command and the dashboard also tell the daemon their `SSH_AUTH_SOCK` when they
connect, and the daemon keeps the last four. A login tries, in order, the
download's own agent, those of the clients (the most recent first), then the
one in the daemon's environment, passing over any that is gone or holds no
keys; so a dashboard opened over SSH with a forwarded agent does not take over
the logins of downloads added from your desktop, though its agent may be tried
after theirs. A daemon that
`puppy` starts on demand has your shell's agent either way. One that runs as a
login service (see [docs/service.md](service.md)) starts without it and
picks it up from the first command you run; a download it resumes after a
reboot, before any command, has only the service's own `SSH_AUTH_SOCK` (with
systemd: `systemctl --user import-environment SSH_AUTH_SOCK`, or an agent
started as a user service), so give it a key file without a passphrase, or run
any `puppy` command to hand it your agent. To fix the agent instead, set its
socket (a named pipe on Windows):

```toml
[ssh]
agent = "/run/user/1000/ssh-agent.socket"   # always this agent
identity_file = "~/.ssh/puppy_ed25519"      # tried after the agent's keys
```

**Host keys.** Before anything is sent, the server's host key must be listed
for it in `~/.ssh/known_hosts` or in puppy's own `known_hosts`, in its data
directory (`~/.local/share/puppy/known_hosts` on Linux,
`~/Library/Application Support/puppy/known_hosts` on macOS,
`%APPDATA%\puppy\known_hosts` on Windows). Both are read in OpenSSH's
format: hashed entries (`HashKnownHosts`), `[host]:port` for another port than
22, wildcards and `!` negations, and `@revoked` keys, which are refused.
Host certificates (`@cert-authority`) are not supported. puppy never trusts a
key on first use. A server whose key is not listed fails the download with
its fingerprint, as OpenSSH prints it:

```
SSH: the host key of [host.example]:2222 is not known (ssh-ed25519 key
SHA256:Qm8T…). Check the fingerprint with the server's administrator; to trust
it, run `puppy ssh trust host.example:2222`
```

A server presenting another key of a type listed for it fails with
`REMOTE HOST IDENTIFICATION HAS CHANGED`, naming the file and line of the old
key: it may be an attack, so nothing is sent and puppy offers no way to go on
other than removing the old key yourself (`ssh-keygen -R`).

`puppy ssh trust HOST[:PORT]` (also `[IPv6]:PORT` or an `sftp://` URL)
connects, prints the server's key and fingerprint, and on a terminal asks
whether to trust it; check the fingerprint with whoever runs the server (on
the server: `ssh-keygen -lf /etc/ssh/ssh_host_ed25519_key.pub`). `--yes`
trusts without asking (in scripts, where it refuses to ask otherwise). The key
goes into puppy's own `known_hosts`, never into `~/.ssh/known_hosts`. A host
already known is left as it is, and a changed key is refused as above. HOST
must be a plain host name (letters, digits, `.`, `_`, `-`) or an IP address:
patterns, lists and markers are refused, as they would mean more in
`known_hosts`.

A host known only with keys of other types that now offers a key of a new
type (puppy asks for the known types first, so it offers none of them) looks
like an attack as well: downloads from it fail, and `puppy ssh trust` refuses
it with a warning naming the keys it is known with. If the server's keys
really changed, `--replace-type` trusts the new key anyway (after the usual
question, or with `--yes`). `--json` prints the host, port, key type,
fingerprint and what happened (`added`, `known`, `changed`, `other-type`,
`revoked`).

Downloads are segmented as over HTTP: up to `connections` SSH connections to
the server, each logged in once, with one SFTP session that reads its range
with several requests in flight, and is reused for the next range. Several
connections, rather than several channels on one, give several TCP streams,
which is what makes segments faster. A server that closes a new connection
while others are logged in (OpenSSH's `MaxStartups`, a per-user limit) caps
the download at the connections it accepted; the refused range waits for one
of them, after a growing delay, and the cap holds until the download is paused
or retried.

The file's size and modification time (`STAT`) are its validator. Every range
opens the file and checks them on the open file first; different values mean
the file changed, and the download needs attention (`remote changed`) without
taking a byte of it. They are checked again when a range is done. Resuming
(also after a restart) checks the same; a server that reports no modification
time gets the byte comparison of HTTP servers without validators.

A password in an SFTP URL is kept and hidden as one in an FTP URL is: only in
the daemon's private state file, `***` everywhere it is shown, never in the
log or the resume sidecar. The daemon's log keeps only warnings and errors of
the SSH library, whatever `RUST_LOG` says. SFTP connections never go through
the `proxy` setting, and there is no `ProxyJump` or `~/.ssh/config` support:
puppy connects to the URL's host and port directly. Retries, speed limits,
`--checksum`, pause, resume, restart and mirrors work as for HTTP.
