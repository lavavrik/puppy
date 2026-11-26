//! A small in-process SSH server with an SFTP subsystem, for the SFTP transport's tests.
//!
//! It serves files from memory (replaceable while clients are connected: a "changed
//! remote file") with `OPEN`/`READ`/`STAT`/`FSTAT`/`CLOSE`, logs in with the committed
//! test keys (`keys/`) or a password (`password` or `keyboard-interactive`), and records
//! what clients did: login methods, read offsets, concurrent connections, agent
//! forwarding requests. A connection limit can be imposed (connections over it are
//! closed before the SSH banner, as OpenSSH's `MaxStartups` does), reads can be
//! throttled, shortened or failed once.
//!
//! Test keys (OpenSSH format, `keys/`): `host_ed25519` (the server's), `host_ed25519_other`
//! (a "changed" host key), `host_ecdsa` (a host key of another type), `user_ed25519` and `user_rsa` (unencrypted), `agent_ed25519`
//! (for a test agent), `user_encrypted_ed25519` (passphrase `puppy-test-passphrase`).

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use russh::keys::{PrivateKey, PublicKey};
use russh::server::{Auth, Msg, PublicKeyOffer, Response, Session, VerifiedPublicKey};
use russh::{Channel, ChannelId, MethodKind, MethodSet};
use russh_sftp::protocol::{
    Attrs, Data, FileAttributes, Handle, OpenFlags, SftpHandle, SftpString, Status, StatusCode,
    Version,
};
use tokio::net::{TcpListener, TcpStream};

/// The user every test logs in as.
pub const USER: &str = "puppy";

/// The path of a committed test key (`name.pub` for its public half).
pub fn key_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("keys")
        .join(name)
}

pub fn private_key(name: &str) -> PrivateKey {
    russh::keys::load_secret_key(key_path(name), None).expect("test key")
}

pub fn public_key(name: &str) -> PublicKey {
    russh::keys::load_public_key(key_path(&format!("{name}.pub"))).expect("test public key")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostKey {
    /// `keys/host_ed25519`.
    Main,
    /// `keys/host_ed25519_other`.
    Other,
    /// `keys/host_ecdsa` (another key type).
    Ecdsa,
}

impl HostKey {
    fn name(self) -> &'static str {
        match self {
            HostKey::Main => "host_ed25519",
            HostKey::Other => "host_ed25519_other",
            HostKey::Ecdsa => "host_ecdsa",
        }
    }
}

#[derive(Debug, Clone)]
pub struct SftpCfg {
    /// The password of [`USER`] (`None`: password logins fail).
    pub password: Option<String>,
    /// Offer `keyboard-interactive` for the password instead of `password`.
    pub keyboard_interactive: bool,
    /// Public keys [`USER`] may log in with (names of `keys/*.pub`).
    pub authorized: Vec<&'static str>,
    pub host_key: HostKey,
    /// Report modification times.
    pub mtime: bool,
    /// Each connection's reads are served at most this fast.
    pub bytes_per_sec: Option<u64>,
    /// At most this many connections at once; further ones are closed at once.
    pub max_connections: Option<usize>,
    /// Fail one read (connection lost) once this many bytes were served (once; re-armed
    /// by `set_cfg`).
    pub fail_after_bytes_once: Option<u64>,
    /// Answer reads with at most this many bytes (short reads).
    pub max_read: Option<u32>,
    /// Answer reads with this many bytes more than asked for (a broken server).
    pub oversize_reads: Option<u32>,
    /// Never answer the SFTP subsystem request, sending channel data every 50 ms
    /// instead (a server that keeps a client waiting).
    pub stall_subsystem: bool,
}

impl Default for SftpCfg {
    fn default() -> Self {
        SftpCfg {
            password: None,
            keyboard_interactive: false,
            authorized: vec![
                "user_ed25519",
                "user_rsa",
                "agent_ed25519",
                "user_encrypted_ed25519",
            ],
            host_key: HostKey::Main,
            mtime: true,
            bytes_per_sec: None,
            max_connections: None,
            fail_after_bytes_once: None,
            max_read: None,
            oversize_reads: None,
            stall_subsystem: false,
        }
    }
}

#[derive(Clone)]
struct File {
    data: Arc<Vec<u8>>,
    mtime: u32,
}

struct Shared {
    cfg: Mutex<SftpCfg>,
    files: Mutex<HashMap<String, File>>,
    logins: Mutex<Vec<String>>,
    reads: Mutex<Vec<u64>>,
    opens: AtomicUsize,
    connections: AtomicUsize,
    open_now: AtomicUsize,
    max_concurrent: AtomicUsize,
    turned_away: AtomicUsize,
    agent_requests: AtomicUsize,
    served: AtomicU64,
    fail_used: AtomicBool,
}

pub struct SftpServer {
    shared: Arc<Shared>,
    addr: SocketAddr,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for SftpServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Paths as stored: absolute; a relative one is in [`USER`]'s home (`/home/puppy`).
fn normalize(path: &str) -> String {
    if path.starts_with('/') {
        path.to_string()
    } else {
        format!("/home/{USER}/{path}")
    }
}

impl SftpServer {
    pub async fn start(cfg: SftpCfg) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("local addr");
        let shared = Arc::new(Shared {
            cfg: Mutex::new(cfg),
            files: Mutex::new(HashMap::new()),
            logins: Mutex::new(Vec::new()),
            reads: Mutex::new(Vec::new()),
            opens: AtomicUsize::new(0),
            connections: AtomicUsize::new(0),
            open_now: AtomicUsize::new(0),
            max_concurrent: AtomicUsize::new(0),
            turned_away: AtomicUsize::new(0),
            agent_requests: AtomicUsize::new(0),
            served: AtomicU64::new(0),
            fail_used: AtomicBool::new(false),
        });
        let s = shared.clone();
        let task = tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let s = s.clone();
                tokio::spawn(async move { connection(s, stream).await });
            }
        });
        SftpServer { shared, addr, task }
    }

    pub fn port(&self) -> u16 {
        self.addr.port()
    }

    /// `sftp://puppy@127.0.0.1:<port>/<path>` (`path` as given: `/abs` or `~/rel`).
    pub fn url(&self, path: &str) -> url::Url {
        let path = path.trim_start_matches('/');
        let path = if path.starts_with("~/") {
            path.to_string()
        } else {
            format!("/{path}")
        };
        url::Url::parse(&format!("sftp://{USER}@127.0.0.1:{}/{path}", self.port()))
            .map(|mut u| {
                // `sftp://h//abs` would be odd: one slash.
                let p = u.path().replacen("//", "/", 1);
                u.set_path(&p);
                u
            })
            .expect("url")
    }

    /// The `known_hosts` line for this server with `key` (`[127.0.0.1]:port key`).
    pub fn known_hosts_line(&self, key: HostKey) -> String {
        let k = public_key(key.name());
        format!(
            "[127.0.0.1]:{} {}\n",
            self.port(),
            k.to_openssh().expect("openssh public key")
        )
    }

    /// Serves `data` at `path` (absolute, or relative to the user's home) with
    /// modification time `mtime` (Unix seconds), replacing what was there.
    pub fn set_file(&self, path: &str, data: Vec<u8>, mtime: u32) {
        self.shared.files.lock().unwrap().insert(
            normalize(path),
            File {
                data: Arc::new(data),
                mtime,
            },
        );
    }

    pub fn cfg(&self) -> SftpCfg {
        self.shared.cfg.lock().unwrap().clone()
    }

    pub fn set_cfg(&self, f: impl FnOnce(&mut SftpCfg)) {
        f(&mut self.shared.cfg.lock().unwrap());
        self.shared.fail_used.store(false, Ordering::SeqCst);
        self.shared.served.store(0, Ordering::SeqCst);
    }

    /// Successful logins, as `method` (`publickey:<key comment>`, `password`,
    /// `keyboard-interactive`), in order.
    pub fn logins(&self) -> Vec<String> {
        self.shared.logins.lock().unwrap().clone()
    }

    /// The offsets of the reads served, in order.
    pub fn read_offsets(&self) -> Vec<u64> {
        self.shared.reads.lock().unwrap().clone()
    }

    /// Files opened.
    pub fn opens(&self) -> usize {
        self.shared.opens.load(Ordering::SeqCst)
    }

    /// Connections accepted (turned-away ones not counted).
    pub fn connections(&self) -> usize {
        self.shared.connections.load(Ordering::SeqCst)
    }

    /// Connections open now.
    pub fn open_connections(&self) -> usize {
        self.shared.open_now.load(Ordering::SeqCst)
    }

    pub fn max_concurrent(&self) -> usize {
        self.shared.max_concurrent.load(Ordering::SeqCst)
    }

    pub fn turned_away(&self) -> usize {
        self.shared.turned_away.load(Ordering::SeqCst)
    }

    /// `auth-agent-req@openssh.com` requests (agent forwarding) received.
    pub fn agent_requests(&self) -> usize {
        self.shared.agent_requests.load(Ordering::SeqCst)
    }
}

/// Counts a connection as open until dropped.
struct Open(Arc<Shared>);

impl Drop for Open {
    fn drop(&mut self) {
        self.0.open_now.fetch_sub(1, Ordering::SeqCst);
    }
}

async fn connection(shared: Arc<Shared>, stream: TcpStream) {
    let cfg = shared.cfg.lock().unwrap().clone();
    let n = shared.open_now.fetch_add(1, Ordering::SeqCst) + 1;
    let open = Open(shared.clone());
    if cfg.max_connections.is_some_and(|m| n > m) {
        shared.turned_away.fetch_add(1, Ordering::SeqCst);
        drop(stream);
        drop(open);
        return;
    }
    shared.connections.fetch_add(1, Ordering::SeqCst);
    shared.max_concurrent.fetch_max(n, Ordering::SeqCst);
    let mut methods = vec![MethodKind::PublicKey];
    methods.push(if cfg.keyboard_interactive {
        MethodKind::KeyboardInteractive
    } else {
        MethodKind::Password
    });
    let config = Arc::new(russh::server::Config {
        methods: MethodSet::from(&methods[..]),
        auth_rejection_time: Duration::from_millis(1),
        auth_rejection_time_initial: Some(Duration::ZERO),
        keys: vec![private_key(cfg.host_key.name())],
        nodelay: true,
        ..russh::server::Config::default()
    });
    let handler = Ssh {
        shared: shared.clone(),
        cfg,
        channels: HashMap::new(),
    };
    if let Ok(running) = russh::server::run_stream(config, stream, handler).await {
        let _ = running.await;
    }
    drop(open);
}

struct Ssh {
    shared: Arc<Shared>,
    cfg: SftpCfg,
    channels: HashMap<ChannelId, Channel<Msg>>,
}

impl Ssh {
    fn authorized(&self, user: &str, key: &PublicKey) -> Option<String> {
        if user != USER {
            return None;
        }
        self.cfg
            .authorized
            .iter()
            .map(|n| public_key(n))
            .find(|k| k.key_data() == key.key_data())
            .map(|k| k.comment().to_string())
    }

    fn login(&self, what: String) -> Auth {
        self.shared.logins.lock().unwrap().push(what);
        Auth::Accept
    }
}

impl russh::server::Handler for Ssh {
    type Error = russh::Error;

    async fn auth_none(&mut self, _user: &str) -> Result<Auth, Self::Error> {
        Ok(Auth::reject())
    }

    async fn auth_publickey_offered(
        &mut self,
        user: &str,
        offer: PublicKeyOffer<'_>,
    ) -> Result<Auth, Self::Error> {
        Ok(match self.authorized(user, offer.public_key) {
            Some(_) => Auth::Accept,
            None => Auth::reject(),
        })
    }

    async fn auth_publickey(
        &mut self,
        user: &str,
        key: VerifiedPublicKey<'_>,
    ) -> Result<Auth, Self::Error> {
        Ok(match self.authorized(user, key.public_key) {
            Some(comment) => self.login(format!("publickey:{comment}")),
            None => Auth::reject(),
        })
    }

    async fn auth_password(&mut self, user: &str, password: &str) -> Result<Auth, Self::Error> {
        if !self.cfg.keyboard_interactive
            && user == USER
            && self.cfg.password.as_deref() == Some(password)
        {
            return Ok(self.login("password".into()));
        }
        Ok(Auth::reject())
    }

    async fn auth_keyboard_interactive<'a>(
        &'a mut self,
        user: &str,
        _submethods: &str,
        response: Option<Response<'a>>,
    ) -> Result<Auth, Self::Error> {
        if !self.cfg.keyboard_interactive || user != USER {
            return Ok(Auth::reject());
        }
        let Some(mut response) = response else {
            return Ok(Auth::Partial {
                name: "".into(),
                instructions: "".into(),
                prompts: vec![("Password: ".into(), false)].into(),
            });
        };
        let answer = response.next();
        let ok = matches!((&answer, &self.cfg.password), (Some(a), Some(p)) if a.as_ref() == p.as_bytes());
        Ok(if ok {
            self.login("keyboard-interactive".into())
        } else {
            Auth::reject()
        })
    }

    async fn channel_open_session(
        &mut self,
        channel: Channel<Msg>,
        reply: russh::server::ChannelOpenHandle,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        self.channels.insert(channel.id(), channel);
        reply.accept().await;
        Ok(())
    }

    async fn agent_request(
        &mut self,
        _channel: ChannelId,
        _session: &mut Session,
    ) -> Result<bool, Self::Error> {
        self.shared.agent_requests.fetch_add(1, Ordering::SeqCst);
        Ok(false)
    }

    async fn channel_eof(
        &mut self,
        channel: ChannelId,
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        session.close(channel)?;
        Ok(())
    }

    async fn subsystem_request(
        &mut self,
        channel_id: ChannelId,
        name: &str,
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        if self.cfg.stall_subsystem {
            let handle = session.handle();
            tokio::spawn(async move {
                while handle.data(channel_id, &b"."[..]).await.is_ok() {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
            });
            return Ok(());
        }
        match (name, self.channels.remove(&channel_id)) {
            ("sftp", Some(channel)) => {
                session.channel_success(channel_id)?;
                let fs = Fs {
                    shared: self.shared.clone(),
                    handles: HashMap::new(),
                    next: 0,
                };
                russh_sftp::server::run(channel.into_stream(), fs).await;
            }
            _ => session.channel_failure(channel_id)?,
        }
        Ok(())
    }
}

struct Fs {
    shared: Arc<Shared>,
    /// Open handles: the path each reads (the file as it is at each read: replacing a
    /// file is like rewriting it in place).
    handles: HashMap<Vec<u8>, String>,
    next: u64,
}

fn status(id: u32, code: StatusCode) -> Status {
    Status {
        id,
        status_code: code,
        error_message: code.to_string(),
        language_tag: "en-US".into(),
    }
}

impl Fs {
    fn file(&self, path: &str) -> Option<File> {
        self.shared.files.lock().unwrap().get(path).cloned()
    }

    fn attrs(&self, path: &str) -> Result<FileAttributes, StatusCode> {
        let mtime = self.shared.cfg.lock().unwrap().mtime;
        if let Some(f) = self.file(path) {
            return Ok(FileAttributes {
                size: Some(f.data.len() as u64),
                permissions: Some(0o100644),
                mtime: mtime.then_some(f.mtime),
                atime: mtime.then_some(f.mtime),
                ..FileAttributes::empty()
            });
        }
        let dir = format!("{}/", path.trim_end_matches('/'));
        if self
            .shared
            .files
            .lock()
            .unwrap()
            .keys()
            .any(|k| k.starts_with(&dir))
        {
            return Ok(FileAttributes {
                permissions: Some(0o040755),
                ..FileAttributes::empty()
            });
        }
        Err(StatusCode::NoSuchFile)
    }
}

impl russh_sftp::server::Handler for Fs {
    type Error = StatusCode;

    fn unimplemented(&self) -> Self::Error {
        StatusCode::OpUnsupported
    }

    async fn init(
        &mut self,
        _version: u32,
        _extensions: HashMap<String, String>,
    ) -> Result<Version, Self::Error> {
        Ok(Version::new())
    }

    async fn stat(&mut self, id: u32, path: SftpString) -> Result<Attrs, Self::Error> {
        let path = normalize(&path);
        Ok(Attrs {
            id,
            attrs: self.attrs(&path)?,
        })
    }

    async fn lstat(&mut self, id: u32, path: SftpString) -> Result<Attrs, Self::Error> {
        self.stat(id, path).await
    }

    async fn open(
        &mut self,
        id: u32,
        filename: SftpString,
        _pflags: OpenFlags,
        _attrs: FileAttributes,
    ) -> Result<Handle, Self::Error> {
        let path = normalize(&filename);
        if self.file(&path).is_none() {
            return Err(StatusCode::NoSuchFile);
        }
        self.shared.opens.fetch_add(1, Ordering::SeqCst);
        self.next += 1;
        let handle = format!("h{}", self.next).into_bytes();
        self.handles.insert(handle.clone(), path);
        Ok(Handle {
            id,
            handle: SftpHandle::from(handle),
        })
    }

    async fn fstat(&mut self, id: u32, handle: SftpHandle) -> Result<Attrs, Self::Error> {
        let path = self
            .handles
            .get(handle.as_ref())
            .cloned()
            .ok_or(StatusCode::Failure)?;
        Ok(Attrs {
            id,
            attrs: self.attrs(&path)?,
        })
    }

    async fn read(
        &mut self,
        id: u32,
        handle: SftpHandle,
        offset: u64,
        len: u32,
    ) -> Result<Data, Self::Error> {
        let path = self
            .handles
            .get(handle.as_ref())
            .cloned()
            .ok_or(StatusCode::Failure)?;
        let file = self.file(&path).ok_or(StatusCode::NoSuchFile)?;
        let cfg = self.shared.cfg.lock().unwrap().clone();
        let size = file.data.len() as u64;
        if offset >= size {
            return Err(StatusCode::Eof);
        }
        let mut len = u64::from(len);
        if let Some(m) = cfg.max_read {
            len = len.min(u64::from(m));
        }
        if let Some(extra) = cfg.oversize_reads {
            len += u64::from(extra);
        }
        let end = (offset + len).min(size);
        let n = end - offset;
        let served = self.shared.served.fetch_add(n, Ordering::SeqCst) + n;
        if let Some(limit) = cfg.fail_after_bytes_once
            && served > limit
            && !self.shared.fail_used.swap(true, Ordering::SeqCst)
        {
            return Err(StatusCode::ConnectionLost);
        }
        if let Some(bps) = cfg.bytes_per_sec {
            tokio::time::sleep(Duration::from_secs_f64(n as f64 / bps as f64)).await;
        }
        self.shared.reads.lock().unwrap().push(offset);
        Ok(Data {
            id,
            data: file.data[offset as usize..end as usize].to_vec(),
        })
    }

    async fn close(&mut self, id: u32, handle: SftpHandle) -> Result<Status, Self::Error> {
        self.handles.remove(handle.as_ref());
        Ok(status(id, StatusCode::Ok))
    }
}

/// A test ssh-agent holding `keys` (names of `keys/*`) on a Unix socket at `sock`.
#[cfg(unix)]
pub async fn start_agent(sock: &std::path::Path, keys: &[&str]) -> tokio::task::JoinHandle<()> {
    let listener = tokio::net::UnixListener::bind(sock).expect("bind the agent socket");
    let incoming = futures_util::stream::unfold(listener, |l| async move {
        let r = l.accept().await.map(|(s, _)| s);
        Some((r, l))
    });
    let task = tokio::spawn(async move {
        let _ = russh::keys::agent::server::serve(Box::pin(incoming), ()).await;
    });
    let mut client = russh::keys::agent::client::AgentClient::connect_uds(sock)
        .await
        .expect("connect to the test agent");
    for k in keys {
        client
            .add_identity(&private_key(k), &[])
            .await
            .expect("add a key to the test agent");
    }
    task
}
