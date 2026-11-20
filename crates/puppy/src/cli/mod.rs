//! Scriptable subcommands: shared context, errors with their exit codes, and
//! pipe-safe output.

pub mod add;
pub mod list;
pub mod output;

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
// Not every kind is produced yet: the commands that do arrive in later tasks.
#[allow(dead_code)]
#[derive(Debug, thiserror::Error)]
pub enum CliError {
    /// Bad arguments (exit 2).
    #[error("{0}")]
    Usage(String),
    /// An id that does not exist (exit 3).
    #[error("{0}")]
    UnknownId(String),
    /// A waited-for download failed (exit 4).
    #[error("{0}")]
    Failed(String),
    /// A waited-for download needs attention (exit 5).
    #[error("{0}")]
    Attention(String),
    /// Anything else: daemon or operation errors (exit 1).
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

impl From<ClientError> for CliError {
    fn from(e: ClientError) -> Self {
        CliError::Other(e.into())
    }
}

impl CliError {
    pub fn exit_code(&self) -> u8 {
        match self {
            CliError::Other(_) => 1,
            CliError::Usage(_) => 2,
            CliError::UnknownId(_) => 3,
            CliError::Failed(_) => 4,
            CliError::Attention(_) => 5,
        }
    }
}

/// The exit code for several outcomes: the highest wins (0 for none).
#[allow(dead_code)] // used by the multi-id commands
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
        assert_eq!(CliError::Failed("x".into()).exit_code(), 4);
        assert_eq!(CliError::Attention("x".into()).exit_code(), 5);
        assert_eq!(CliError::Other(anyhow::anyhow!("x")).exit_code(), 1);
    }

    #[test]
    fn worst_is_max() {
        assert_eq!(worst([]), 0);
        assert_eq!(worst([0, 3, 1]), 3);
        assert_eq!(worst([4, 5, 2]), 5);
    }
}
