//! A small in-process FTP(S) server for the FTP transport's tests.
//!
//! It speaks just enough of RFC 959/2228/2428/3659 for a download client: login, `TYPE`,
//! `EPSV`/`PASV`, `SIZE`, `MDTM`, `REST`, `RETR`, `FEAT`, explicit (`AUTH TLS`) and
//! implicit FTPS with `PBSZ`/`PROT`. Every feature a client may rely on can be switched
//! off, a session limit can be imposed (answered with 421 or 530, like real servers do),
//! and what clients did (commands, `REST` offsets, logins, concurrent sessions) is
//! recorded for assertions. Files live in memory and can be replaced while clients are
//! connected (a "changed remote file").

use std::collections::HashMap;
use std::io;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::TlsAcceptor;
use tokio_rustls::rustls::{self, pki_types::pem::PemObject};

/// The test CA (PEM) that signed the server certificate (`localhost`, `127.0.0.1`).
pub const TEST_CA_PEM: &str = include_str!("../certs/ca.pem");
const SERVER_CERT_PEM: &str = include_str!("../certs/server.pem");
const SERVER_KEY_PEM: &str = include_str!("../certs/server.key");

/// The test CA as DER, for a client's trusted roots.
pub fn test_ca_der() -> Vec<u8> {
    rustls::pki_types::CertificateDer::from_pem_slice(TEST_CA_PEM.as_bytes())
        .expect("bundled test CA")
        .to_vec()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FtpTls {
    /// Plain FTP: `AUTH TLS` is refused (502).
    Off,
    /// `AUTH TLS` on the plain control connection (explicit FTPS).
    Explicit,
    /// TLS from the first byte (implicit FTPS, `ftps://`).
    Implicit,
}

/// How a session over the limit is turned away.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LimitReply {
    /// `421` instead of the greeting (vsftpd, pure-ftpd).
    Greeting421,
    /// `530` in answer to `PASS` (ProFTPD's `MaxClientsPerHost`).
    Login530,
}

#[derive(Debug, Clone)]
pub struct FtpCfg {
    /// `Some((user, password))`: only that login works. `None`: anonymous (any user).
    pub login: Option<(String, String)>,
    pub tls: FtpTls,
    pub size: bool,
    pub mdtm: bool,
    pub rest: bool,
    pub epsv: bool,
    pub feat: bool,
    /// At most this many sessions at once; further ones are turned away.
    pub max_sessions: Option<usize>,
    pub limit_reply: LimitReply,
    /// Data connections send at most this fast.
    pub bytes_per_sec: Option<u64>,
    /// Abort one data connection after this many bytes (once; re-armed by `set_cfg`).
    pub fail_after_bytes_once: Option<u64>,
    /// Accept `AUTH TLS` but refuse `PROT P` (534), as servers whose TLS is set up for
    /// the control connection only do.
    pub refuse_prot: bool,
}

impl Default for FtpCfg {
    fn default() -> Self {
        FtpCfg {
            login: None,
            tls: FtpTls::Off,
            size: true,
            mdtm: true,
            rest: true,
            epsv: true,
            feat: true,
            max_sessions: None,
            limit_reply: LimitReply::Greeting421,
            bytes_per_sec: None,
            fail_after_bytes_once: None,
            refuse_prot: false,
        }
    }
}

#[derive(Clone)]
struct File {
    data: Arc<Vec<u8>>,
    mdtm: String,
}

struct Shared {
    cfg: Mutex<FtpCfg>,
    files: Mutex<HashMap<String, File>>,
    commands: Mutex<Vec<String>>,
    rest_offsets: Mutex<Vec<u64>>,
    retrs: AtomicUsize,
    logins: AtomicUsize,
    sessions: AtomicUsize,
    max_concurrent: AtomicUsize,
    turned_away: AtomicUsize,
    fail_used: AtomicBool,
    acceptor: TlsAcceptor,
}

pub struct FtpServer {
    shared: Arc<Shared>,
    addr: SocketAddr,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for FtpServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn acceptor() -> TlsAcceptor {
    let certs = rustls::pki_types::CertificateDer::pem_slice_iter(SERVER_CERT_PEM.as_bytes())
        .collect::<Result<Vec<_>, _>>()
        .expect("bundled test certificate");
    let key = rustls::pki_types::PrivateKeyDer::from_pem_slice(SERVER_KEY_PEM.as_bytes())
        .expect("bundled test key");
    let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    let cfg = rustls::ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .expect("default TLS versions")
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .expect("test certificate and key match");
    TlsAcceptor::from(Arc::new(cfg))
}

impl FtpServer {
    pub async fn start(cfg: FtpCfg) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("local addr");
        let shared = Arc::new(Shared {
            cfg: Mutex::new(cfg),
            files: Mutex::new(HashMap::new()),
            commands: Mutex::new(Vec::new()),
            rest_offsets: Mutex::new(Vec::new()),
            retrs: AtomicUsize::new(0),
            logins: AtomicUsize::new(0),
            sessions: AtomicUsize::new(0),
            max_concurrent: AtomicUsize::new(0),
            turned_away: AtomicUsize::new(0),
            fail_used: AtomicBool::new(false),
            acceptor: acceptor(),
        });
        let s = shared.clone();
        let task = tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let s = s.clone();
                tokio::spawn(async move {
                    let _ = session(s, stream).await;
                });
            }
        });
        FtpServer { shared, addr, task }
    }

    pub fn port(&self) -> u16 {
        self.addr.port()
    }

    /// `ftp://127.0.0.1:<port>/<path>` (`ftps://` for an implicit-TLS server).
    pub fn url(&self, path: &str) -> url::Url {
        let scheme = match self.cfg().tls {
            FtpTls::Implicit => "ftps",
            _ => "ftp",
        };
        self.url_with(scheme, path)
    }

    /// `<scheme>://127.0.0.1:<port>/<path>`.
    pub fn url_with(&self, scheme: &str, path: &str) -> url::Url {
        let path = path.trim_start_matches('/');
        url::Url::parse(&format!("{scheme}://127.0.0.1:{}/{path}", self.port())).expect("url")
    }

    /// Serves `data` at `path` with modification time `mdtm` (`YYYYMMDDHHMMSS`),
    /// replacing what was there.
    pub fn set_file(&self, path: &str, data: Vec<u8>, mdtm: &str) {
        self.shared.files.lock().unwrap().insert(
            path.trim_start_matches('/').to_string(),
            File {
                data: Arc::new(data),
                mdtm: mdtm.to_string(),
            },
        );
    }

    pub fn cfg(&self) -> FtpCfg {
        self.shared.cfg.lock().unwrap().clone()
    }

    pub fn set_cfg(&self, f: impl FnOnce(&mut FtpCfg)) {
        f(&mut self.shared.cfg.lock().unwrap());
        self.shared.fail_used.store(false, Ordering::SeqCst);
    }

    /// Every command received, as sent (`PASS` included), in order.
    pub fn commands(&self) -> Vec<String> {
        self.shared.commands.lock().unwrap().clone()
    }

    /// The offsets of the `REST` commands received, in order.
    pub fn rest_offsets(&self) -> Vec<u64> {
        self.shared.rest_offsets.lock().unwrap().clone()
    }

    /// `RETR`s that opened a data connection.
    pub fn retrs(&self) -> usize {
        self.shared.retrs.load(Ordering::SeqCst)
    }

    pub fn logins(&self) -> usize {
        self.shared.logins.load(Ordering::SeqCst)
    }

    /// The most sessions that were open at once (turned-away ones not counted).
    pub fn max_concurrent(&self) -> usize {
        self.shared.max_concurrent.load(Ordering::SeqCst)
    }

    /// Sessions refused for being over `max_sessions`.
    pub fn turned_away(&self) -> usize {
        self.shared.turned_away.load(Ordering::SeqCst)
    }

    /// Sessions open now.
    pub fn sessions(&self) -> usize {
        self.shared.sessions.load(Ordering::SeqCst)
    }
}

enum Conn {
    Plain(TcpStream),
    Tls(Box<tokio_rustls::server::TlsStream<TcpStream>>),
    Gone,
}

impl Conn {
    async fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Conn::Plain(s) => s.read(buf).await,
            Conn::Tls(s) => s.read(buf).await,
            Conn::Gone => Ok(0),
        }
    }

    async fn write_all(&mut self, b: &[u8]) -> io::Result<()> {
        match self {
            Conn::Plain(s) => s.write_all(b).await,
            Conn::Tls(s) => {
                s.write_all(b).await?;
                s.flush().await
            }
            Conn::Gone => Err(io::ErrorKind::NotConnected.into()),
        }
    }
}

/// Counts a session while it is open.
struct Open(Arc<Shared>);

impl Drop for Open {
    fn drop(&mut self) {
        self.0.sessions.fetch_sub(1, Ordering::SeqCst);
    }
}

struct Session {
    s: Arc<Shared>,
    ctrl: Conn,
    buf: Vec<u8>,
    user: Option<String>,
    logged_in: bool,
    over_limit: bool,
    protect: bool,
    rest: u64,
    passive: Option<TcpListener>,
}

async fn session(s: Arc<Shared>, stream: TcpStream) -> io::Result<()> {
    let cfg = s.cfg.lock().unwrap().clone();
    let now = s.sessions.fetch_add(1, Ordering::SeqCst) + 1;
    let _open = Open(s.clone());
    let over_limit = cfg.max_sessions.is_some_and(|m| now > m);
    if !over_limit {
        s.max_concurrent.fetch_max(now, Ordering::SeqCst);
    }
    let ctrl = match cfg.tls {
        FtpTls::Implicit => Conn::Tls(Box::new(s.acceptor.accept(stream).await?)),
        _ => Conn::Plain(stream),
    };
    let mut se = Session {
        s: s.clone(),
        ctrl,
        buf: Vec::new(),
        user: None,
        logged_in: false,
        over_limit,
        protect: false,
        rest: 0,
        passive: None,
    };
    if over_limit && cfg.limit_reply == LimitReply::Greeting421 {
        s.turned_away.fetch_add(1, Ordering::SeqCst);
        se.reply("421 Too many connections (test server)").await?;
        return Ok(());
    }
    se.reply("220 puppy test FTP server ready").await?;
    while let Some(line) = se.line().await? {
        s.commands.lock().unwrap().push(line.clone());
        let (cmd, arg) = match line.split_once(' ') {
            Some((c, a)) => (c.to_ascii_uppercase(), a.to_string()),
            None => (line.to_ascii_uppercase(), String::new()),
        };
        if !se.command(&cmd, &arg).await? {
            break;
        }
    }
    Ok(())
}

impl Session {
    async fn reply(&mut self, text: &str) -> io::Result<()> {
        self.ctrl.write_all(format!("{text}\r\n").as_bytes()).await
    }

    /// The next command line (`None` at EOF).
    async fn line(&mut self) -> io::Result<Option<String>> {
        loop {
            if let Some(i) = self.buf.windows(2).position(|w| w == b"\r\n") {
                let line = String::from_utf8_lossy(&self.buf[..i]).into_owned();
                self.buf.drain(..i + 2);
                return Ok(Some(line));
            }
            let mut chunk = [0u8; 1024];
            let n = self.ctrl.read(&mut chunk).await?;
            if n == 0 {
                return Ok(None);
            }
            self.buf.extend_from_slice(&chunk[..n]);
        }
    }

    fn cfg(&self) -> FtpCfg {
        self.s.cfg.lock().unwrap().clone()
    }

    fn file(&self, arg: &str) -> Option<File> {
        let path = arg.trim_start_matches('/');
        self.s.files.lock().unwrap().get(path).cloned()
    }

    /// Handles one command; `false` ends the session.
    async fn command(&mut self, cmd: &str, arg: &str) -> io::Result<bool> {
        let cfg = self.cfg();
        let public = matches!(
            cmd,
            "USER" | "PASS" | "AUTH" | "PBSZ" | "PROT" | "FEAT" | "QUIT" | "NOOP" | "SYST"
        );
        if !public && !self.logged_in {
            self.reply("530 Please login with USER and PASS").await?;
            return Ok(true);
        }
        match cmd {
            "USER" => {
                self.user = Some(arg.to_string());
                self.reply("331 Password required").await?;
            }
            "PASS" => {
                let ok = match &cfg.login {
                    Some((u, p)) => self.user.as_deref() == Some(u) && arg == p,
                    None => true,
                };
                if self.over_limit {
                    self.s.turned_away.fetch_add(1, Ordering::SeqCst);
                    self.reply(
                        "530 Sorry, the maximum number of clients from your host are already connected",
                    )
                    .await?;
                    return Ok(false);
                } else if ok {
                    self.logged_in = true;
                    self.s.logins.fetch_add(1, Ordering::SeqCst);
                    self.reply("230 Login successful").await?;
                } else {
                    self.reply("530 Login incorrect").await?;
                }
            }
            "AUTH" if cfg.tls == FtpTls::Explicit && matches!(self.ctrl, Conn::Plain(_)) => {
                self.reply("234 Proceed with negotiation").await?;
                let Conn::Plain(tcp) = std::mem::replace(&mut self.ctrl, Conn::Gone) else {
                    unreachable!("checked above");
                };
                self.ctrl = Conn::Tls(Box::new(self.s.acceptor.accept(tcp).await?));
            }
            "PBSZ" if cfg.tls != FtpTls::Off => self.reply("200 PBSZ=0").await?,
            "PROT" if cfg.refuse_prot => {
                self.reply("534 Request denied for policy reasons").await?
            }
            "PROT" if cfg.tls != FtpTls::Off => {
                self.protect = arg.trim().eq_ignore_ascii_case("P");
                self.reply("200 Protection level set").await?;
            }
            "FEAT" if cfg.feat => {
                let mut lines = vec!["211-Features:".to_string()];
                if cfg.epsv {
                    lines.push(" EPSV".into());
                }
                if cfg.mdtm {
                    lines.push(" MDTM".into());
                }
                lines.push(" PASV".into());
                if cfg.rest {
                    lines.push(" REST STREAM".into());
                }
                if cfg.size {
                    lines.push(" SIZE".into());
                }
                if cfg.tls == FtpTls::Explicit {
                    lines.push(" AUTH TLS".into());
                }
                if cfg.tls != FtpTls::Off {
                    lines.push(" PBSZ".into());
                    lines.push(" PROT".into());
                }
                lines.push("211 End".into());
                self.reply(&lines.join("\r\n")).await?;
            }
            "SYST" => self.reply("215 UNIX Type: L8").await?,
            "NOOP" => self.reply("200 NOOP ok").await?,
            "TYPE" => self.reply("200 Switching to Binary mode").await?,
            "OPTS" => self.reply("200 OK").await?,
            "PWD" => self.reply("257 \"/\" is the current directory").await?,
            "CWD" => self.reply("250 Directory successfully changed").await?,
            "EPSV" if cfg.epsv => {
                let l = TcpListener::bind("127.0.0.1:0").await?;
                let port = l.local_addr()?.port();
                self.passive = Some(l);
                self.reply(&format!("229 Entering Extended Passive Mode (|||{port}|)"))
                    .await?;
            }
            "PASV" => {
                let l = TcpListener::bind("127.0.0.1:0").await?;
                let port = l.local_addr()?.port();
                self.passive = Some(l);
                // A bogus address, as NATed servers send: clients must use the control
                // connection's peer instead.
                self.reply(&format!(
                    "227 Entering Passive Mode (10,255,255,1,{},{})",
                    port / 256,
                    port % 256
                ))
                .await?;
            }
            "SIZE" if cfg.size => match self.file(arg) {
                Some(f) => self.reply(&format!("213 {}", f.data.len())).await?,
                None => self.reply("550 Could not get file size").await?,
            },
            "MDTM" if cfg.mdtm => match self.file(arg) {
                Some(f) => self.reply(&format!("213 {}", f.mdtm)).await?,
                None => {
                    self.reply("550 Could not get file modification time")
                        .await?
                }
            },
            "REST" if cfg.rest => match arg.trim().parse::<u64>() {
                Ok(n) => {
                    self.rest = n;
                    self.s.rest_offsets.lock().unwrap().push(n);
                    self.reply(&format!("350 Restart position accepted ({n})"))
                        .await?;
                }
                Err(_) => self.reply("501 Bad REST argument").await?,
            },
            "RETR" => self.retr(arg, &cfg).await?,
            "ABOR" => self.reply("226 Nothing to abort").await?,
            "QUIT" => {
                self.reply("221 Goodbye").await?;
                return Ok(false);
            }
            _ => self.reply("502 Command not implemented").await?,
        }
        Ok(true)
    }

    async fn retr(&mut self, arg: &str, cfg: &FtpCfg) -> io::Result<()> {
        let offset = std::mem::take(&mut self.rest);
        let Some(file) = self.file(arg) else {
            self.passive = None;
            return self.reply("550 Failed to open file").await;
        };
        let Some(listener) = self.passive.take() else {
            return self.reply("425 Use PASV or EPSV first").await;
        };
        let accepted = tokio::time::timeout(Duration::from_secs(10), listener.accept()).await;
        let Ok(Ok((tcp, _))) = accepted else {
            return self.reply("425 Failed to establish connection").await;
        };
        self.s.retrs.fetch_add(1, Ordering::SeqCst);
        let len = file.data.len() as u64;
        self.reply(&format!(
            "150 Opening BINARY mode data connection ({} bytes)",
            len.saturating_sub(offset)
        ))
        .await?;
        let from = usize::try_from(offset.min(len)).unwrap_or(usize::MAX);
        let body = &file.data[from..];
        let fail_at = cfg
            .fail_after_bytes_once
            .filter(|_| !self.s.fail_used.swap(true, Ordering::SeqCst));
        let sent = if self.protect {
            match self.s.acceptor.accept(tcp).await {
                Ok(tls) => send(tls, body, cfg.bytes_per_sec, fail_at).await,
                Err(_) => Err(io::ErrorKind::ConnectionAborted.into()),
            }
        } else {
            send(tcp, body, cfg.bytes_per_sec, fail_at).await
        };
        match sent {
            Ok(()) => self.reply("226 Transfer complete").await,
            Err(_) => self.reply("426 Connection closed; transfer aborted").await,
        }
    }
}

/// Sends `body` (throttled), then closes the connection cleanly (TLS `close_notify`
/// included). With `fail_at`, aborts after that many bytes instead.
async fn send<S: AsyncWrite + AsyncRead + Unpin>(
    mut s: S,
    body: &[u8],
    bps: Option<u64>,
    fail_at: Option<u64>,
) -> io::Result<()> {
    const CHUNK: usize = 16 * 1024;
    let mut sent = 0usize;
    while sent < body.len() {
        let mut n = CHUNK.min(body.len() - sent);
        if let Some(at) = fail_at {
            let at = usize::try_from(at).unwrap_or(usize::MAX);
            if sent >= at {
                return Err(io::ErrorKind::ConnectionAborted.into());
            }
            n = n.min(at - sent);
        }
        s.write_all(&body[sent..sent + n]).await?;
        sent += n;
        if let Some(bps) = bps.filter(|&b| b > 0) {
            tokio::time::sleep(Duration::from_secs_f64(n as f64 / bps as f64)).await;
        }
    }
    s.shutdown().await
}
