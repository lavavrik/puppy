//! Logging in without prompting (the daemon has no terminal), in this order:
//!
//! 1. the keys of an ssh-agent ([`SshAgent`]), the agent connection bound to the
//!    session (`session-bind@openssh.com`, not for forwarding) where it supports that;
//! 2. the configured private key files, unencrypted ones only (an encrypted one needs
//!    its passphrase, which cannot be asked for: it must be loaded into the agent);
//! 3. the URL's password (`password`, or `keyboard-interactive` answering the password
//!    prompt), sent only to that URL's host.
//!
//! What the server does not accept (by its list of methods) is not tried. Failing, the
//! error says what was tried and, for encrypted keys, to run `ssh-add`.

use super::Endpoint;
use super::conn::{Client, OpenError};
use crate::config::{SftpConfig, SshAgent};
use crate::engine::EngineError;
use crate::http::probe::timed;
use russh::client::{AuthResult, Handle, KeyboardInteractiveAuthResponse};
use russh::keys::agent::AgentIdentity;
use russh::keys::agent::client::{AgentClient, AgentStream};
use russh::keys::{HashAlg, PrivateKeyWithHashAlg, PublicKey};
use russh::{MethodKind, MethodSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

type Agent = AgentClient<Box<dyn AgentStream + Send + Unpin + 'static>>;

/// The local user name (the default, as for `ssh`).
fn local_user() -> Option<String> {
    ["USER", "USERNAME", "LOGNAME"]
        .iter()
        .find_map(|v| std::env::var(v).ok().filter(|u| !u.is_empty()))
}

/// `~/x` for a path in the home directory (messages).
fn shown(path: &Path) -> String {
    match dirs::home_dir().and_then(|h| path.strip_prefix(&h).ok().map(Path::to_path_buf)) {
        Some(rel) => format!("~/{}", rel.display()),
        None => path.display().to_string(),
    }
}

#[cfg(unix)]
async fn connect_agent_at(path: &Path) -> Option<Agent> {
    match AgentClient::connect_uds(path).await {
        Ok(a) => Some(a.dynamic()),
        Err(e) => {
            tracing::debug!(agent = %path.display(), "cannot use the ssh-agent: {e}");
            None
        }
    }
}

#[cfg(windows)]
async fn connect_agent_at(path: &Path) -> Option<Agent> {
    match AgentClient::connect_named_pipe(path).await {
        Ok(a) => Some(a.dynamic()),
        Err(e) => {
            tracing::debug!(agent = %path.display(), "cannot use the ssh-agent: {e}");
            None
        }
    }
}

#[cfg(not(any(unix, windows)))]
async fn connect_agent_at(_path: &Path) -> Option<Agent> {
    None
}

/// The agents to try, in order: for [`SshAgent::Auto`] the one of the client that
/// added the download, those other clients offered (the most recent first), then this
/// process's own (`SSH_AUTH_SOCK`; on Windows the OpenSSH agent's pipe, then Pageant).
fn agent_candidates(cfg: &SshAgent, ep: &Endpoint) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    match cfg {
        SshAgent::Off => {}
        SshAgent::Path(p) => out.push(p.clone()),
        SshAgent::Auto => {
            out.extend(ep.agent.clone());
            out.extend(super::offered_agents());
            out.extend(
                std::env::var_os("SSH_AUTH_SOCK")
                    .filter(|v| !v.is_empty())
                    .map(PathBuf::from),
            );
            #[cfg(windows)]
            out.push(PathBuf::from(r"\\.\pipe\openssh-ssh-agent"));
        }
    }
    let mut seen = Vec::new();
    out.retain(|p| {
        let new = !seen.contains(p);
        seen.push(p.clone());
        new
    });
    out
}

/// The agents that answer, in [`agent_candidates`] order.
async fn agents(cfg: &SshAgent, ep: &Endpoint) -> Vec<(String, Agent)> {
    let mut out = Vec::new();
    for c in agent_candidates(cfg, ep) {
        if let Some(a) = connect_agent_at(&c).await {
            out.push((c.display().to_string(), a));
        }
    }
    #[cfg(windows)]
    if matches!(cfg, SshAgent::Auto)
        && let Ok(a) = AgentClient::connect_pageant().await
    {
        out.push(("Pageant".to_string(), a.dynamic()));
    }
    out
}

/// What a login attempt learned.
struct Attempts {
    methods: MethodSet,
    tried: Vec<String>,
    encrypted: Vec<PathBuf>,
}

impl Attempts {
    fn allows(&self, m: MethodKind) -> bool {
        self.methods.contains(&m)
    }

    /// Updates the methods the server still accepts; `true` when logged in.
    fn result(&mut self, r: AuthResult) -> bool {
        match r {
            AuthResult::Success => true,
            AuthResult::Failure {
                remaining_methods, ..
            } => {
                if !remaining_methods.is_empty() {
                    self.methods = remaining_methods;
                }
                false
            }
        }
    }
}

fn lost(e: impl std::fmt::Display) -> OpenError {
    OpenError::Refused(format!(
        "the SSH connection was lost while logging in ({e})"
    ))
}

/// The hash for an RSA key's signatures (the best the server supports, never SHA-1).
async fn rsa_hash(h: &Handle<Client>, key: &PublicKey) -> Option<HashAlg> {
    if !key.algorithm().is_rsa() {
        return None;
    }
    match h.best_supported_rsa_hash().await {
        Ok(Some(Some(alg))) => Some(alg),
        _ => Some(HashAlg::Sha256),
    }
}

pub(super) async fn login(
    h: &mut Handle<Client>,
    ep: &Endpoint,
    cfg: &SftpConfig,
    read: Duration,
) -> Result<(), OpenError> {
    let user = match ep.user.clone().or_else(local_user) {
        Some(u) => u,
        None => {
            return Err(OpenError::Fatal(EngineError::Ssh(
                "SSH: no user name: give one in the URL (sftp://user@host/path)".into(),
            )));
        }
    };
    let mut a = Attempts {
        methods: MethodSet::from(&[MethodKind::PublicKey, MethodKind::Password][..]),
        tried: Vec::new(),
        encrypted: Vec::new(),
    };
    // `none` lists the methods the server accepts (and a server may need no more).
    let r = timed(read, h.authenticate_none(user.clone()))
        .await
        .map_err(OpenError::Fatal)?
        .map_err(lost)?;
    if a.result(r) {
        return Ok(());
    }
    let mut offered: Vec<PublicKey> = Vec::new();

    // Each agent that answers, until one of its keys gets in: one that is gone or has
    // no keys (a stale or empty forwarded agent) is passed over.
    let candidates = if a.allows(MethodKind::PublicKey) {
        agents(&cfg.agent, ep).await
    } else {
        Vec::new()
    };
    for (name, mut agent) in candidates {
        if !a.allows(MethodKind::PublicKey) {
            break;
        }
        if let Some(binding) = h.agent_session_binding() {
            // Older agents do not know the extension: fine.
            let _ = timed(read, agent.bind_session(binding, false)).await;
        }
        let ids = match timed(read, agent.request_identities()).await {
            Ok(Ok(ids)) => ids,
            _ => Vec::new(),
        };
        tracing::debug!(agent = %name, "ssh-agent with {} key(s)", ids.len());
        let keys: Vec<PublicKey> = ids
            .into_iter()
            .filter_map(|id| match id {
                AgentIdentity::PublicKey { key, .. } => Some(key),
                _ => None,
            })
            .filter(|k| !offered.iter().any(|o| o.key_data() == k.key_data()))
            .collect();
        if keys.is_empty() {
            continue;
        }
        a.tried.push(format!("ssh-agent ({} key(s))", keys.len()));
        for key in keys {
            if !a.allows(MethodKind::PublicKey) {
                break;
            }
            let hash = rsa_hash(h, &key).await;
            let r = timed(
                read,
                h.authenticate_publickey_with(user.clone(), key.clone(), hash, &mut agent),
            )
            .await
            .map_err(OpenError::Fatal)?;
            offered.push(key);
            match r {
                Ok(r) => {
                    if a.result(r) {
                        return Ok(());
                    }
                }
                // The agent would not sign (or is gone): try the next way in.
                Err(e) => tracing::debug!("the ssh-agent did not sign: {e}"),
            }
            if h.is_closed() {
                return Err(lost("closed"));
            }
        }
    }

    for path in &cfg.identity_files {
        if !a.allows(MethodKind::PublicKey) {
            break;
        }
        if !path.exists() {
            continue;
        }
        let key = match russh::keys::load_secret_key(path, None) {
            Ok(k) => k,
            Err(russh::keys::Error::KeyIsEncrypted) => {
                a.encrypted.push(path.clone());
                continue;
            }
            Err(e) => {
                tracing::info!(key = %path.display(), "cannot use this SSH key: {e}");
                continue;
            }
        };
        if offered
            .iter()
            .any(|o| o.key_data() == key.public_key().key_data())
        {
            continue;
        }
        a.tried.push(shown(path));
        let hash = rsa_hash(h, key.public_key()).await;
        let key = PrivateKeyWithHashAlg::new(Arc::new(key), hash);
        let r = timed(read, h.authenticate_publickey(user.clone(), key))
            .await
            .map_err(OpenError::Fatal)?
            .map_err(lost)?;
        if a.result(r) {
            return Ok(());
        }
    }

    if let Some(pass) = &ep.pass {
        if a.allows(MethodKind::Password) {
            a.tried.push("the URL's password".into());
            let r = timed(read, h.authenticate_password(user.clone(), pass.clone()))
                .await
                .map_err(OpenError::Fatal)?
                .map_err(lost)?;
            if a.result(r) {
                return Ok(());
            }
        }
        if a.allows(MethodKind::KeyboardInteractive) {
            if !a.tried.iter().any(|t| t.contains("password")) {
                a.tried
                    .push("the URL's password (keyboard-interactive)".into());
            }
            if keyboard_interactive(h, &user, pass, read, &mut a).await? {
                return Ok(());
            }
        }
    }

    if h.is_closed() {
        return Err(lost("closed"));
    }
    Err(OpenError::Fatal(EngineError::Ssh(failure_message(
        &user, ep, &a,
    ))))
}

/// `keyboard-interactive` with the password as the answer to a single hidden prompt
/// (an empty round is answered empty; anything else is not answered).
async fn keyboard_interactive(
    h: &mut Handle<Client>,
    user: &str,
    pass: &str,
    read: Duration,
    a: &mut Attempts,
) -> Result<bool, OpenError> {
    let mut r = timed(read, h.authenticate_keyboard_interactive_start(user, None))
        .await
        .map_err(OpenError::Fatal)?
        .map_err(lost)?;
    for _ in 0..4 {
        match r {
            KeyboardInteractiveAuthResponse::Success => return Ok(true),
            KeyboardInteractiveAuthResponse::Failure {
                remaining_methods, ..
            } => {
                if !remaining_methods.is_empty() {
                    a.methods = remaining_methods;
                }
                return Ok(false);
            }
            KeyboardInteractiveAuthResponse::InfoRequest { prompts, .. } => {
                let answers = match prompts.as_slice() {
                    [] => Vec::new(),
                    [p] if !p.echo => vec![pass.to_string()],
                    _ => return Ok(false),
                };
                r = timed(read, h.authenticate_keyboard_interactive_respond(answers))
                    .await
                    .map_err(OpenError::Fatal)?
                    .map_err(lost)?;
            }
        }
    }
    Ok(false)
}

fn failure_message(user: &str, ep: &Endpoint, a: &Attempts) -> String {
    let accepts: Vec<&str> = a.methods.iter().map(<&str>::from).collect();
    let tried = if a.tried.is_empty() {
        "no key or password it accepts".to_string()
    } else {
        a.tried.join(", ")
    };
    let mut msg = format!(
        "SSH: login as {user} to {}:{} failed (tried {tried}; the server accepts {})",
        ep.host,
        ep.port,
        accepts.join(", ")
    );
    if !a.encrypted.is_empty() {
        let keys: Vec<String> = a.encrypted.iter().map(|p| shown(p)).collect();
        msg.push_str(&format!(
            ". {} {} encrypted and puppy cannot ask for a passphrase: load it into \
             ssh-agent (`ssh-add {}`) and make sure the agent is reachable \
             (SSH_AUTH_SOCK)",
            keys.join(", "),
            if keys.len() == 1 { "is" } else { "are" },
            keys[0]
        ));
    }
    if ep.pass.is_none() && a.allows(MethodKind::Password) && a.encrypted.is_empty() {
        msg.push_str(". Use ssh-agent or a key file, or put a password in the URL");
    }
    msg
}
