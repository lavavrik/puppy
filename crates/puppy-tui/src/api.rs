//! The dashboard's view of the daemon: [`DaemonApi`] (faked in tests) and [`ClientApi`],
//! the real one over [`puppy_client::Client`].

use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use async_trait::async_trait;
use puppy_client::{Client, ClientError, Paths};
use puppy_proto::{DownloadSnapshot, ErrorCode, Event, ReplyData, Request, Settings, UpdateStatus};
use tokio::sync::{Mutex, mpsc};

use crate::app::{Cmd, CmdError, Msg};

/// Snapshot events per second the dashboard asks for (every 250 ms).
pub const SUBSCRIBE_INTERVAL_MS: u64 = 250;
/// The name the dashboard gives in `hello`.
pub const CLIENT_NAME: &str = "puppy-tui";

/// What a successful connect brings: the daemon's version, its settings and the list.
pub type Connected = (String, Settings, Vec<DownloadSnapshot>);

/// Why connecting failed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ApiError {
    /// The daemon speaks another protocol version (e.g. left running by an older
    /// `puppy`): it must be restarted ([`Cmd::RestartDaemon`]).
    Incompatible(String),
    /// No daemon answered, even after starting one: a hint naming the daemon's log.
    NoDaemon(String),
    Other(String),
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ApiError::Incompatible(s) | ApiError::NoDaemon(s) | ApiError::Other(s) => {
                f.write_str(s)
            }
        }
    }
}

impl std::error::Error for ApiError {}

#[async_trait]
pub trait DaemonApi: Send + Sync + 'static {
    /// (Re)connects: handshake, settings, the list, and a subscription to events every
    /// [`SUBSCRIBE_INTERVAL_MS`].
    async fn connect(&self) -> Result<Connected, ApiError>;
    /// The next event of the current connection; `None` once it is closed.
    async fn next_event(&self) -> Option<Event>;
    /// Runs a command: [`Cmd::FetchDetail`] answers `Some(Msg::Detail)`,
    /// [`Cmd::FetchFiles`] `Some(Msg::Files)`, the others `None`. Never called with [`Cmd::Quit`].
    async fn exec(&self, cmd: Cmd) -> Result<Option<Msg>, CmdError>;
}

/// [`DaemonApi`] over a [`Client`], starting the daemon (`exe daemon …`) when none runs.
pub struct ClientApi {
    paths: Paths,
    exe: PathBuf,
    /// The connection, shared by concurrent calls.
    client: RwLock<Option<Arc<Client>>>,
    /// Its events, apart from the client so waiting for one never blocks a call.
    events: Mutex<Option<mpsc::Receiver<Event>>>,
}

impl ClientApi {
    pub fn new(paths: Paths, exe: PathBuf) -> Self {
        Self {
            paths,
            exe,
            client: RwLock::new(None),
            events: Mutex::new(None),
        }
    }

    fn client(&self) -> Result<Arc<Client>, String> {
        self.client
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
            .ok_or_else(|| "not connected to the daemon".to_string())
    }

    fn set_client(&self, client: Option<Arc<Client>>) {
        *self.client.write().unwrap_or_else(|e| e.into_inner()) = client;
    }
}

/// How a failed handshake is reported.
fn connect_error(e: ClientError) -> ApiError {
    match e {
        ClientError::Daemon {
            code: ErrorCode::ProtocolMismatch,
            message,
        } => ApiError::Incompatible(message),
        // The hello reply itself was not understood: another protocol.
        ClientError::Protocol(m) => ApiError::Incompatible(m),
        ClientError::NoDaemon { log_file, .. } => ApiError::NoDaemon(format!(
            "no daemon answered after starting one — see its log: {}",
            log_file.display()
        )),
        e => ApiError::Other(e.to_string()),
    }
}

fn unexpected(what: &str, data: ReplyData) -> ApiError {
    ApiError::Other(format!("unexpected reply to {what}: {data:?}"))
}

#[async_trait]
impl DaemonApi for ClientApi {
    async fn connect(&self) -> Result<Connected, ApiError> {
        // Drop the old connection (if any) first: its calls fail, its events end.
        self.set_client(None);
        *self.events.lock().await = None;

        let mut client = Client::connect_or_spawn(&self.paths, CLIENT_NAME, &self.exe, &[])
            .await
            .map_err(connect_error)?;
        let events = client.take_events();
        let other = |e: ClientError| ApiError::Other(e.to_string());
        let settings = match client.call(Request::GetSettings).await.map_err(other)? {
            ReplyData::Settings { settings } => settings,
            d => return Err(unexpected("get_settings", d)),
        };
        let downloads = client.list().await.map_err(other)?;
        client
            .subscribe(SUBSCRIBE_INTERVAL_MS)
            .await
            .map_err(other)?;
        let version = client.daemon_version().to_string();
        *self.events.lock().await = events;
        self.set_client(Some(Arc::new(client)));
        Ok((version, settings, downloads))
    }

    async fn next_event(&self) -> Option<Event> {
        self.events.lock().await.as_mut()?.recv().await
    }

    async fn exec(&self, cmd: Cmd) -> Result<Option<Msg>, CmdError> {
        if cmd == Cmd::RestartDaemon {
            self.set_client(None);
            return Client::shutdown_incompatible(&self.paths)
                .await
                .map(|()| None)
                .map_err(|e| CmdError::Failed(e.to_string()));
        }
        let client = self.client()?;
        let request = match cmd {
            Cmd::Add {
                request,
                start_paused,
            } => Request::Add {
                request,
                start_paused,
                limit: None,
            },
            Cmd::Pause(id) => Request::Pause { id },
            Cmd::Resume(id) => Request::Resume { id },
            Cmd::Remove { id, delete_files } => Request::Remove { id, delete_files },
            Cmd::Restart(id) => Request::Restart { id },
            Cmd::Move { id, position } => Request::Move { id, position },
            Cmd::SetLimit { id, bps } => Request::SetLimit { id, bps },
            Cmd::SetGlobalLimits { down, up } => Request::SetGlobalLimits { down, up },
            Cmd::SetMaxActive(n) => Request::SetMaxActive { n },
            Cmd::SelectFiles { id, selection } => Request::SelectFiles { id, selection },
            Cmd::SetSeed { id, policy } => Request::SetSeed { id, policy },
            Cmd::FetchFiles(id) => {
                return match client.call(Request::ListFiles { id }).await {
                    Ok(ReplyData::Files { files }) => Ok(Some(Msg::Files(id, files))),
                    Ok(d) => Err(CmdError::Failed(format!("unexpected reply: {d:?}"))),
                    Err(e) => Err(call_error(e)),
                };
            }
            Cmd::FetchDetail(id) => {
                return match client.call(Request::Get { id }).await {
                    Ok(ReplyData::Download { detail, .. }) => Ok(Some(Msg::Detail(id, detail))),
                    Ok(d) => Err(CmdError::Failed(format!("unexpected reply: {d:?}"))),
                    Err(e) => Err(call_error(e)),
                };
            }
            Cmd::FetchSettings => {
                return match client.call(Request::GetSettings).await {
                    Ok(ReplyData::Settings { settings }) => Ok(Some(Msg::Settings(settings))),
                    Ok(d) => Err(CmdError::Failed(format!("unexpected reply: {d:?}"))),
                    Err(e) => Err(call_error(e)),
                };
            }
            Cmd::InstallUpdate => {
                return match client.call(Request::Update { check_only: false }).await {
                    Ok(ReplyData::Update {
                        status: status @ UpdateStatus::Installed { .. },
                    }) => Ok(Some(Msg::UpdateStatus(status))),
                    Ok(ReplyData::Update { .. }) => {
                        Err(CmdError::Failed("puppy is already up to date".into()))
                    }
                    Ok(d) => Err(CmdError::Failed(format!("unexpected reply: {d:?}"))),
                    Err(e) => Err(call_error(e)),
                };
            }
            Cmd::RestartDaemon | Cmd::Reexec | Cmd::Quit => return Ok(None),
        };
        client.call(request).await.map(|_| None).map_err(call_error)
    }
}

/// A failed call, for a toast: the daemon's own message when it refused.
fn call_error(e: ClientError) -> CmdError {
    match e {
        ClientError::Daemon {
            code: ErrorCode::Unsupported,
            message,
        } => {
            // The daemon's text repeats what the toast already says.
            let reason = message.strip_prefix("unsupported: ").unwrap_or(&message);
            CmdError::Unsupported(reason.to_string())
        }
        ClientError::Daemon { message, .. } if !message.is_empty() => CmdError::Failed(message),
        e => CmdError::Failed(e.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unsupported_refusals_are_their_own_error() {
        let e = call_error(ClientError::Daemon {
            code: ErrorCode::Unsupported,
            message: "unsupported: per-torrent limits while running".into(),
        });
        assert_eq!(
            e,
            CmdError::Unsupported("per-torrent limits while running".into())
        );
        let e = call_error(ClientError::Daemon {
            code: ErrorCode::InvalidState,
            message: "nope".into(),
        });
        assert_eq!(e, CmdError::Failed("nope".into()));
    }
}
