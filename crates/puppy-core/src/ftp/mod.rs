//! FTP and FTPS as a transport of the segmented URL download (`HttpDownload`).
//!
//! FTP is not an engine of its own: a URL download's locations (its source and mirrors)
//! each go over HTTP(S) or FTP(S), and everything above the wire (segments and work
//! stealing, the resume sidecar, validation before a resume, finalizing, the checksum
//! check, rate limits) is the same. What this module provides per location is what the
//! HTTP side has in `probe`, `validate` and `worker`:
//!
//! - [`probe`]: size (`SIZE`), modification time (`MDTM`), restart support (`FEAT`
//!   listing `REST STREAM`) and the file name;
//! - [`validate`]: whether the file a `.part` holds bytes of is still the one served
//!   (`SIZE` and `MDTM` must match; without `MDTM` downloaded bytes are compared);
//! - [`worker`]: one segment over one control and data connection (`REST` + `RETR`).
//!
//! A [`Site`] is one FTP location: its parsed URL, the sessions it keeps open (logged in
//! control connections, reused between segments) and what was learned about the server
//! (whether `EPSV` works, whether it refused `AUTH TLS`, how many sessions it allows).
//! It lives as long as the download.

mod session;
mod tls;
pub(crate) mod validate;
pub(crate) mod worker;

use crate::config::HttpConfig;
use crate::engine::EngineError;
use crate::fsutil::sanitize_filename;
use crate::http::probe::RemoteInfo;
use percent_encoding::percent_decode_str;
use session::Session;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};
use tokio::sync::Notify;

/// Idle sessions older than this are not reused (servers drop idle clients after a few
/// minutes; a session that idled for long may be dead already).
const IDLE_REUSE: Duration = Duration::from_secs(20);

/// The URL schemes this transport serves.
pub fn is_ftp_scheme(scheme: &str) -> bool {
    matches!(scheme, "ftp" | "ftps" | "ftpes")
}

pub fn is_ftp(url: &url::Url) -> bool {
    is_ftp_scheme(url.scheme())
}

/// How a location's control connection is secured.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Wire {
    /// `ftps://`: TLS from the first byte (port 990 by default).
    Implicit,
    /// `ftpes://`: `AUTH TLS`, required.
    Explicit,
    /// `ftp://`: as configured ([`FtpTls`]).
    Configured,
}

/// What a session connects to and logs in with. The password is never logged or shown
/// (see the `Debug` impl).
#[derive(Clone)]
struct Endpoint {
    host: String,
    port: u16,
    /// The file's path as sent in `SIZE`/`MDTM`/`RETR`: decoded, relative to the login
    /// directory unless it starts with `%2F` (RFC 1738).
    path: String,
    user: String,
    pass: String,
    wire: Wire,
}

impl std::fmt::Debug for Endpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Endpoint")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("path", &self.path)
            .field("user", &self.user)
            .field("wire", &self.wire)
            .finish_non_exhaustive()
    }
}

const ANONYMOUS: &str = "anonymous";
/// The customary anonymous password (an e-mail address, or what looks like one).
const ANONYMOUS_PASSWORD: &str = "anonymous@";

impl Endpoint {
    fn parse(url: &url::Url) -> Result<Endpoint, String> {
        let wire = match url.scheme() {
            "ftp" => Wire::Configured,
            "ftps" => Wire::Implicit,
            "ftpes" => Wire::Explicit,
            other => return Err(format!("not an FTP URL ({other}:)")),
        };
        let host = match url.host() {
            Some(url::Host::Ipv6(a)) => a.to_string(),
            Some(h) => h.to_string(),
            None => return Err("the URL has no host".into()),
        };
        if host.is_empty() {
            return Err("the URL has no host".into());
        }
        let port = url.port().unwrap_or(match wire {
            Wire::Implicit => 990,
            _ => 21,
        });
        let decode = |s: &str| percent_decode_str(s).decode_utf8_lossy().into_owned();
        // `;type=i` (RFC 1738) selects binary transfers, which are used anyway.
        let raw = url.path().split(';').next().unwrap_or("");
        let raw = raw.strip_prefix('/').unwrap_or(raw);
        let path = decode(raw);
        if path.is_empty() || path.ends_with('/') {
            return Err("the URL names a directory, not a file".into());
        }
        if path.contains(['\r', '\n']) {
            return Err("the URL's path contains a line break".into());
        }
        let (user, pass) = match (url.username(), url.password()) {
            ("", None) => (ANONYMOUS.to_string(), ANONYMOUS_PASSWORD.to_string()),
            (u, p) => {
                let user = decode(u);
                let pass = p.map(decode).unwrap_or_default();
                (user, pass)
            }
        };
        if user.contains(['\r', '\n']) || pass.contains(['\r', '\n']) {
            return Err("the URL's credentials contain a line break".into());
        }
        Ok(Endpoint {
            host,
            port,
            path,
            user,
            pass,
            wire,
        })
    }

    /// The file's name: the last path segment.
    fn filename(&self) -> String {
        let last = self.path.rsplit('/').next().unwrap_or("");
        sanitize_filename(if last.is_empty() { "download" } else { last })
    }
}

/// One FTP location of a download (see the module docs).
pub struct Site {
    /// As given (its password included: never shown).
    url: url::Url,
    ep: Result<Endpoint, String>,
    st: Mutex<SiteState>,
    /// Signalled whenever a session is given back or closed.
    freed: Notify,
}

#[derive(Default)]
struct SiteState {
    /// Logged-in sessions: in use or idle.
    connected: usize,
    /// The most sessions that were logged in at once (this transfer).
    peak: usize,
    /// Sessions being opened.
    connecting: usize,
    /// At most this many sessions: learned when the server turned one away.
    cap: Option<usize>,
    idle: Vec<(Session, Instant)>,
    /// `EPSV` failed: use `PASV`.
    pasv: bool,
    /// The server refused `AUTH TLS` (with TLS only tried).
    tls_refused: bool,
}

impl std::fmt::Debug for Site {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Site")
            .field("ep", &self.ep)
            .finish_non_exhaustive()
    }
}

/// A session lent out by [`Site::session`]. Dropped, it is closed (and counted as
/// such); [`Lease::keep`] gives it back for reuse instead.
pub(crate) struct Lease {
    site: Arc<Site>,
    session: Option<Session>,
}

impl Lease {
    fn get(&mut self) -> &mut Session {
        self.session
            .as_mut()
            .expect("present until kept or dropped")
    }

    /// Gives the session back to the site for reuse (it must be idle: no transfer
    /// pending).
    fn keep(mut self) {
        let session = self.session.take().expect("present until kept or dropped");
        let mut st = self.site.lock();
        st.idle.push((session, Instant::now()));
        drop(st);
        self.site.freed.notify_one();
    }
}

impl Lease {
    /// Logs out (`QUIT`) and closes the session, abandoning a transfer still running:
    /// the server then counts it gone at once, before a new session would find it still
    /// counted against a session limit.
    async fn close(mut self, cfg: &HttpConfig) {
        if let Some(mut s) = self.session.take() {
            let read = Duration::from_millis(cfg.read_timeout_ms).min(Duration::from_secs(5));
            s.quit(read).await;
            drop(s);
            let mut st = self.site.lock();
            st.connected = st.connected.saturating_sub(1);
            drop(st);
            self.site.freed.notify_one();
        }
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        if self.session.take().is_some() {
            let mut st = self.site.lock();
            st.connected = st.connected.saturating_sub(1);
            drop(st);
            self.site.freed.notify_one();
        }
    }
}

/// Uncounts a session being opened (if the open fails or is cancelled).
struct Pending<'a> {
    site: &'a Site,
    armed: bool,
}

impl Drop for Pending<'_> {
    fn drop(&mut self) {
        if self.armed {
            let mut st = self.site.lock();
            st.connecting = st.connecting.saturating_sub(1);
            drop(st);
            self.site.freed.notify_one();
        }
    }
}

impl Site {
    pub fn new(url: &url::Url) -> Arc<Site> {
        Arc::new(Site {
            url: url.clone(),
            ep: Endpoint::parse(url),
            st: Mutex::new(SiteState::default()),
            freed: Notify::new(),
        })
    }

    fn lock(&self) -> MutexGuard<'_, SiteState> {
        self.st.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn endpoint(&self) -> Result<&Endpoint, EngineError> {
        self.ep
            .as_ref()
            .map_err(|e| EngineError::Invalid(e.clone()))
    }

    /// Whether `active` workers already use every session the server allows.
    pub fn at_capacity(&self, active: u16) -> bool {
        self.lock().cap.is_some_and(|c| usize::from(active) >= c)
    }

    /// Ends a transfer: closes the idle sessions and forgets the learned session limit
    /// (a later run starts with the configured connections again).
    pub fn reset(&self) {
        let mut st = self.lock();
        let closed = st.idle.len();
        st.idle.clear();
        st.connected = st.connected.saturating_sub(closed);
        st.peak = st.connected;
        st.cap = None;
    }

    /// A logged-in session in binary mode: an idle one if there is one, else a new one.
    ///
    /// A server may limit sessions (per client, per user): it turns one more away with
    /// 421 or 530 while others are logged in. The limit is then taken to be the sessions
    /// logged in, no more are opened, and this waits for one of them to be given back
    /// or closed. Cancel-safe.
    pub(crate) async fn session(self: &Arc<Self>, cfg: &HttpConfig) -> Result<Lease, EngineError> {
        let ep = self.endpoint()?.clone();
        loop {
            let freed = self.freed.notified();
            tokio::pin!(freed);
            freed.as_mut().enable();
            let learned = {
                let mut st = self.lock();
                let now = Instant::now();
                // Drop stale idle sessions, then reuse the freshest.
                let before = st.idle.len();
                st.idle
                    .retain(|(_, at)| now.duration_since(*at) < IDLE_REUSE);
                let stale = before - st.idle.len();
                st.connected = st.connected.saturating_sub(stale);
                if let Some((session, _)) = st.idle.pop() {
                    return Ok(Lease {
                        site: self.clone(),
                        session: Some(session),
                    });
                }
                if st.cap.is_some_and(|c| st.connected + st.connecting >= c) {
                    None
                } else {
                    st.connecting += 1;
                    Some((st.pasv, st.tls_refused))
                }
            };
            let Some((pasv, tls_refused)) = learned else {
                freed.await;
                continue;
            };
            // Counted as connecting from here until it is logged in or has failed.
            let mut pending = Pending {
                site: self,
                armed: true,
            };
            let refusal = match Session::open(&ep, cfg, pasv, tls_refused).await {
                Ok((session, refused)) => {
                    pending.armed = false;
                    let mut st = self.lock();
                    st.connecting -= 1;
                    st.connected += 1;
                    st.peak = st.peak.max(st.connected);
                    if let Some(c) = st.cap
                        && st.connected > c
                    {
                        // More got in after all (the server's own count lagged).
                        st.cap = Some(st.connected);
                    }
                    st.tls_refused |= refused;
                    drop(st);
                    // Sessions waiting on others logging in can count again.
                    self.freed.notify_waiters();
                    return Ok(Lease {
                        site: self.clone(),
                        session: Some(session),
                    });
                }
                Err(e) => e,
            };
            pending.armed = false;
            // Whether to wait for the other sessions (else fail with the error).
            let wait = {
                let mut st = self.lock();
                st.connecting -= 1;
                let (connected, connecting) = (st.connected, st.connecting);
                match refusal {
                    EngineError::Ftp {
                        code: 421 | 530, ..
                    } if connected > 0 => {
                        // As many as were ever logged in at once got in: that many are
                        // allowed (fewer may be logged in now, closing ones not yet
                        // counted out by the server).
                        let allowed = connected.max(st.peak);
                        if st.cap.is_none_or(|c| c > allowed) {
                            tracing::info!(
                                host = %ep.host,
                                "the FTP server allows {allowed} connection(s); using no more"
                            );
                            st.cap = Some(allowed);
                        }
                        Ok(())
                    }
                    // Others are still logging in: see how they fare.
                    EngineError::Ftp { code: 421, .. } if connecting > 0 => Ok(()),
                    EngineError::Ftp { code: 530, message } if looks_like_limit(&message) => {
                        if connecting > 0 {
                            Ok(())
                        } else {
                            Err(EngineError::Unavailable(format!("FTP 530: {message}")))
                        }
                    }
                    e => Err(e),
                }
            };
            self.freed.notify_one();
            wait?;
            freed.await;
        }
    }

    /// `EPSV` failed on this server: use `PASV` from now on (or `EPSV` again, if `PASV`
    /// failed).
    fn set_pasv(&self, pasv: bool) {
        self.lock().pasv = pasv;
    }

    fn pasv(&self) -> bool {
        self.lock().pasv
    }
}

/// A 530 reply that reads like a session limit rather than a failed login (ProFTPD:
/// "Sorry, the maximum number of clients (5) from your host are already connected").
fn looks_like_limit(message: &str) -> bool {
    let m = message.to_ascii_lowercase();
    ["too many", "maximum number", "already connected", "limit"]
        .iter()
        .any(|w| m.contains(w))
}

/// What the FTP worker needs to know about its location.
#[derive(Clone)]
pub struct FtpTarget {
    pub site: Arc<Site>,
    /// The file's modification time (`MDTM`) as recorded: a session that finds another
    /// one finds a changed file.
    pub mdtm: Option<String>,
}

/// Probes the file at `site`: size, modification time, restart support and name.
pub(crate) async fn probe(site: &Arc<Site>, cfg: &HttpConfig) -> Result<RemoteInfo, EngineError> {
    let ep = site.endpoint()?.clone();
    if cfg.proxy.is_some() {
        tracing::info!(host = %ep.host, "FTP does not use the configured proxy: connecting directly");
    }
    let mut lease = site.session(cfg).await?;
    let s = lease.get();
    let feat = s.features(cfg).await?;
    let total = s.size(&ep.path, cfg).await?;
    let mdtm = s.mdtm(&ep.path, cfg).await?;
    lease.keep();
    // Restarting (REST) is assumed when the server cannot list its features; a server
    // that then refuses it gets one stream instead.
    let rest = feat.as_ref().is_none_or(|f| f.contains_key("REST"));
    Ok(RemoteInfo {
        final_url: site.url.clone(),
        total,
        accepts_ranges: total.is_some() && rest,
        etag: None,
        etag_strong: false,
        last_modified: mdtm,
        filename: ep.filename(),
    })
}

/// What an FTP session got wrong, as an engine error.
fn ftp_err(e: suppaftp::FtpError) -> EngineError {
    use suppaftp::FtpError;
    match e {
        FtpError::ConnectionError(e) => EngineError::Network(e.to_string()),
        FtpError::SecureError(m) if m.contains("certificate") => EngineError::Tls(m),
        FtpError::SecureError(m) => EngineError::Network(format!("TLS: {m}")),
        FtpError::UnexpectedResponse(r) => reply_err(&r.body),
        FtpError::BadResponse => {
            EngineError::Network("the FTP server sent a malformed reply".into())
        }
        FtpError::InvalidAddress(e) => {
            EngineError::Network(format!("the FTP server sent a bad address: {e}"))
        }
        FtpError::DataConnectionAlreadyOpen => {
            EngineError::Network("a data connection is already open".into())
        }
    }
}

/// An FTP reply (`body`: its raw lines, code first) as an error.
fn reply_err(body: &[u8]) -> EngineError {
    let text = String::from_utf8_lossy(body);
    let first = text.lines().next().unwrap_or("").trim();
    let code = first
        .get(..3)
        .and_then(|c| c.parse::<u16>().ok())
        .unwrap_or(0);
    let message = first
        .get(3..)
        .unwrap_or("")
        .trim_start_matches(['-', ' '])
        .trim()
        .to_string();
    EngineError::Ftp { code, message }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ep(s: &str) -> Result<Endpoint, String> {
        Endpoint::parse(&url::Url::parse(s).unwrap())
    }

    #[test]
    fn endpoints() {
        let e = ep("ftp://example.org/pub/a%20b.iso").unwrap();
        assert_eq!(
            (e.host.as_str(), e.port, e.path.as_str()),
            ("example.org", 21, "pub/a b.iso")
        );
        assert_eq!(
            (e.user.as_str(), e.pass.as_str()),
            (ANONYMOUS, ANONYMOUS_PASSWORD)
        );
        assert_eq!(e.wire, Wire::Configured);
        assert_eq!(e.filename(), "a b.iso");
        let e = ep("ftps://bob:p%40ss@[::1]/%2Fsrv/f;type=i").unwrap();
        assert_eq!(
            (e.host.as_str(), e.port, e.path.as_str()),
            ("::1", 990, "/srv/f")
        );
        assert_eq!((e.user.as_str(), e.pass.as_str()), ("bob", "p@ss"));
        assert_eq!(e.wire, Wire::Implicit);
        let e = ep("ftpes://bob@h:2121/f").unwrap();
        assert_eq!(
            (e.port, e.user.as_str(), e.pass.as_str()),
            (2121, "bob", "")
        );
        assert_eq!(e.wire, Wire::Explicit);
        assert!(ep("ftp://h/").is_err());
        assert!(ep("ftp://h/dir/").is_err());
        assert!(ep("ftp://h/a%0D%0ADELE%20b").is_err());
        let shown = format!("{:?}", ep("ftp://u:SECRET@h/f").unwrap());
        assert!(!shown.contains("SECRET"), "{shown}");
    }

    #[test]
    fn reply_errors() {
        let e = reply_err(b"550 Failed to open file.\r\n");
        assert!(
            matches!(&e, EngineError::Ftp { code: 550, message } if message == "Failed to open file.")
        );
        let e = reply_err(b"421-Too many\r\n421 connections\r\n");
        assert!(matches!(e, EngineError::Ftp { code: 421, .. }));
        assert!(looks_like_limit(
            "Sorry, the maximum number of clients (5) for this user are already connected."
        ));
        assert!(!looks_like_limit("Login incorrect."));
    }
}
