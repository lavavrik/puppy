//! One SFTP connection: TCP, the SSH handshake with the host key checked, the login
//! ([`super::auth`]) and an SFTP session on one channel.

use super::Endpoint;
use super::hostkey::{self, Known, Verdict};
use crate::config::{HttpConfig, SftpConfig};
use crate::engine::EngineError;
use crate::http::probe::timed;
use russh::client::{self, Msg, Session};
use russh::keys::{Algorithm, PublicKey, PublicKeyOrCertificate};
use russh::{Channel, ChannelOpenFailure};
use russh_sftp::client::RawSftpSession;
use russh_sftp::client::error::Error as SftpError;
use russh_sftp::protocol::{FileAttributes, OpenFlags, StatusCode};
use std::borrow::Cow;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::net::TcpStream;

/// Read requests ask for this much unless the server says (`limits@openssh.com`) it
/// serves more; every server must serve this much (SFTP drafts: 32768-byte packets).
const DEFAULT_READ: u32 = 32 * 1024;
/// The most a read request asks for (OpenSSH serves up to 256 KiB).
const MAX_READ: u64 = 255 * 1024;

/// Why a connection could not be opened.
#[derive(Debug)]
pub(crate) enum OpenError {
    /// The server closed the connection or refused the session before it was usable:
    /// with other connections logged in, a connection limit.
    Refused(String),
    Fatal(EngineError),
}

/// The SSH side of a connection: checks the host key, refuses every channel the server
/// would open (agent forwarding among them: never requested, never accepted).
pub(crate) struct Client {
    known: Known,
    /// Use a trusted key (else only record it: `puppy ssh trust`).
    accept: bool,
    seen: Arc<Mutex<Option<(PublicKey, Verdict)>>>,
}

async fn refuse(reply: client::ChannelOpenHandle) -> Result<(), russh::Error> {
    reply
        .reject(ChannelOpenFailure::AdministrativelyProhibited)
        .await;
    Ok(())
}

impl client::Handler for Client {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        key: &PublicKeyOrCertificate,
    ) -> Result<bool, Self::Error> {
        // Host certificates are never asked for (`host_key_certificates` is empty).
        let PublicKeyOrCertificate::PublicKey { key, .. } = key else {
            return Ok(false);
        };
        let verdict = self.known.verdict(key);
        let ok = self.accept && matches!(verdict, Verdict::Trusted);
        *self.seen.lock().unwrap_or_else(|e| e.into_inner()) = Some((key.clone(), verdict));
        Ok(ok)
    }

    async fn server_channel_open_agent_forward(
        &mut self,
        _channel: Channel<Msg>,
        reply: client::ChannelOpenHandle,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        refuse(reply).await
    }

    async fn server_channel_open_session(
        &mut self,
        _channel: Channel<Msg>,
        reply: client::ChannelOpenHandle,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        refuse(reply).await
    }

    async fn server_channel_open_forwarded_tcpip(
        &mut self,
        _channel: Channel<Msg>,
        _connected_address: &str,
        _connected_port: u32,
        _originator_address: &str,
        _originator_port: u32,
        reply: client::ChannelOpenHandle,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        refuse(reply).await
    }

    async fn server_channel_open_forwarded_streamlocal(
        &mut self,
        _channel: Channel<Msg>,
        _socket_path: &str,
        reply: client::ChannelOpenHandle,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        refuse(reply).await
    }

    async fn server_channel_open_direct_tcpip(
        &mut self,
        _channel: Channel<Msg>,
        _host_to_connect: &str,
        _port_to_connect: u32,
        _originator_address: &str,
        _originator_port: u32,
        reply: client::ChannelOpenHandle,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        refuse(reply).await
    }

    async fn server_channel_open_direct_streamlocal(
        &mut self,
        _channel: Channel<Msg>,
        _socket_path: &str,
        reply: client::ChannelOpenHandle,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        refuse(reply).await
    }

    async fn server_channel_open_x11(
        &mut self,
        _channel: Channel<Msg>,
        _originator_address: &str,
        _originator_port: u32,
        reply: client::ChannelOpenHandle,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        refuse(reply).await
    }
}

/// The `known_hosts` files to check: the configured ones, then puppy's own.
pub fn known_hosts_files(cfg: &SftpConfig) -> Vec<PathBuf> {
    let mut files = cfg.known_hosts.clone();
    files.extend(cfg.trusted_hosts.clone());
    files
}

/// Host key algorithms, those known for the host first (as OpenSSH orders them), never
/// `ssh-rsa` with SHA-1 (OpenSSH refuses it since 8.8).
fn host_key_order(known: &Known) -> Vec<Algorithm> {
    let all: Vec<Algorithm> = russh::Preferred::DEFAULT
        .key
        .iter()
        .filter(|a| !matches!(a, Algorithm::Rsa { hash: None }))
        .cloned()
        .collect();
    let mut out: Vec<Algorithm> = Vec::new();
    for k in known.algorithms() {
        for a in all.iter().filter(|a| hostkey::same_type(a, &k)) {
            if !out.contains(a) {
                out.push(a.clone());
            }
        }
    }
    for a in all {
        if !out.contains(&a) {
            out.push(a);
        }
    }
    out
}

fn client_config(known: &Known) -> Arc<client::Config> {
    Arc::new(client::Config {
        preferred: russh::Preferred {
            key: Cow::Owned(host_key_order(known)),
            ..russh::Preferred::DEFAULT
        },
        keepalive_interval: Some(Duration::from_secs(15)),
        keepalive_max: 3,
        nodelay: true,
        ..client::Config::default()
    })
}

async fn connect_tcp(host: &str, port: u16, timeout: Duration) -> Result<TcpStream, EngineError> {
    let addrs: Vec<_> = timed(timeout, tokio::net::lookup_host((host, port)))
        .await?
        .map_err(|e| EngineError::Network(format!("cannot resolve {host}: {e}")))?
        .collect();
    let mut last = None;
    for addr in addrs {
        match tokio::time::timeout(timeout, TcpStream::connect(addr)).await {
            Ok(Ok(s)) => return Ok(s),
            Ok(Err(e)) => last = Some(e.to_string()),
            Err(_) => last = Some("connection timed out".into()),
        }
    }
    Err(EngineError::Network(format!(
        "cannot connect to {host}:{port}: {}",
        last.unwrap_or_else(|| "no address".into())
    )))
}

/// Why the host key is not accepted, with what to do about it.
fn host_key_error(host: &str, port: u16, key: &PublicKey, verdict: &Verdict) -> EngineError {
    let name = hostkey::host_pattern(host, port);
    let arg = hostkey::host_arg(host, port);
    let shown = hostkey::describe(key);
    EngineError::Ssh(match verdict {
        Verdict::Trusted => format!("SSH: the host key of {name} was not accepted"),
        Verdict::Unknown { other_types: false } => format!(
            "SSH: the host key of {name} is not known ({shown}). Check the fingerprint \
             with the server's administrator; to trust it, run `puppy ssh trust {arg}`"
        ),
        Verdict::Unknown { other_types: true } => format!(
            "SSH: {name} offers only a key of a type not known for it ({shown}), while \
             keys of other types are: this can be a man-in-the-middle attack. Nothing was \
             sent to it. If the server's keys really changed, check the fingerprint with \
             its administrator and run `puppy ssh trust --replace-type {arg}`"
        ),
        Verdict::Changed { file, line } => format!(
            "SSH: REMOTE HOST IDENTIFICATION HAS CHANGED for {name}: it presents {shown}, \
             not the key in {}:{line}. Someone could be eavesdropping on you (a \
             man-in-the-middle attack), or the host key was just changed. Nothing was sent \
             to it. If the change is expected, remove the old key (`ssh-keygen -R '{name}'`, \
             or edit that file) and run `puppy ssh trust {arg}`",
            file.display()
        ),
        Verdict::Revoked { file, line } => format!(
            "SSH: the host key of {name} ({shown}) is revoked in {}:{line}",
            file.display()
        ),
    })
}

/// The SSH handshake over a new TCP connection; `accept`: go on with a trusted key.
/// Returns the session and the host key seen (with its verdict).
async fn handshake(
    host: &str,
    port: u16,
    cfg: &HttpConfig,
    accept: bool,
) -> (
    Result<client::Handle<Client>, OpenError>,
    Option<(PublicKey, Verdict)>,
) {
    let connect = Duration::from_millis(cfg.connect_timeout_ms);
    let read = Duration::from_millis(cfg.read_timeout_ms);
    let known = Known::lookup(&known_hosts_files(&cfg.sftp), host, port);
    let config = client_config(&known);
    let tcp = match connect_tcp(host, port, connect).await {
        Ok(t) => t,
        Err(e) => return (Err(OpenError::Fatal(e)), None),
    };
    let seen = Arc::new(Mutex::new(None));
    let handler = Client {
        known,
        accept,
        seen: seen.clone(),
    };
    let result =
        tokio::time::timeout(connect + read, client::connect_stream(config, tcp, handler)).await;
    let seen = seen.lock().unwrap_or_else(|e| e.into_inner()).take();
    let result = match result {
        Err(_) => Err(OpenError::Fatal(EngineError::Network(format!(
            "the SSH handshake with {host}:{port} timed out"
        )))),
        Ok(Ok(h)) => Ok(h),
        Ok(Err(e)) => Err(match &seen {
            Some((key, verdict)) if !matches!(verdict, Verdict::Trusted) => {
                OpenError::Fatal(host_key_error(host, port, key, verdict))
            }
            _ => handshake_error(host, port, e),
        }),
    };
    (result, seen)
}

fn handshake_error(host: &str, port: u16, e: russh::Error) -> OpenError {
    use russh::Error as E;
    match e {
        E::Disconnect | E::HUP | E::IO(_) | E::SendError | E::Version => OpenError::Refused(
            format!("the SSH server at {host}:{port} closed the connection ({e})"),
        ),
        E::NoCommonAlgo { .. } | E::WrongServerSig | E::UnknownKey => OpenError::Fatal(
            EngineError::Ssh(format!("SSH: cannot agree with {host}:{port}: {e}")),
        ),
        e => OpenError::Fatal(EngineError::Network(format!("SSH with {host}:{port}: {e}"))),
    }
}

/// Fetches the host key of `host:port` (for `puppy ssh trust`): what the server
/// presents, and what the `known_hosts` files of `cfg` say about it. Nothing is sent
/// after the key exchange.
pub async fn fetch_host_key(
    host: &str,
    port: u16,
    cfg: &HttpConfig,
) -> Result<(PublicKey, Verdict), EngineError> {
    let (result, seen) = handshake(host, port, cfg, false).await;
    match (result, seen) {
        (_, Some(seen)) => Ok(seen),
        (Ok(_), None) => Err(EngineError::Ssh(format!(
            "SSH: {host}:{port} presented no host key"
        ))),
        (Err(OpenError::Fatal(e)), None) => Err(e),
        (Err(OpenError::Refused(why)), None) => Err(EngineError::Network(why)),
    }
}

/// What `STAT`/`FSTAT` tell about a file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Attrs {
    pub size: Option<u64>,
    pub mtime: Option<u32>,
    pub dir: bool,
}

impl From<&FileAttributes> for Attrs {
    fn from(a: &FileAttributes) -> Self {
        Attrs {
            size: a.size,
            mtime: a.mtime,
            dir: a.permissions.is_some() && a.file_type().is_dir(),
        }
    }
}

/// An open remote file.
pub(crate) type FileHandle = russh_sftp::protocol::SftpHandle;

pub(crate) struct Conn {
    handle: client::Handle<Client>,
    sftp: RawSftpSession,
    /// The most a read request asks for.
    pub read_len: u32,
}

impl Conn {
    pub(crate) async fn open(ep: &Endpoint, cfg: &HttpConfig) -> Result<Conn, OpenError> {
        let read = Duration::from_millis(cfg.read_timeout_ms);
        let (handle, _) = handshake(&ep.host, ep.port, cfg, true).await;
        let mut handle = handle?;
        super::auth::login(&mut handle, ep, &cfg.sftp, read).await?;
        let refused = |what: &str, e: &dyn std::fmt::Display| {
            OpenError::Refused(format!(
                "the SSH server at {}:{} refused {what} ({e})",
                ep.host, ep.port
            ))
        };
        let mut channel = match timed(read, handle.channel_open_session()).await {
            Ok(Ok(c)) => c,
            Ok(Err(e)) => return Err(refused("a session", &e)),
            Err(e) => return Err(OpenError::Fatal(e)),
        };
        if let Err(e) = channel.request_subsystem(true, "sftp").await {
            return Err(refused("the SFTP subsystem", &e));
        }
        // The reply to the subsystem request, within one deadline however many other
        // channel messages come first.
        let deadline = tokio::time::Instant::now() + read;
        loop {
            let left = deadline.saturating_duration_since(tokio::time::Instant::now());
            match timed(left, channel.wait()).await {
                Ok(Some(russh::ChannelMsg::Success)) => break,
                Ok(Some(russh::ChannelMsg::Failure)) => {
                    return Err(OpenError::Fatal(EngineError::Ssh(format!(
                        "SSH: {}:{} does not offer SFTP",
                        ep.host, ep.port
                    ))));
                }
                Ok(Some(_)) => {}
                Ok(None) => return Err(refused("the SFTP subsystem", &"channel closed")),
                Err(e) => return Err(OpenError::Fatal(e)),
            }
        }
        let mut sftp = RawSftpSession::new(channel.into_stream());
        sftp.set_timeout(read.as_secs().max(1));
        let version = match timed(read, sftp.init()).await {
            Ok(Ok(v)) => v,
            Ok(Err(e)) => return Err(refused("an SFTP session", &e)),
            Err(e) => return Err(OpenError::Fatal(e)),
        };
        let mut read_len = DEFAULT_READ;
        if version
            .extensions
            .contains_key(russh_sftp::extensions::LIMITS)
            && let Ok(Ok(limits)) = timed(read, sftp.limits()).await
        {
            if limits.max_read_len > 0 {
                read_len = u32::try_from(limits.max_read_len.min(MAX_READ)).unwrap_or(DEFAULT_READ);
            }
            sftp.set_limits(limits.into());
        }
        Ok(Conn {
            handle,
            sftp,
            read_len,
        })
    }

    pub(crate) fn is_closed(&self) -> bool {
        self.handle.is_closed()
    }

    /// Closes the connection in the background (a polite disconnect, then gone).
    pub(crate) fn close_soon(self) {
        if let Ok(rt) = tokio::runtime::Handle::try_current() {
            rt.spawn(async move {
                let _ = self.sftp.close_session();
                let bye = self
                    .handle
                    .disconnect(russh::Disconnect::ByApplication, "", "");
                let _ = tokio::time::timeout(Duration::from_secs(2), bye).await;
            });
        }
    }

    pub(crate) async fn stat(&self, path: &str, cfg: &HttpConfig) -> Result<Attrs, EngineError> {
        let read = Duration::from_millis(cfg.read_timeout_ms);
        let a = timed(read, self.sftp.stat(path))
            .await?
            .map_err(|e| sftp_err(e, path))?;
        Ok(Attrs::from(&a.attrs))
    }

    pub(crate) async fn open_file(
        &self,
        path: &str,
        cfg: &HttpConfig,
    ) -> Result<FileHandle, EngineError> {
        let read = Duration::from_millis(cfg.read_timeout_ms);
        let h = timed(
            read,
            self.sftp
                .open(path, OpenFlags::READ, FileAttributes::empty()),
        )
        .await?
        .map_err(|e| sftp_err(e, path))?;
        Ok(h.handle)
    }

    pub(crate) async fn fstat(
        &self,
        h: &FileHandle,
        path: &str,
        cfg: &HttpConfig,
    ) -> Result<Attrs, EngineError> {
        let read = Duration::from_millis(cfg.read_timeout_ms);
        let a = timed(read, self.sftp.fstat(h.clone()))
            .await?
            .map_err(|e| sftp_err(e, path))?;
        Ok(Attrs::from(&a.attrs))
    }

    /// `len` bytes at `offset` (fewer if the server sends fewer); `None` at the end of
    /// the file. Several may be in flight at once.
    pub(crate) async fn read(
        &self,
        h: &FileHandle,
        path: &str,
        offset: u64,
        len: u32,
    ) -> Result<Option<Vec<u8>>, EngineError> {
        match self.sftp.read(h.clone(), offset, len).await {
            Ok(d) => Ok(Some(d.data)),
            Err(SftpError::Status(s)) if s.status_code == StatusCode::Eof => Ok(None),
            Err(e) => Err(sftp_err(e, path)),
        }
    }

    pub(crate) async fn close_file(&self, h: FileHandle, cfg: &HttpConfig) {
        let read = Duration::from_millis(cfg.read_timeout_ms).min(Duration::from_secs(5));
        let _ = timed(read, self.sftp.close(h)).await;
    }
}

/// An SFTP error as an engine error: missing files and refused permissions are
/// permanent, the rest (lost connections, timeouts, generic failures) transient.
pub(crate) fn sftp_err(e: SftpError, path: &str) -> EngineError {
    match e {
        SftpError::Status(s) => {
            let detail = if s.error_message.trim().is_empty() {
                s.status_code.to_string()
            } else {
                s.error_message.trim().to_string()
            };
            match s.status_code {
                StatusCode::NoSuchFile => EngineError::Ssh(format!("SFTP: no such file: {path}")),
                StatusCode::PermissionDenied => {
                    EngineError::Ssh(format!("SFTP: permission denied: {path} ({detail})"))
                }
                StatusCode::OpUnsupported | StatusCode::BadMessage => {
                    EngineError::Ssh(format!("SFTP: {detail} ({path})"))
                }
                _ => EngineError::Network(format!("SFTP: {detail} ({path})")),
            }
        }
        SftpError::Timeout => EngineError::Network("SFTP: the server did not answer".into()),
        e => EngineError::Network(format!("SFTP: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use russh::keys::HashAlg;

    #[test]
    fn known_types_come_first_and_sha1_rsa_never() {
        let order = host_key_order(&Known::default());
        assert_eq!(order[0], Algorithm::Ed25519);
        assert!(!order.contains(&Algorithm::Rsa { hash: None }));
        let rsa =
            PublicKey::from_openssh(include_str!("../../../puppy-testserver/keys/user_rsa.pub"))
                .unwrap();
        let known = Known {
            entries: vec![hostkey::Entry {
                file: PathBuf::new(),
                line: 1,
                key: rsa,
                revoked: false,
            }],
        };
        let order = host_key_order(&known);
        assert_eq!(
            order[..2],
            [
                Algorithm::Rsa {
                    hash: Some(HashAlg::Sha512)
                },
                Algorithm::Rsa {
                    hash: Some(HashAlg::Sha256)
                }
            ]
        );
    }
}
