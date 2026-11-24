//! `puppy`: the command-line entry point.
//!
//! - `puppy daemon [--idle-exit <SECS>]` runs the background daemon (started by a login
//!   service, or detached by a client with `--idle-exit`).
//! - `puppy` opens the full-screen dashboard (starting a daemon if none runs).
//! - `puppy status`, `add`, `ls`, `info`, `pause`, ..., `wait`, `watch`: scriptable
//!   commands (see [`cli`] and the README's "Command line" section).
//! - `puppy native-host` (hidden) is the native messaging host the browser extension
//!   starts (see [`native_host`]).
//! - `--json` (global) makes commands print JSON; errors map to exit codes (see
//!   [`cli::CliError`]).

mod browser;
mod cli;
mod native_host;

use anyhow::Context;
use browser::Browser;
use clap::{Parser, Subcommand, ValueEnum};
use cli::add::AddArgs;
use cli::list::StateFilter;
use cli::output::err;
use cli::{CliError, Ctx};
use puppy_client::Paths;
use puppy_daemon::Config;
use puppy_daemon::lifecycle::{DaemonOptions, RunOutcome, TorrentTestOptions, run_daemon};
use puppy_daemon::notify::{DesktopNotifier, Notifier, desktop_wanted};
use puppy_tui::api::ClientApi;
use puppy_tui::theme::{Theme, ThemeChoice};
use std::io::IsTerminal;
use std::path::PathBuf;
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
    /// Print machine-readable JSON instead of text.
    #[arg(long, global = true)]
    json: bool,
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
    /// Add downloads from URLs, magnet links or .torrent files (`-` reads them from stdin).
    Add {
        /// URLs, magnet links, .torrent paths; `-` reads one per line from stdin.
        #[arg(required = true, value_name = "SOURCE")]
        sources: Vec<String>,
        /// Save into this folder (default: the daemon's download folder).
        #[arg(short = 'o', long, value_name = "DIR")]
        output_dir: Option<PathBuf>,
        /// Save under this file name (one source only).
        #[arg(long, value_name = "NAME")]
        name: Option<String>,
        /// Add without starting.
        #[arg(long)]
        paused: bool,
        /// Speed limit, e.g. `500K`, `2M`; `0` for none.
        #[arg(long, value_name = "RATE")]
        limit: Option<String>,
        /// Extra request header, `"Name: value"` (repeatable).
        #[arg(long = "header", short = 'H', value_name = "HEADER")]
        headers: Vec<String>,
        /// Cookie header value, e.g. `a=b; c=d`.
        #[arg(long, value_name = "COOKIE")]
        cookie: Option<String>,
        /// Referer header value.
        #[arg(long, value_name = "URL")]
        referer: Option<String>,
        /// Wait until the downloads end (exit code as for `puppy wait`).
        #[arg(long)]
        wait: bool,
        /// Torrent: download only these files, by number as `puppy files` shows
        /// them (`1,3-5`) and/or glob (`*.mkv`); repeatable. Every comma splits
        /// specs: use numbers for brace globs or names containing a comma.
        #[arg(long = "files", value_name = "SPEC")]
        files: Vec<String>,
        /// Torrent: wait for you to pick the files with `puppy select` (with
        /// `--wait`, the wait ends at once with exit 5).
        #[arg(long)]
        choose: bool,
        /// Torrent: stop seeding at this upload ratio (`0` never, `default`: the
        /// daemon's `seed_ratio`).
        #[arg(long, value_name = "RATIO")]
        seed_ratio: Option<String>,
        /// Torrent: stop seeding after this long (`90m`, `2h`; `0` never,
        /// `default`: the daemon's `seed_time`).
        #[arg(long, value_name = "DURATION")]
        seed_time: Option<String>,
    },
    /// List downloads.
    Ls {
        /// Only these states (comma-separated or repeated).
        #[arg(
            long,
            short = 's',
            value_enum,
            value_delimiter = ',',
            value_name = "STATE"
        )]
        state: Vec<StateFilter>,
    },
    /// List a torrent's files, numbered from 1.
    Files {
        /// The torrent's id (see `puppy ls`).
        id: String,
    },
    /// Choose which files of a torrent to download (`puppy add --choose` waits for this).
    Select {
        /// The torrent's id.
        id: String,
        /// File numbers or ranges as `puppy files` shows them (`1,3-5`), and/or
        /// globs (`*.mkv`). Every comma splits specs: use numbers for brace globs
        /// or names containing a comma.
        #[arg(required = true, value_name = "SPEC")]
        specs: Vec<String>,
    },
    /// Set when a torrent stops seeding. Replaces its whole policy: what you leave
    /// out goes back to the daemon's `seed_ratio` / `seed_time`.
    Seed {
        /// The torrent's id.
        id: String,
        /// Stop at this upload ratio (`0` never, `default`: the daemon's setting).
        #[arg(long, value_name = "RATIO")]
        ratio: Option<String>,
        /// Stop after seeding this long (`90m`, `2h`; `0` never, `default`).
        #[arg(long, value_name = "DURATION")]
        time: Option<String>,
    },
    /// Show everything about one download.
    Info {
        /// The download's id (see `puppy ls`).
        id: String,
    },
    /// Pause downloads (ids, or `all`).
    Pause {
        #[arg(required = true, value_name = "ID")]
        ids: Vec<String>,
    },
    /// Resume paused or failed downloads (ids, or `all`).
    Resume {
        #[arg(required = true, value_name = "ID")]
        ids: Vec<String>,
    },
    /// Restart failed or attention-needing downloads from scratch (ids, or `all`).
    Restart {
        #[arg(required = true, value_name = "ID")]
        ids: Vec<String>,
    },
    /// Remove downloads.
    Rm {
        #[arg(required = true, value_name = "ID")]
        ids: Vec<String>,
        /// Also delete the downloaded files.
        #[arg(long)]
        delete_files: bool,
    },
    /// Move a queued download to a queue position (0 is first).
    Move { id: String, position: usize },
    /// Limit one download's speed, or the global speed with `--global`.
    Limit {
        /// The download's id.
        id: Option<String>,
        /// Speed limit, e.g. `500K`, `2M`; `0` or `none` for no limit.
        rate: Option<String>,
        /// Set (or, with neither --down nor --up, show) the global limits.
        #[arg(long)]
        global: bool,
        /// Global download limit.
        #[arg(long, value_name = "RATE")]
        down: Option<String>,
        /// Global upload limit.
        #[arg(long, value_name = "RATE")]
        up: Option<String>,
    },
    /// Set how many downloads run at once.
    MaxActive { n: u16 },
    /// Wait until downloads complete, fail or need attention (exit 0/4/5; 3 if gone).
    Wait {
        #[arg(required = true, value_name = "ID")]
        ids: Vec<String>,
    },
    /// Print events as they happen until Ctrl-C (`--json`: one JSON event per line).
    Watch,
    /// Stop the background daemon.
    Stop,
    /// Set up browser download pickup (the puppy extension).
    Browser {
        #[command(subcommand)]
        action: BrowserAction,
    },
    /// Native messaging host started by the browser extension.
    #[command(hide = true)]
    NativeHost {
        /// What the browser passes: the extension's origin (Linux, macOS) or
        /// `--parent-window=` (Windows); ignored.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        browser_args: Vec<String>,
    },
}

#[derive(Subcommand, Debug)]
enum BrowserAction {
    /// Register puppy's native messaging host with Chrome-family browsers.
    Install {
        /// Only this browser.
        #[arg(long, value_enum)]
        browser: Option<Browser>,
        /// With --browser: register even if that browser is not found (creating its
        /// config dir).
        #[arg(long, requires = "browser")]
        force: bool,
    },
    /// Remove the registration.
    Uninstall {
        /// Only this browser.
        #[arg(long, value_enum)]
        browser: Option<Browser>,
    },
    /// Show what is registered, per browser.
    Status,
}

/// `all` (any case) alone means every download; otherwise the ids as given.
fn targets(ids: Vec<String>) -> cli::control::Targets {
    if ids.len() == 1 && ids[0].eq_ignore_ascii_case("all") {
        cli::control::Targets::All
    } else {
        cli::control::Targets::Ids(ids)
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    if cli.command.is_none() && !(std::io::stdin().is_terminal() && std::io::stdout().is_terminal())
    {
        err(
            "puppy's dashboard needs an interactive terminal (try `puppy daemon` to run the background service)",
        );
        return ExitCode::from(2);
    }
    if matches!(cli.command, Some(Command::Daemon { .. })) {
        bundled_roots_for_daemon();
    }
    let rt = match tokio::runtime::Runtime::new() {
        Ok(rt) => rt,
        Err(e) => {
            err(format_args!("puppy: cannot start the async runtime: {e}"));
            return ExitCode::FAILURE;
        }
    };
    let result = rt.block_on(async {
        match cli.command {
            Some(Command::Daemon { idle_exit }) => daemon(idle_exit).await.map_err(Into::into),
            Some(Command::Status) => cli::list::status(&Ctx::from_env(cli.json)?).await,
            Some(Command::Add {
                sources,
                output_dir,
                name,
                paused,
                limit,
                headers,
                cookie,
                referer,
                wait,
                files,
                choose,
                seed_ratio,
                seed_time,
            }) => {
                cli::add::run(
                    &Ctx::from_env(cli.json)?,
                    AddArgs {
                        sources,
                        output_dir,
                        name,
                        paused,
                        limit,
                        headers,
                        cookie,
                        referer,
                        files,
                        choose,
                        seed_ratio,
                        seed_time,
                    },
                    wait,
                )
                .await
            }
            Some(Command::Ls { state }) => cli::list::ls(&Ctx::from_env(cli.json)?, state).await,
            Some(Command::Info { id }) => cli::list::info(&Ctx::from_env(cli.json)?, &id).await,
            Some(Command::Files { id }) => cli::files::files(&Ctx::from_env(cli.json)?, &id).await,
            Some(Command::Select { id, specs }) => {
                cli::files::select(&Ctx::from_env(cli.json)?, &id, &specs).await
            }
            Some(Command::Seed { id, ratio, time }) => {
                cli::seed::seed(
                    &Ctx::from_env(cli.json)?,
                    &id,
                    ratio.as_deref(),
                    time.as_deref(),
                )
                .await
            }
            Some(Command::Pause { ids }) => act(cli.json, cli::control::Action::Pause, ids).await,
            Some(Command::Resume { ids }) => act(cli.json, cli::control::Action::Resume, ids).await,
            Some(Command::Restart { ids }) => {
                act(cli.json, cli::control::Action::Restart, ids).await
            }
            Some(Command::Rm { ids, delete_files }) => {
                cli::control::rm(&Ctx::from_env(cli.json)?, ids, delete_files).await
            }
            Some(Command::Move { id, position }) => {
                cli::control::move_to(&Ctx::from_env(cli.json)?, id, position).await
            }
            Some(Command::Limit {
                id,
                rate,
                global,
                down,
                up,
            }) => {
                cli::control::limit(
                    &Ctx::from_env(cli.json)?,
                    cli::control::LimitArgs {
                        id,
                        rate,
                        global,
                        down,
                        up,
                    },
                )
                .await
            }
            Some(Command::MaxActive { n }) => {
                cli::control::max_active(&Ctx::from_env(cli.json)?, n).await
            }
            Some(Command::Wait { ids }) => cli::wait::wait(&Ctx::from_env(cli.json)?, ids).await,
            Some(Command::Watch) => cli::wait::watch(&Ctx::from_env(cli.json)?).await,
            Some(Command::Stop) => cli::control::stop(&Ctx::from_env(cli.json)?).await,
            Some(Command::Browser { action }) => match action {
                BrowserAction::Install { browser, force } => {
                    cli::browser::install(cli.json, browser, force).await
                }
                BrowserAction::Uninstall { browser } => {
                    cli::browser::uninstall(cli.json, browser).await
                }
                BrowserAction::Status => cli::browser::status(cli.json).await,
            },
            Some(Command::NativeHost { browser_args: _ }) => native_host::run().await,
            None => dashboard(cli.theme).await.map_err(Into::into),
        }
    });
    rt.shutdown_timeout(RUNTIME_SHUTDOWN);
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            cli::report_error(&e);
            ExitCode::from(e.exit_code())
        }
    }
}

async fn act(json: bool, action: cli::control::Action, ids: Vec<String>) -> Result<(), CliError> {
    cli::control::act(&Ctx::from_env(json)?, action, targets(ids)).await
}

/// The daemon on a system without CA certificates (a minimal container): trust
/// puppy's bundled roots instead of failing to build any HTTPS client. Runs before the
/// async runtime exists, while this is the process's only thread.
fn bundled_roots_for_daemon() {
    // A failure to resolve the paths is reported by the daemon itself.
    let Ok(paths) = Paths::resolve() else { return };
    // SAFETY: no other thread has been started yet.
    if let Err(e) = unsafe { puppy_core::tls::use_bundled_roots_if_missing(&paths.data_dir) } {
        err(format_args!(
            "puppy: cannot write the bundled CA certificates: {e}"
        ));
    }
}

async fn daemon(idle_exit: Option<u64>) -> anyhow::Result<()> {
    // Undocumented, for puppy's own tests of the real binary: the torrent engine stays
    // off the network (no DHT, trackers, LSD or UPnP; listening on 127.0.0.1).
    // `PUPPY_TORRENT_TEST_PEERS` (comma-separated `ip:port`) are handed to every torrent.
    let torrent_test = (std::env::var_os("PUPPY_TORRENT_TEST_MODE").as_deref()
        == Some("1".as_ref()))
    .then(|| TorrentTestOptions {
        initial_peers: std::env::var("PUPPY_TORRENT_TEST_PEERS")
            .unwrap_or_default()
            .split(',')
            .filter(|a| !a.trim().is_empty())
            .filter_map(|a| {
                let r = a.trim().parse().ok();
                if r.is_none() {
                    err(format_args!(
                        "puppy: ignoring bad PUPPY_TORRENT_TEST_PEERS entry {a:?}"
                    ));
                }
                r
            })
            .collect(),
        ..TorrentTestOptions::default()
    });
    let outcome = run_daemon(DaemonOptions {
        idle_exit: idle_exit.map(Duration::from_secs),
        torrent_test,
        notifier: desktop_wanted(|k| std::env::var(k).ok())
            .then(|| Arc::new(DesktopNotifier::new()) as Arc<dyn Notifier>),
        ..DaemonOptions::default()
    })
    .await?;
    match outcome {
        // Task 6 starts the new binary on `Restart`; until then it just exits.
        RunOutcome::Exited | RunOutcome::Restart => Ok(()),
        RunOutcome::AlreadyRunning => {
            err("puppy: a daemon is already running; nothing to do");
            Ok(())
        }
        RunOutcome::Forced => {
            anyhow::bail!("shutdown interrupted: some downloads may resume from an earlier point")
        }
    }
}

/// The full-screen dashboard (the default command).
async fn dashboard(theme: Option<ThemeArg>) -> anyhow::Result<()> {
    let paths = Paths::resolve().context("cannot set up puppy's directories")?;
    let config = Config::load(&paths.config_file())?;
    let choice = match theme {
        Some(t) => t.into(),
        None => config.theme.parse().unwrap_or_else(|e| {
            err(format_args!("puppy: {e} in config.toml; using \"auto\""));
            ThemeChoice::Auto
        }),
    };
    let theme = Theme::detect(choice, &|k| std::env::var(k).ok());
    let exe = std::env::current_exe().context("cannot find the puppy executable")?;
    puppy_tui::runtime::run(Arc::new(ClientApi::new(paths, exe)), theme).await
}
