//! `puppy`: the command-line entry point.
//!
//! - `puppy daemon [--idle-exit <SECS>]` runs the background daemon (started by a login
//!   service, or detached by a client with `--idle-exit`).
//! - `puppy` connects to the daemon (starting one if none runs) and, until the dashboard
//!   lands, prints a one-line summary.

use anyhow::Context;
use clap::{Parser, Subcommand};
use puppy_client::{Client, Paths};
use puppy_core::types::DownloadState;
use puppy_daemon::lifecycle::{DaemonOptions, RunOutcome, run_daemon};
use std::process::ExitCode;
use std::time::Duration;

/// How long background work may take to wind down once the command is done.
const RUNTIME_SHUTDOWN: Duration = Duration::from_secs(1);

#[derive(Parser, Debug)]
#[command(name = "puppy", version, about = "A modern download manager")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Run the background daemon that owns all downloads (normally started by a
    /// login service, or automatically by `puppy`).
    Daemon {
        /// Exit after this many seconds without queued or active downloads and
        /// without connected clients. Without it the daemon never idle-exits.
        #[arg(long, value_name = "SECS")]
        idle_exit: Option<u64>,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let rt = match tokio::runtime::Runtime::new() {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("puppy: cannot start the async runtime: {e}");
            return ExitCode::FAILURE;
        }
    };
    let result = rt.block_on(async {
        match cli.command {
            Some(Command::Daemon { idle_exit }) => daemon(idle_exit).await,
            None => summary().await,
        }
    });
    rt.shutdown_timeout(RUNTIME_SHUTDOWN);
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("puppy: {e:#}");
            ExitCode::FAILURE
        }
    }
}

async fn daemon(idle_exit: Option<u64>) -> anyhow::Result<()> {
    let outcome = run_daemon(DaemonOptions {
        idle_exit: idle_exit.map(Duration::from_secs),
        ..DaemonOptions::default()
    })
    .await?;
    if outcome == RunOutcome::AlreadyRunning {
        eprintln!("puppy: a daemon is already running; nothing to do");
    }
    Ok(())
}

/// Placeholder for the dashboard: `N downloads, X active` (`1 download, …`).
async fn summary() -> anyhow::Result<()> {
    let paths = Paths::resolve().context("cannot set up puppy's directories")?;
    let exe = std::env::current_exe().context("cannot find the puppy executable")?;
    let client = Client::connect_or_spawn(&paths, "puppy", &exe, &[]).await?;
    let downloads = client.list().await?;
    let active = downloads
        .iter()
        .filter(|d| matches!(d.state, DownloadState::Starting | DownloadState::Active))
        .count();
    let n = downloads.len();
    let noun = if n == 1 { "download" } else { "downloads" };
    println!("{n} {noun}, {active} active");
    Ok(())
}
