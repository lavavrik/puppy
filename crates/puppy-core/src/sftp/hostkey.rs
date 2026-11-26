//! Host key verification: a server's key must be listed for its host in a `known_hosts`
//! file (`~/.ssh/known_hosts`, or puppy's own one that `puppy ssh trust` writes) before
//! anything is sent to it. An unknown key is refused (never trusted on first use); a
//! different key of a type listed for the host is refused loudly, as OpenSSH does.
//!
//! The OpenSSH format is read: `[marker] hosts keytype base64 [comment]` lines, where
//! `hosts` is a comma-separated list of patterns (`*` and `?` wildcards, `!` negation,
//! `[host]:port` for another port than 22) or one hashed entry (`|1|salt|hash`, as
//! `HashKnownHosts` writes them). `@revoked` keys are refused; `@cert-authority` lines
//! are ignored (host certificates are not used). Lines that do not parse are skipped.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use russh::keys::{Algorithm, HashAlg, PublicKey};
use std::io::Write as _;
use std::path::{Path, PathBuf};

/// How a host is named in `known_hosts`: `host` on port 22, `[host]:port` otherwise.
pub fn host_pattern(host: &str, port: u16) -> String {
    if port == 22 {
        host.to_string()
    } else {
        format!("[{host}]:{port}")
    }
}

/// How `puppy ssh trust` is told the host: `host`, `host:port`, `[v6]:port`.
pub fn host_arg(host: &str, port: u16) -> String {
    match (port, host.contains(':')) {
        (22, _) => host.to_string(),
        (_, true) => format!("[{host}]:{port}"),
        (_, false) => format!("{host}:{port}"),
    }
}

/// A key's SHA-256 fingerprint as OpenSSH prints it (`SHA256:` and unpadded base64).
pub fn fingerprint(key: &PublicKey) -> String {
    key.fingerprint(HashAlg::Sha256).to_string()
}

/// `ssh-ed25519 SHA256:...`: the key's type and fingerprint.
pub fn describe(key: &PublicKey) -> String {
    format!("{} key {}", key.algorithm().as_str(), fingerprint(key))
}

/// One `known_hosts` line that names the host.
#[derive(Debug, Clone)]
pub struct Entry {
    pub file: PathBuf,
    /// 1-based.
    pub line: usize,
    pub key: PublicKey,
    pub revoked: bool,
}

/// What `known_hosts` files say about one host.
#[derive(Debug, Clone, Default)]
pub struct Known {
    pub entries: Vec<Entry>,
}

/// Whether a host key may be used.
#[derive(Debug, Clone)]
pub enum Verdict {
    Trusted,
    /// Not listed. `other_types`: keys of other types are listed for the host.
    Unknown {
        other_types: bool,
    },
    /// Another key of the same type is listed: the host's identity changed.
    Changed {
        file: PathBuf,
        line: usize,
    },
    Revoked {
        file: PathBuf,
        line: usize,
    },
}

fn same_key(a: &PublicKey, b: &PublicKey) -> bool {
    a.key_data() == b.key_data()
}

/// RSA keys sign with several algorithms (`rsa-sha2-512`, ...): one family.
pub(crate) fn same_type(a: &Algorithm, b: &Algorithm) -> bool {
    a == b || (matches!(a, Algorithm::Rsa { .. }) && matches!(b, Algorithm::Rsa { .. }))
}

impl Known {
    /// Looks `host:port` up in `files` (unreadable or missing files are skipped).
    pub fn lookup(files: &[PathBuf], host: &str, port: u16) -> Known {
        let name = host_pattern(&host.to_ascii_lowercase(), port);
        let mut entries = Vec::new();
        for file in files {
            let Ok(text) = std::fs::read_to_string(file) else {
                continue;
            };
            for (i, line) in text.lines().enumerate() {
                if let Some((key, revoked)) = parse_line(line, &name) {
                    entries.push(Entry {
                        file: file.clone(),
                        line: i + 1,
                        key,
                        revoked,
                    });
                }
            }
        }
        Known { entries }
    }

    pub fn verdict(&self, key: &PublicKey) -> Verdict {
        if let Some(e) = self
            .entries
            .iter()
            .find(|e| e.revoked && same_key(&e.key, key))
        {
            return Verdict::Revoked {
                file: e.file.clone(),
                line: e.line,
            };
        }
        let listed = || self.entries.iter().filter(|e| !e.revoked);
        if listed().any(|e| same_key(&e.key, key)) {
            return Verdict::Trusted;
        }
        if let Some(e) = listed().find(|e| same_type(&e.key.algorithm(), &key.algorithm())) {
            return Verdict::Changed {
                file: e.file.clone(),
                line: e.line,
            };
        }
        Verdict::Unknown {
            other_types: listed().next().is_some(),
        }
    }

    /// The key types listed for the host (asked for first, as OpenSSH does, so that a
    /// server with several host keys presents a known one).
    pub fn algorithms(&self) -> Vec<Algorithm> {
        let mut out: Vec<Algorithm> = Vec::new();
        for e in self.entries.iter().filter(|e| !e.revoked) {
            let a = e.key.algorithm();
            if !out.iter().any(|o| same_type(o, &a)) {
                out.push(a);
            }
        }
        out
    }
}

/// The key of a `known_hosts` line if it names `name` (`host` or `[host]:port`), and
/// whether it is marked `@revoked`.
fn parse_line(line: &str, name: &str) -> Option<(PublicKey, bool)> {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') {
        return None;
    }
    let mut fields = line.split_whitespace();
    let mut hosts = fields.next()?;
    let mut revoked = false;
    if let Some(marker) = hosts.strip_prefix('@') {
        match marker {
            "revoked" => revoked = true,
            // Host certificates are not used; other markers are unknown.
            _ => return None,
        }
        hosts = fields.next()?;
    }
    let keytype = fields.next()?;
    let blob = fields.next()?;
    if !hosts_match(hosts, name) {
        return None;
    }
    let key = PublicKey::from_openssh(&format!("{keytype} {blob}")).ok()?;
    Some((key, revoked))
}

/// Whether the host patterns of a line match `name`: one hashed entry, or a list where
/// some pattern matches and no negated one does.
fn hosts_match(hosts: &str, name: &str) -> bool {
    if let Some(hashed) = hosts.strip_prefix("|1|") {
        let mut parts = hashed.split('|');
        let (Some(salt), Some(hash)) = (parts.next(), parts.next()) else {
            return false;
        };
        let (Ok(salt), Ok(hash)) = (B64.decode(salt), B64.decode(hash)) else {
            return false;
        };
        return hmac_sha1(&salt, name.as_bytes()).as_slice() == hash.as_slice();
    }
    let mut matched = false;
    for pattern in hosts.split(',') {
        let pattern = pattern.to_ascii_lowercase();
        match pattern.strip_prefix('!') {
            Some(neg) if wildcard(neg.as_bytes(), name.as_bytes()) => return false,
            Some(_) => {}
            None => matched |= wildcard(pattern.as_bytes(), name.as_bytes()),
        }
    }
    matched
}

/// `*` (any run) and `?` (any one character) matching, as OpenSSH's `match_pattern`.
fn wildcard(p: &[u8], s: &[u8]) -> bool {
    let (mut pi, mut si) = (0, 0);
    let mut star: Option<(usize, usize)> = None;
    while si < s.len() {
        if pi < p.len() && (p[pi] == b'?' || p[pi] == s[si]) {
            pi += 1;
            si += 1;
        } else if pi < p.len() && p[pi] == b'*' {
            star = Some((pi, si));
            pi += 1;
        } else if let Some((sp, ss)) = star {
            pi = sp + 1;
            si = ss + 1;
            star = Some((sp, ss + 1));
        } else {
            return false;
        }
    }
    p[pi..].iter().all(|&c| c == b'*')
}

fn hmac_sha1(key: &[u8], msg: &[u8]) -> [u8; 20] {
    use sha1::{Digest, Sha1};
    let mut k = [0u8; 64];
    if key.len() > 64 {
        k[..20].copy_from_slice(&Sha1::digest(key));
    } else {
        k[..key.len()].copy_from_slice(key);
    }
    let ipad: Vec<u8> = k.iter().map(|b| b ^ 0x36).collect();
    let opad: Vec<u8> = k.iter().map(|b| b ^ 0x5c).collect();
    let inner = Sha1::new().chain_update(&ipad).chain_update(msg).finalize();
    Sha1::new()
        .chain_update(&opad)
        .chain_update(inner)
        .finalize()
        .into()
}

/// Whether `host` may be written into a `known_hosts` file as a plain name: letters,
/// digits, `.`, `_` and `-` (a host name or IPv4 address), or an IPv6 address. Anything
/// else could turn into a pattern (`*`, `?`, `!`, `,`), a hashed entry or a marker.
pub fn valid_host(host: &str) -> bool {
    if host.parse::<std::net::Ipv6Addr>().is_ok() {
        return true;
    }
    !host.is_empty()
        && !host.starts_with(['-', '.'])
        && host
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

/// Adds `host:port`'s `key` to the `known_hosts` file at `path` (created, private, with
/// its directory if missing). `host` must be [`valid_host`].
pub fn trust(path: &Path, host: &str, port: u16, key: &PublicKey) -> std::io::Result<()> {
    if !valid_host(host) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("{host:?} is not a host name or IP address"),
        ));
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut opts = std::fs::OpenOptions::new();
    opts.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts.open(path)?;
    let ends_with_newline = std::fs::read(path)
        .map(|b| b.last().is_none_or(|&c| c == b'\n'))
        .unwrap_or(true);
    let key = PublicKey::new(key.key_data().clone(), "");
    let line = key.to_openssh().map_err(std::io::Error::other)?;
    let pattern = host_pattern(&host.to_ascii_lowercase(), port);
    let sep = if ends_with_newline { "" } else { "\n" };
    writeln!(f, "{sep}{pattern} {}", line.trim())?;
    f.sync_all()
}

#[cfg(test)]
mod tests {
    use super::*;

    const ED: &str =
        "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIJdD7y3aLq454yWBdwLWbieU1ebz9/cu7/QEXn9OIeZJ";
    const ED2: &str =
        "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAILIG2T/B0l0gaqj3puu510tu9N1OkQ4znY3LYuEm5zCF";

    fn key(s: &str) -> PublicKey {
        PublicKey::from_openssh(s).unwrap()
    }

    fn known(text: &str, host: &str, port: u16) -> Known {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("known_hosts");
        std::fs::write(&path, text).unwrap();
        Known::lookup(&[path, dir.path().join("missing")], host, port)
    }

    #[test]
    fn patterns() {
        assert!(hosts_match("a.example,b.example", "b.example"));
        assert!(hosts_match("*.example", "a.example"));
        assert!(!hosts_match("*.example,!a.example", "a.example"));
        assert!(hosts_match("h?st", "host"));
        assert!(!hosts_match("host", "[host]:2222"));
        assert!(hosts_match("[host]:2222", "[host]:2222"));
        assert!(hosts_match("HOST", "host"));
        assert!(wildcard(b"*", b""));
        assert!(!wildcard(b"a*b", b"ac"));
    }

    #[test]
    fn hashed_entries() {
        // A hashed entry as `ssh-keygen -H` writes it: `|1|` salt `|` HMAC-SHA1.
        let salt = [7u8; 20];
        let hash = hmac_sha1(&salt, b"[localhost]:2222");
        let line = format!("|1|{}|{} {ED}\n", B64.encode(salt), B64.encode(hash));
        let k = known(&line, "localhost", 2222);
        assert!(matches!(k.verdict(&key(ED)), Verdict::Trusted));
        assert!(known(&line, "localhost", 22).entries.is_empty());
    }

    #[test]
    fn hmac_matches_rfc2202() {
        // RFC 2202 test case 2.
        let mac = hmac_sha1(b"Jefe", b"what do ya want for nothing?");
        let hex: String = mac.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(hex, "effcdf6ae5eb2fa2d27416d5f184df9c259a7c79");
    }

    #[test]
    fn verdicts() {
        let text =
            format!("# comment\n\nbad line\n[h]:2222 {ED}\nother {ED2}\n@cert-authority * {ED2}\n");
        let k = known(&text, "h", 2222);
        assert!(matches!(k.verdict(&key(ED)), Verdict::Trusted));
        assert!(
            matches!(k.verdict(&key(ED2)), Verdict::Changed { line: 4, .. }),
            "{:?}",
            k.verdict(&key(ED2))
        );
        assert!(matches!(
            known(&text, "nobody", 22).verdict(&key(ED)),
            Verdict::Unknown { other_types: false }
        ));
        let revoked = format!("h {ED}\n@revoked h {ED}\n");
        assert!(matches!(
            known(&revoked, "h", 22).verdict(&key(ED)),
            Verdict::Revoked { line: 2, .. }
        ));
    }

    #[test]
    fn trust_appends_a_line() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub/known_hosts");
        trust(&path, "Example.ORG", 2222, &key(ED)).unwrap();
        trust(&path, "::1", 22, &key(ED2)).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text, format!("[example.org]:2222 {ED}\n::1 {ED2}\n"));
        let k = Known::lookup(std::slice::from_ref(&path), "example.org", 2222);
        assert!(matches!(k.verdict(&key(ED)), Verdict::Trusted));
        assert_eq!(host_arg("::1", 2222), "[::1]:2222");
        assert_eq!(host_arg("h", 2222), "h:2222");
        assert!(fingerprint(&key(ED)).starts_with("SHA256:"));
        for bad in [
            "a,b",
            "*.x",
            "!x",
            "|1|abc|def",
            "@revoked",
            "",
            "-x",
            "a b",
            "h\nx",
        ] {
            assert!(!valid_host(bad), "{bad:?}");
            assert!(trust(&path, bad, 22, &key(ED)).is_err(), "{bad:?}");
        }
        for good in ["example.org", "10.0.0.1", "::1", "fe80::1", "my_host-2"] {
            assert!(valid_host(good), "{good:?}");
        }
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            format!("[example.org]:2222 {ED}\n::1 {ED2}\n")
        );
    }
}
