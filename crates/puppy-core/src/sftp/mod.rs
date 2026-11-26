//! SFTP as a transport of the segmented URL download (`HttpDownload`), like FTP (see
//! [`crate::ftp`]): an `sftp://` location (source or mirror) is probed, validated before
//! a resume and downloaded in segments by this module, and everything above the wire
//! (segments and work stealing, the resume sidecar, finalizing, the checksum, rate
//! limits, mirrors) is shared.
//!
//! - [`probe`]: `STAT` gives the size and modification time (the validator) and that
//!   the path is a regular file;
//! - [`validate`]: before a resume the size and modification time must match; without
//!   a modification time downloaded bytes are compared;
//! - [`worker`]: one segment over one connection: the file is opened, its attributes
//!   (`FSTAT` of the open handle) checked against the recorded ones, then ranges read
//!   with several requests in flight; the attributes are checked again at the end.
//!
//! A [`Site`] is one SFTP location: its parsed URL and the connections it keeps open
//! (each an SSH connection with one SFTP channel, logged in and reused between
//! segments). Segments use several connections rather than several channels of one:
//! separate TCP flows and cipher states are what makes segmenting faster. A server that
//! turns a connection away while others are logged in (OpenSSH's `MaxStartups`, PAM
//! limits) sets the limit to the connections it accepted; puppy waits for one of those
//! and backs off before trying again.
//!
//! Logins never prompt (see [`auth`]): the agent, then unencrypted key files, then the
//! URL's password. Host keys must be known (see [`hostkey`]). Agent forwarding is never
//! requested, and a URL's password only ever goes to that URL's host.

mod auth;
mod conn;
pub mod hostkey;
pub(crate) mod validate;
pub(crate) mod worker;

pub use conn::{fetch_host_key, known_hosts_files};

use crate::config::HttpConfig;
use crate::engine::EngineError;
use crate::fsutil::sanitize_filename;
use crate::http::probe::RemoteInfo;
use conn::{Conn, OpenError};
use percent_encoding::percent_decode_str;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};
use tokio::sync::Notify;

/// Idle connections older than this are not reused.
const IDLE_REUSE: Duration = Duration::from_secs(60);

pub fn is_sftp_scheme(scheme: &str) -> bool {
    scheme == "sftp"
}

pub fn is_sftp(url: &url::Url) -> bool {
    is_sftp_scheme(url.scheme())
}

/// The agent socket the most recent client offered (its `SSH_AUTH_SOCK`): used by
/// [`crate::config::SshAgent::Auto`] before this process's own.
static OFFERED_AGENT: Mutex<Option<PathBuf>> = Mutex::new(None);

/// Records the agent socket a client offered (an empty or relative path is ignored).
pub fn offer_agent(sock: &str) {
    let path = PathBuf::from(sock);
    if sock.is_empty() || !(path.is_absolute() || sock.starts_with(r"\\.\pipe\")) {
        return;
    }
    *OFFERED_AGENT.lock().unwrap_or_else(|e| e.into_inner()) = Some(path);
}

fn offered_agent() -> Option<PathBuf> {
    OFFERED_AGENT
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
}

/// What a connection connects to and logs in with. The password is never logged or
/// shown (see the `Debug` impl).
#[derive(Clone)]
pub(crate) struct Endpoint {
    pub host: String,
    pub port: u16,
    /// As sent to the server: absolute (`sftp://h/srv/f` is `/srv/f`), or relative to
    /// the login directory for `sftp://h/~/f` (`f`).
    pub path: String,
    /// The URL's user name, if any (else the local one).
    pub user: Option<String>,
    pub pass: Option<String>,
}

impl std::fmt::Debug for Endpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Endpoint")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("path", &self.path)
            .field("user", &self.user)
            .finish_non_exhaustive()
    }
}

impl Endpoint {
    pub(crate) fn parse(url: &url::Url) -> Result<Endpoint, String> {
        if !is_sftp(url) {
            return Err(format!("not an SFTP URL ({}:)", url.scheme()));
        }
        let host = match url.host() {
            Some(url::Host::Ipv6(a)) => a.to_string(),
            Some(h) => h.to_string(),
            None => return Err("the URL has no host".into()),
        };
        if host.is_empty() {
            return Err("the URL has no host".into());
        }
        let port = url.port().unwrap_or(22);
        let decode = |s: &str| percent_decode_str(s).decode_utf8_lossy().into_owned();
        let raw = decode(url.path());
        let path = match raw.strip_prefix("/~/") {
            Some(rel) => rel.to_string(),
            None => raw,
        };
        if path.is_empty() || path == "/" || path.ends_with('/') {
            return Err("the URL names a directory, not a file".into());
        }
        if path.contains('\0') {
            return Err("the URL's path contains a NUL byte".into());
        }
        let user = Some(decode(url.username())).filter(|u| !u.is_empty());
        let pass = url.password().map(decode);
        Ok(Endpoint {
            host,
            port,
            path,
            user,
            pass,
        })
    }

    fn filename(&self) -> String {
        let last = self.path.rsplit('/').next().unwrap_or("");
        sanitize_filename(if last.is_empty() { "download" } else { last })
    }
}

/// One SFTP location of a download (see the module docs).
pub struct Site {
    /// As given (its password included: never shown).
    url: url::Url,
    ep: Result<Endpoint, String>,
    st: Mutex<SiteState>,
    /// Signalled (to the waiters at the time) when a connection is given back or closed.
    freed: Notify,
}

#[derive(Default)]
struct SiteState {
    /// Logged-in connections: in use or idle.
    connected: usize,
    /// Connections being opened.
    connecting: usize,
    /// At most this many connections: learned when the server turned one away.
    cap: Option<usize>,
    idle: Vec<(Conn, Instant)>,
}

impl std::fmt::Debug for Site {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Site")
            .field("ep", &self.ep)
            .finish_non_exhaustive()
    }
}

/// A connection lent out by [`Site::conn`]. Dropped, it is closed (and counted as
/// such); [`Lease::keep`] gives it back for reuse instead.
pub(crate) struct Lease {
    site: Arc<Site>,
    conn: Option<Conn>,
}

impl Lease {
    fn get(&mut self) -> &mut Conn {
        self.conn.as_mut().expect("present until kept or dropped")
    }

    /// Gives the connection back for reuse (no file handle may be left open).
    fn keep(mut self) {
        let conn = self.conn.take().expect("present until kept or dropped");
        let mut st = self.site.lock();
        if conn.is_closed() {
            st.connected = st.connected.saturating_sub(1);
        } else {
            st.idle.push((conn, Instant::now()));
        }
        drop(st);
        self.site.freed.notify_waiters();
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        if let Some(conn) = self.conn.take() {
            conn.close_soon();
            let mut st = self.site.lock();
            st.connected = st.connected.saturating_sub(1);
            drop(st);
            self.site.freed.notify_waiters();
        }
    }
}

/// Uncounts a connection being opened (if the open fails or is cancelled).
struct Pending<'a> {
    site: &'a Site,
    armed: bool,
}

impl Drop for Pending<'_> {
    fn drop(&mut self) {
        if self.armed {
            self.site.lock().connecting -= 1;
            self.site.freed.notify_waiters();
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

    pub(crate) fn endpoint(&self) -> Result<&Endpoint, EngineError> {
        self.ep
            .as_ref()
            .map_err(|e| EngineError::Invalid(e.clone()))
    }

    /// Whether `active` workers already use every connection the server allows.
    pub fn at_capacity(&self, active: u16) -> bool {
        self.lock().cap.is_some_and(|c| usize::from(active) >= c)
    }

    /// Ends a transfer: closes the idle connections and forgets the learned limit.
    pub fn reset(&self) {
        let mut st = self.lock();
        let idle = std::mem::take(&mut st.idle);
        st.connected = st.connected.saturating_sub(idle.len());
        st.cap = None;
        drop(st);
        for (c, _) in idle {
            c.close_soon();
        }
    }

    /// A logged-in connection with an SFTP session: an idle one if there is one, else
    /// a new one. Cancel-safe.
    ///
    /// A server turning a new connection away while others are logged in limits
    /// connections: the limit is taken to be the ones logged in, and this waits for one
    /// of them to be given back (after a delay, growing with each refusal).
    pub(crate) async fn conn(self: &Arc<Self>, cfg: &HttpConfig) -> Result<Lease, EngineError> {
        let ep = self.endpoint()?.clone();
        let mut refusals: u32 = 0;
        loop {
            let freed = self.freed.notified();
            tokio::pin!(freed);
            freed.as_mut().enable();
            let open = {
                let mut st = self.lock();
                let now = Instant::now();
                let before = st.idle.len();
                st.idle
                    .retain(|(c, at)| now.duration_since(*at) < IDLE_REUSE && !c.is_closed());
                let stale = before - st.idle.len();
                st.connected -= stale;
                if let Some((conn, _)) = st.idle.pop() {
                    return Ok(Lease {
                        site: self.clone(),
                        conn: Some(conn),
                    });
                }
                if st.cap.is_some_and(|c| st.connected + st.connecting >= c) {
                    false
                } else {
                    st.connecting += 1;
                    true
                }
            };
            if !open {
                freed.await;
                continue;
            }
            let mut pending = Pending {
                site: self,
                armed: true,
            };
            let why = match Conn::open(&ep, cfg).await {
                Ok(conn) => {
                    pending.armed = false;
                    let mut st = self.lock();
                    st.connecting -= 1;
                    st.connected += 1;
                    if let Some(c) = st.cap
                        && st.connected > c
                    {
                        st.cap = Some(st.connected);
                    }
                    return Ok(Lease {
                        site: self.clone(),
                        conn: Some(conn),
                    });
                }
                Err(OpenError::Fatal(e)) => return Err(e),
                Err(OpenError::Refused(why)) => why,
            };
            pending.armed = false;
            let (connected, connecting) = {
                let mut st = self.lock();
                st.connecting -= 1;
                (st.connected, st.connecting)
            };
            if connected == 0 && connecting == 0 {
                // Nobody got in: not a limit, the server is in trouble (or refuses us).
                return Err(EngineError::Network(why));
            }
            if connected > 0 {
                let mut st = self.lock();
                if st.cap.is_none_or(|c| c > connected) {
                    tracing::info!(
                        host = %ep.host,
                        "the SFTP server allows {connected} connection(s); using no more"
                    );
                    st.cap = Some(connected);
                }
            }
            // The others waiting on the count are told. This round's `freed` is never
            // awaited again (the next round registers anew), so this cannot wake this
            // task; it waits before trying again.
            self.freed.notify_waiters();
            let delay = crate::http::worker::backoff(cfg, refusals);
            refusals = refusals.saturating_add(1);
            tokio::time::sleep(delay).await;
        }
    }
}

/// What the SFTP worker needs to know about its location.
#[derive(Clone)]
pub struct SftpTarget {
    pub site: Arc<Site>,
    /// The file's modification time as recorded (`last_modified`, Unix seconds): a
    /// connection finding another one finds a changed file.
    pub mtime: Option<String>,
}

/// The validator as recorded in `last_modified`: Unix seconds, prefixed so that it is
/// never mistaken for an HTTP date.
pub(crate) fn mtime_validator(mtime: Option<u32>) -> Option<String> {
    mtime.map(|t| format!("sftp-mtime:{t}"))
}

/// Probes the file at `site`: size, modification time and name.
pub(crate) async fn probe(site: &Arc<Site>, cfg: &HttpConfig) -> Result<RemoteInfo, EngineError> {
    let ep = site.endpoint()?.clone();
    if cfg.proxy.is_some() {
        tracing::info!(host = %ep.host, "SFTP does not use the configured proxy: connecting directly");
    }
    let mut lease = site.conn(cfg).await?;
    let attrs = lease.get().stat(&ep.path, cfg).await?;
    lease.keep();
    if attrs.dir {
        return Err(EngineError::Invalid(format!(
            "{} is a directory, not a file",
            ep.path
        )));
    }
    Ok(RemoteInfo {
        final_url: site.url.clone(),
        total: attrs.size,
        accepts_ranges: attrs.size.is_some(),
        etag: None,
        etag_strong: false,
        last_modified: mtime_validator(attrs.mtime),
        filename: ep.filename(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ep(s: &str) -> Result<Endpoint, String> {
        Endpoint::parse(&url::Url::parse(s).unwrap())
    }

    #[test]
    fn endpoints() {
        let e = ep("sftp://example.org/srv/a%20b.iso").unwrap();
        assert_eq!(
            (e.host.as_str(), e.port, e.path.as_str()),
            ("example.org", 22, "/srv/a b.iso")
        );
        assert_eq!((e.user.as_deref(), e.pass.as_deref()), (None, None));
        assert_eq!(e.filename(), "a b.iso");
        let e = ep("sftp://bob:p%40ss@[::1]:2222/~/f").unwrap();
        assert_eq!(
            (e.host.as_str(), e.port, e.path.as_str()),
            ("::1", 2222, "f")
        );
        assert_eq!(
            (e.user.as_deref(), e.pass.as_deref()),
            (Some("bob"), Some("p@ss"))
        );
        assert!(ep("sftp://h/").is_err());
        assert!(ep("sftp://h/dir/").is_err());
        assert!(ep("sftp://h/~/").is_err());
        assert!(ep("sftp://h/a%00b").is_err());
        assert!(ep("ftp://h/a").is_err());
        let shown = format!("{:?}", ep("sftp://u:SECRET@h/f").unwrap());
        assert!(!shown.contains("SECRET"), "{shown}");
    }

    #[test]
    fn offered_agents_must_be_absolute() {
        offer_agent("relative/sock");
        offer_agent("");
        assert!(offered_agent().is_none_or(|p| p.is_absolute()));
    }
}
