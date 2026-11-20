//! `puppy`: the command-line entry point.
//!
//! - `puppy daemon [--idle-exit <SECS>]` runs the background daemon (started by a login
//!   service, or detached by a client with `--idle-exit`).
//! - `puppy` opens the full-screen dashboard (starting a daemon if none runs).
//! - `puppy status` prints a one-line summary (scriptable).

use anyhow::Context;
use clap::{Parser, Subcommand, ValueEnum};
use puppy_client::{Client, Paths};
use puppy_daemon::Config;
use puppy_daemon::lifecycle::{DaemonOptions, RunOutcome, TorrentTestOptions, run_daemon};
use puppy_proto::WireState;
use puppy_tui::api::ClientApi;
use puppy_tui::theme::{Theme, ThemeChoice};
use std::io::IsTerminal;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

/// How long background work may take to wind down once the command is done.
const RUNTIME_SHUTDOWN: Duration = Duration::from_secs(1);

#[derive(Parser, Debug)]
#[command(name = "puppy", version, about = "A modern download manager")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
    /// Color theme for the dashboard (overrides `theme` in config.toml).
    #[arg(long, value_enum, global = true, value_name = "THEME")]
    theme: Option<ThemeArg>,
}

#[derive(ValueEnum, Clone, Copy, Debug)]
enum ThemeArg {
    Auto,
    Dark,
    Light,
}

impl From<ThemeArg> for ThemeChoice {
    fn from(t: ThemeArg) -> Self {
        match t {
            ThemeArg::Auto => ThemeChoice::Auto,
            ThemeArg::Dark => ThemeChoice::Dark,
            ThemeArg::Light => ThemeChoice::Light,
        }
    }
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
    /// Print a one-line summary (`N downloads, X active`), starting a daemon if needed.
    Status,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    if cli.command.is_none() && !(std::io::stdin().is_terminal() && std::io::stdout().is_terminal())
    {
        eprintln!(
            "puppy's dashboard needs an interactive terminal (try `puppy daemon` to run the background service)"
        );
        return ExitCode::from(2);
    }
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
            Some(Command::Status) => summary().await,
            None => dashboard(cli.theme).await,
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
    // Undocumented, for puppy's own tests of the real binary: the torrent engine stays
    // off the network (no DHT, trackers, LSD or UPnP; listening on 127.0.0.1).
    let torrent_test = (std::env::var_os("PUPPY_TORRENT_TEST_MODE").as_deref()
        == Some("1".as_ref()))
    .then(TorrentTestOptions::default);
    let outcome = run_daemon(DaemonOptions {
        idle_exit: idle_exit.map(Duration::from_secs),
        torrent_test,
        ..DaemonOptions::default()
    })
    .await?;
    match outcome {
        RunOutcome::Exited => Ok(()),
        RunOutcome::AlreadyRunning => {
            eprintln!("puppy: a daemon is already running; nothing to do");
            Ok(())
        }
        RunOutcome::Forced => {
            anyhow::bail!("shutdown interrupted: some downloads may resume from an earlier point")
        }
    }
}

/// Placeholder for the dashboard: `N downloads, X active` (`1 download, …`).
async fn summary() -> anyhow::Result<()> {
    let paths = Paths::resolve().context("cannot set up puppy's directories")?;
    let exe = std::env::current_exe().context("cannot find the puppy executable")?;
    let client = Client::connect_or_spawn(&paths, "puppy", &exe, &[]).await?;
    let downloads = client.list().await?;
    let active = downloads
        .iter()
        .filter(|d| matches!(d.state, WireState::Starting | WireState::Active))
        .count();
    let n = downloads.len();
    let noun = if n == 1 { "download" } else { "downloads" };
    println!("{n} {noun}, {active} active");
    Ok(())
}

/// The full-screen dashboard (the default command).
async fn dashboard(theme: Option<ThemeArg>) -> anyhow::Result<()> {
    let paths = Paths::resolve().context("cannot set up puppy's directories")?;
    let config = Config::load(&paths.config_file())?;
    let choice = match theme {
        Some(t) => t.into(),
        None => config.theme.parse().unwrap_or_else(|e| {
            eprintln!("puppy: {e} in config.toml; using \"auto\"");
            ThemeChoice::Auto
        }),
    };
    let theme = Theme::detect(choice, &|k| std::env::var(k).ok());
    let exe = std::env::current_exe().context("cannot find the puppy executable")?;
    puppy_tui::runtime::run(Arc::new(ClientApi::new(paths, exe)), theme).await
}
