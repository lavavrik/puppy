//! Scriptable subcommands: shared context, errors with their exit codes, and
//! pipe-safe output.

pub mod add;
pub mod browser;
pub mod control;
pub mod files;
pub mod input_file;
pub mod list;
pub mod output;
pub mod seed;
pub mod update;
pub mod wait;

use anyhow::Context;
pub use puppy_client::ClientError;
use puppy_client::{Client, Paths};
use std::path::PathBuf;

/// What every command needs: where puppy lives, which executable to spawn as the
/// daemon, and whether to print JSON.
pub struct Ctx {
    pub paths: Paths,
    pub exe: PathBuf,
    pub json: bool,
}

impl Ctx {
    pub fn from_env(json: bool) -> anyhow::Result<Self> {
        let paths = Paths::resolve().context("cannot set up puppy's directories")?;
        let exe = std::env::current_exe().context("cannot find the puppy executable")?;
        Ok(Ctx { paths, exe, json })
    }

    /// Connects to the daemon, starting one if none runs.
    pub async fn client(&self) -> Result<Client, CliError> {
        Ok(Client::connect_or_spawn(&self.paths, "puppy-cli", &self.exe, &[]).await?)
    }
}

/// Why a command failed; each kind has its own exit code.
#[derive(Debug, thiserror::Error)]
pub enum CliError {
    /// Bad arguments (exit 2).
    #[error("{0}")]
    Usage(String),
    /// An id that does not exist (exit 3).
    #[error("{0}")]
    UnknownId(String),
    /// Already reported per item; only the exit code remains (e.g. `wait`: 4 when a
    /// download failed, 5 when one needs attention, 130 after Ctrl-C).
    #[error("")]
    Reported(u8),
    /// An update that cannot happen here (exit 6); the text is why.
    #[error("cannot update: {0}")]
    NotPossible(String),
    /// Anything else: daemon or operation errors (exit 1).
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

/// Whether `e` says the daemon speaks another protocol version.
pub fn is_mismatch(e: &ClientError) -> bool {
    matches!(
        e,
        ClientError::Daemon {
            code: puppy_proto::ErrorCode::ProtocolMismatch,
            ..
        }
    ) || matches!(e, ClientError::Protocol(m) if m.starts_with("unexpected hello reply"))
}

/// What to do about an incompatible daemon.
pub const MISMATCH_HINT: &str =
    "an incompatible puppy daemon is running \u{2014} run `puppy stop` first";

impl From<ClientError> for CliError {
    fn from(e: ClientError) -> Self {
        if is_mismatch(&e) {
            return CliError::Other(anyhow::anyhow!("{e}; {MISMATCH_HINT}"));
        }
        CliError::Other(e.into())
    }
}

impl CliError {
    pub fn exit_code(&self) -> u8 {
        match self {
            CliError::Other(_) => 1,
            CliError::Usage(_) => 2,
            CliError::UnknownId(_) => 3,
            CliError::NotPossible(_) => 6,
            CliError::Reported(c) => *c,
        }
    }
}

/// Prints `e` on stderr, unless it was already reported.
pub fn report_error(e: &CliError) {
    match e {
        CliError::Reported(_) => {}
        CliError::Other(e) => output::err(format_args!("puppy: {e:#}")),
        e => output::err(format_args!("puppy: {e}")),
    }
}

/// The exit code for several outcomes: the highest wins (0 for none).
pub fn worst(codes: impl IntoIterator<Item = u8>) -> u8 {
    codes.into_iter().max().unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_codes() {
        assert_eq!(CliError::Usage("x".into()).exit_code(), 2);
        assert_eq!(CliError::UnknownId("x".into()).exit_code(), 3);
        assert_eq!(CliError::NotPossible("x".into()).exit_code(), 6);
        assert_eq!(CliError::Reported(5).exit_code(), 5);
        assert_eq!(CliError::Other(anyhow::anyhow!("x")).exit_code(), 1);
    }

    #[test]
    fn worst_is_max() {
        assert_eq!(worst([]), 0);
        assert_eq!(worst([0, 3, 1]), 3);
        assert_eq!(worst([4, 5, 2]), 5);
    }
}
