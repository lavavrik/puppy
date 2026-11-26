//! Engine configuration.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Whether `ftp://` sessions use TLS (`AUTH TLS`, explicit FTPS). `ftps://` (implicit
/// FTPS) and `ftpes://` (explicit FTPS, required) URLs choose for themselves.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FtpTls {
    /// Plain FTP: credentials and data travel unencrypted.
    Off,
    /// Ask for TLS and use it if the server offers it; carry on in plain FTP only if the
    /// server refuses `AUTH TLS`. A certificate that does not verify fails the download.
    #[default]
    Try,
    /// TLS or nothing: a server refusing `AUTH TLS` fails the download.
    Require,
}

#[derive(Debug, Clone, Default)]
pub struct FtpConfig {
    pub tls: FtpTls,
    /// More trusted root certificates (DER) for FTPS, besides the system's (tests).
    pub extra_roots: Vec<Vec<u8>>,
}

/// Which ssh-agent SFTP logins ask to sign (see [`crate::sftp`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum SshAgent {
    /// The agent of the client that added the download (`RequestContext::ssh_agent`),
    /// those clients offered with their `hello` (the most recent first, a few kept),
    /// then this process's own: `SSH_AUTH_SOCK`, and on Windows the OpenSSH agent's
    /// named pipe and Pageant. An agent that is gone or holds no keys is passed over.
    #[default]
    Auto,
    /// This socket (Unix) or named pipe (Windows) only.
    Path(PathBuf),
    /// No agent.
    Off,
}

/// SFTP logins and host keys.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SftpConfig {
    pub agent: SshAgent,
    /// Private keys tried after the agent's, in order (a missing file is skipped; an
    /// encrypted one cannot be used: its passphrase cannot be asked for).
    pub identity_files: Vec<PathBuf>,
    /// `known_hosts` files a server's host key is looked up in (OpenSSH format, hashed
    /// entries included).
    pub known_hosts: Vec<PathBuf>,
    /// puppy's own `known_hosts`, written by `puppy ssh trust` (and looked up too).
    pub trusted_hosts: Option<PathBuf>,
}

impl Default for SftpConfig {
    /// The user's: `~/.ssh/id_ed25519`, `id_ecdsa`, `id_rsa` and `~/.ssh/known_hosts`.
    fn default() -> Self {
        let ssh = dirs::home_dir().map(|h| h.join(".ssh"));
        let in_ssh = |names: &[&str]| -> Vec<PathBuf> {
            ssh.iter()
                .flat_map(|d| names.iter().map(move |n| d.join(n)))
                .collect()
        };
        SftpConfig {
            agent: SshAgent::Auto,
            identity_files: in_ssh(&["id_ed25519", "id_ecdsa", "id_rsa"]),
            known_hosts: in_ssh(&["known_hosts"]),
            trusted_hosts: None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct HttpConfig {
    pub connections: u16,
    pub min_segment_bytes: u64,
    pub max_retries: u32,
    pub backoff_base_ms: u64,
    pub backoff_max_ms: u64,
    pub flush_interval_ms: u64,
    pub spot_check_bytes: u64,
    /// TCP/TLS connect timeout.
    pub connect_timeout_ms: u64,
    /// Idle timeout per network wait (response headers or the next body chunk), measured
    /// only while actually waiting on the server; a stalled connection errors out.
    pub read_timeout_ms: u64,
    /// Upper bound for validating a remote file before resuming it.
    pub validate_timeout_ms: u64,
    /// HTTP(S) proxy for HTTP(S) downloads. FTP and SFTP connections never use it.
    pub proxy: Option<url::Url>,
    pub default_dir: PathBuf,
    pub ftp: FtpConfig,
    pub sftp: SftpConfig,
}

impl Default for HttpConfig {
    fn default() -> Self {
        Self {
            connections: 8,
            min_segment_bytes: 1024 * 1024,
            max_retries: 8,
            backoff_base_ms: 500,
            backoff_max_ms: 30_000,
            flush_interval_ms: 1000,
            spot_check_bytes: 64 * 1024,
            connect_timeout_ms: 15_000,
            read_timeout_ms: 30_000,
            validate_timeout_ms: 60_000,
            proxy: None,
            default_dir: dirs::download_dir()
                .or_else(|| dirs::home_dir().map(|h| h.join("Downloads")))
                .unwrap_or_else(|| PathBuf::from("Downloads")),
            ftp: FtpConfig::default(),
            sftp: SftpConfig::default(),
        }
    }
}
