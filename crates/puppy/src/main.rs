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
use puppy_daemon::update::UpdateOptions;
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
#[allow(clippy::large_enum_variant)] // parsed once per run; boxing buys nothing
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
    /// Look for a newer puppy and install it.
    Update {
        /// Only say whether a newer version exists.
        #[arg(long)]
        check: bool,
    },
    /// Add downloads from URLs, magnet links, .torrent files or metalinks (`-` reads them
    /// from stdin, `-i` from an aria2-style input file).
    Add {
        /// URLs, magnet links, .torrent or .meta4/.metalink paths or URLs; `-` reads one
        /// per line from stdin.
        #[arg(required_unless_present = "input_file", value_name = "SOURCE")]
        sources: Vec<String>,
        /// Read downloads from an aria2-style input file (`-`: stdin): one URI per
        /// line, each followed by indented `key=value` options (`dir`, `out`,
        /// `checksum`, `header`, ...). The other flags are defaults the options
        /// override.
        #[arg(short = 'i', long, value_name = "FILE")]
        input_file: Option<PathBuf>,
        /// Save into this folder (default: the daemon's download folder).
        #[arg(short = 'o', long, value_name = "DIR")]
        output_dir: Option<PathBuf>,
        /// Save under this file name (one source only; a metalink of one file).
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
        /// Check the finished file against this digest, `ALGO=HEX` (`sha-256`,
        /// `sha-512`, `sha-1`, `md5`, ...); one HTTP(S), FTP(S) or SFTP source only.
        #[arg(long, value_name = "ALGO=HEX")]
        checksum: Option<String>,
        /// Another URL of the same file, HTTP(S), FTP(S) or SFTP (repeatable; one URL source only):
        /// connections are spread over the URLs that agree on the file, and fail over
        /// between them. Cookies and headers go to the source's origin only.
        #[arg(long = "mirror", value_name = "URL")]
        mirrors: Vec<String>,
        /// Read every source as a metalink, a file or an HTTP(S) URL, whatever its name
        /// (asking the server for one: MirrorBrain answers a file's URL with its metalink).
        /// Without it, .meta4/.metalink files and URLs and files that look like one are.
        #[arg(long, conflicts_with = "no_metalink")]
        metalink: bool,
        /// Download .meta4/.metalink sources as files instead of reading them.
        #[arg(long)]
        no_metalink: bool,
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
    /// SFTP host keys.
    Ssh {
        #[command(subcommand)]
        action: SshAction,
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
enum SshAction {
    /// Fetch a server's host key, show its fingerprint and, once confirmed, add it to
    /// puppy's own known_hosts, so that SFTP downloads from it work.
    Trust {
        /// HOST, HOST:PORT, [IPv6]:PORT or an sftp:// URL (port 22 by default).
        #[arg(value_name = "HOST[:PORT]")]
        host: String,
        /// Trust it without asking (check the fingerprint first: it is printed).
        #[arg(long, short = 'y')]
        yes: bool,
        /// Trust a key of a type not known for a host that has keys of other types
        /// (refused otherwise: that is what an attack looks like).
        #[arg(long)]
        replace_type: bool,
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
    // Looked up now: once an update replaces the file, Linux reports this process's
    // executable as "<path> (deleted)".
    let own_exe = std::env::current_exe().and_then(|e| e.canonicalize());
    let mut restart = false;
    let result = rt.block_on(async {
        match cli.command {
            Some(Command::Daemon { idle_exit }) => match daemon(idle_exit).await {
                Ok(DaemonEnd::Done) => Ok(()),
                Ok(DaemonEnd::Restart) => {
                    restart = true;
                    Ok(())
                }
                Err(e) => Err(e.into()),
            },
            Some(Command::Status) => cli::list::status(&Ctx::from_env(cli.json)?).await,
            Some(Command::Update { check }) => {
                cli::update::update(&Ctx::from_env(cli.json)?, check).await
            }
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
                checksum,
                files,
                choose,
                seed_ratio,
                seed_time,
                input_file,
                mirrors,
                metalink,
                no_metalink,
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
                        checksum,
                        files,
                        choose,
                        seed_ratio,
                        seed_time,
                        input_file,
                        mirrors,
                        metalink,
                        no_metalink,
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
            Some(Command::Ssh {
                action:
                    SshAction::Trust {
                        host,
                        yes,
                        replace_type,
                    },
            }) => cli::ssh::trust(&Ctx::from_env(cli.json)?, &host, yes, replace_type).await,
            Some(Command::NativeHost { browser_args: _ }) => native_host::run().await,
            None => dashboard(cli.theme).await.map_err(Into::into),
        }
    });
    rt.shutdown_timeout(RUNTIME_SHUTDOWN);
    if restart {
        // The daemon has shut down and released its lock and the runtime is gone.
        return restart_into_update(own_exe);
    }
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            cli::report_error(&e);
            ExitCode::from(e.exit_code())
        }
    }
}

/// Starts the freshly installed binary in place of this daemon, with the same arguments.
/// On Unix `exec` keeps the process (and its pid); it only returns on failure.
#[cfg(unix)]
fn restart_into_update(exe: std::io::Result<PathBuf>) -> ExitCode {
    use std::os::unix::process::CommandExt;
    let e = match exe {
        Ok(exe) => std::process::Command::new(exe)
            .args(std::env::args_os().skip(1))
            .exec(),
        Err(e) => e,
    };
    // The daemon's log is still open: leave a trace where a service's stderr is lost.
    tracing::error!("cannot start the updated puppy: {e}");
    err(format_args!("puppy: cannot start the updated puppy: {e}"));
    ExitCode::FAILURE
}

/// Windows cannot replace a process image: start the new binary detached, which waits
/// for this daemon's lock (`PUPPY_UPDATE_HANDOFF`), and exit.
#[cfg(windows)]
fn restart_into_update(exe: std::io::Result<PathBuf>) -> ExitCode {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    let started = exe.and_then(|exe| {
        puppy_client::respawn_detached(&exe, &args, &[("PUPPY_UPDATE_HANDOFF", "1")])
    });
    match started {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            tracing::error!("cannot start the updated puppy: {e}");
            err(format_args!("puppy: cannot start the updated puppy: {e}"));
            ExitCode::FAILURE
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

/// How the daemon ended, when it did not fail.
enum DaemonEnd {
    Done,
    /// An update was installed: start the new binary.
    Restart,
}

/// Self-update settings for the real daemon; `Err(why)` (updates off) if the
/// executable's own path cannot be found or `PUPPY_UPDATE_URL` is bad. Runs before the
/// daemon's logging starts: the reason is printed here and handed to the daemon to log.
fn update_options() -> Result<UpdateOptions, String> {
    let off = |why: String| {
        err(format_args!("puppy: updates are off, {why}"));
        Err(why)
    };
    let exe = match std::env::current_exe().and_then(|e| e.canonicalize()) {
        Ok(exe) => exe,
        Err(e) => return off(format!("cannot find puppy's own executable: {e}")),
    };
    let api_base = std::env::var("PUPPY_UPDATE_URL")
        .ok()
        .filter(|u| !u.trim().is_empty())
        .unwrap_or_else(|| "https://api.github.com".to_string());
    let api_base = match api_base.parse() {
        Ok(url) => url,
        Err(e) => return off(format!("bad PUPPY_UPDATE_URL {api_base:?}: {e}")),
    };
    Ok(UpdateOptions {
        api_base,
        exe,
        // Undocumented, for puppy's own tests (debug builds): install although this is
        // not a release build. An optimized build never honours it.
        allow_dev: cfg!(debug_assertions)
            && std::env::var_os("PUPPY_UPDATE_ALLOW_DEV").as_deref() == Some("1".as_ref()),
        settle: Duration::from_secs(60),
        check_delay: Duration::from_secs(5),
        check_every: Duration::from_secs(24 * 60 * 60),
    })
}

async fn daemon(idle_exit: Option<u64>) -> anyhow::Result<DaemonEnd> {
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
    let update = update_options();
    let update_off_reason = update.as_ref().err().cloned();
    let outcome = run_daemon(DaemonOptions {
        idle_exit: idle_exit.map(Duration::from_secs),
        torrent_test,
        notifier: desktop_wanted(|k| std::env::var(k).ok())
            .then(|| Arc::new(DesktopNotifier::new()) as Arc<dyn Notifier>),
        update: update.ok(),
        update_off_reason,
        // A daemon started by a Windows update hand-off waits for its predecessor.
        lock_wait: (std::env::var_os("PUPPY_UPDATE_HANDOFF").as_deref() == Some("1".as_ref()))
            .then_some(Duration::from_secs(5)),
        ..DaemonOptions::default()
    })
    .await?;
    match outcome {
        RunOutcome::Exited => Ok(DaemonEnd::Done),
        RunOutcome::Restart => Ok(DaemonEnd::Restart),
        RunOutcome::AlreadyRunning => {
            err("puppy: a daemon is already running; nothing to do");
            Ok(DaemonEnd::Done)
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
    let api = ClientApi::new(paths, exe).with_http(config.http_config());
    puppy_tui::runtime::run(Arc::new(api), theme).await
}
