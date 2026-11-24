//! The daemon's updater: keeps the update status, checks for releases on a schedule,
//! and installs one on request.

use super::{
    Release, TARGET, UpdateError, cannot_install_reason, fetch, latest, replace_exe, sanity_check,
};
use crate::config::UpdateConfig;
use crate::manager::Manager;
use crate::paths::Paths;
use puppy_proto::{Event, UpdateStatus};
use semver::Version;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};
use url::Url;

/// How long a release lookup may take.
const LOOKUP_TIMEOUT: Duration = Duration::from_secs(30);
/// How long a whole install attempt (download, verify, sanity check, swap) may take.
const INSTALL_TIMEOUT: Duration = Duration::from_secs(10 * 60);
/// How often the conditions for an automatic install are looked at.
const QUIET_CHECK: Duration = Duration::from_millis(250);
/// In the update work dir: the version that installed an update, written just before
/// the swap and taken by the next daemon at start ([`take_updated_marker`]).
const UPDATED_FROM: &str = "updated-from";

/// Reads and removes the marker an install left in `update_dir`: the version the
/// update was installed from. `None` when there is none (or it cannot be read).
pub fn take_updated_marker(update_dir: &Path) -> Option<String> {
    let path = update_dir.join(UPDATED_FROM);
    let text = std::fs::read_to_string(&path).ok()?;
    if let Err(e) = std::fs::remove_file(&path) {
        warn!(path = %path.display(), "cannot remove the update marker: {e}");
    }
    Some(text.trim().to_string())
}

/// How the daemon updates itself. `DaemonOptions::update`; `None` there turns updates off.
#[derive(Debug, Clone)]
pub struct UpdateOptions {
    /// Where the GitHub API lives (`https://api.github.com`; a fake in tests).
    pub api_base: Url,
    /// The binary to replace (main: the canonicalized current executable).
    pub exe: PathBuf,
    /// Tests only: install even though this is not a release build.
    pub allow_dev: bool,
    /// How long the daemon must stay quiet before installing by itself (main: 60 s).
    pub settle: Duration,
    /// The first check runs this long after start, if one is due (main: 5 s).
    pub check_delay: Duration,
    /// Checks run this often (main: 24 h); a check is due when the last one recorded
    /// in `update.json` is at least this old.
    pub check_every: Duration,
}

/// `update.json`: when the last release lookup ran.
#[derive(Serialize, Deserialize)]
struct UpdateState {
    /// Unix seconds.
    last_check: u64,
}

/// The daemon's updater. Cheap to clone; clones share one status.
#[derive(Clone)]
pub struct Updater {
    inner: Arc<Inner>,
}

struct Inner {
    opts: UpdateOptions,
    cfg: UpdateConfig,
    http: reqwest::Client,
    work_dir: PathBuf,
    state_file: PathBuf,
    manager: Arc<Manager>,
    current: Version,
    status: Mutex<UpdateStatus>,
    /// The release found by the last lookup, if it is newer.
    release: Mutex<Option<Release>>,
    /// Held for a whole install attempt; holds the last attempt's outcome so callers
    /// that waited for it share it.
    install: tokio::sync::Mutex<Option<Outcome>>,
    /// Finished install attempts.
    attempts: AtomicU64,
    /// Held for a whole release lookup.
    check: tokio::sync::Mutex<()>,
    /// Finished lookups, so a check that waited for a running one takes its result.
    checks: AtomicU64,
    restart: CancellationToken,
    /// The daemon's connected-client count, given to [`Updater::run`].
    clients: OnceLock<Arc<AtomicUsize>>,
    /// A downloaded, verified binary an automatic install gave way with, kept so the
    /// next attempt at the same version skips the download.
    staged: Mutex<Option<(Version, PathBuf)>>,
}

/// What must still hold just before an install swaps the binary in.
#[derive(Clone, Copy, Debug)]
enum Gate {
    /// Asked for explicitly: nothing.
    Forced,
    /// Automatic: [`Manager::is_quiet`] and no client.
    Quiet,
    /// Before an idle exit: [`Manager::is_idle`] and no client.
    Idle,
}

/// A finished install attempt, shareable among the callers that waited for it.
#[derive(Clone)]
enum Outcome {
    Installed(Version),
    NotPossible(String),
    Failed(String),
}

impl From<Outcome> for Result<Version, UpdateError> {
    fn from(o: Outcome) -> Self {
        match o {
            Outcome::Installed(v) => Ok(v),
            Outcome::NotPossible(m) => Err(UpdateError::NotPossible(m)),
            Outcome::Failed(m) => Err(UpdateError::Failed(m)),
        }
    }
}

impl From<&Result<Version, UpdateError>> for Outcome {
    fn from(r: &Result<Version, UpdateError>) -> Self {
        match r {
            Ok(v) => Outcome::Installed(v.clone()),
            Err(UpdateError::NotPossible(m)) => Outcome::NotPossible(m.clone()),
            Err(UpdateError::Failed(m)) => Outcome::Failed(m.clone()),
        }
    }
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

impl Updater {
    pub fn new(
        opts: UpdateOptions,
        cfg: UpdateConfig,
        http: reqwest::Client,
        paths: &Paths,
        manager: Arc<Manager>,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                opts,
                cfg,
                http,
                work_dir: paths.update_dir.clone(),
                state_file: paths.update_state.clone(),
                manager,
                current: Version::parse(env!("CARGO_PKG_VERSION"))
                    .expect("the package version is semver"),
                status: Mutex::new(UpdateStatus::NotChecked),
                release: Mutex::new(None),
                install: tokio::sync::Mutex::new(None),
                attempts: AtomicU64::new(0),
                check: tokio::sync::Mutex::new(()),
                checks: AtomicU64::new(0),
                restart: CancellationToken::new(),
                clients: OnceLock::new(),
                staged: Mutex::new(None),
            }),
        }
    }

    /// The current status, from memory.
    pub fn status(&self) -> UpdateStatus {
        self.inner
            .status
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Cancelled once a new binary has been installed: the daemon should shut down and
    /// start it.
    pub fn restart_requested(&self) -> CancellationToken {
        self.inner.restart.clone()
    }

    /// Looks up the latest release now (whether or not one is due) and records the time
    /// of the check in `update.json`. A check already running is waited for and its
    /// result returned. While an install runs or after one finished, it only returns
    /// the status.
    pub async fn check_now(&self) -> UpdateStatus {
        let i = &self.inner;
        let seen = i.checks.load(Ordering::SeqCst);
        if matches!(
            self.status(),
            UpdateStatus::Installing { .. } | UpdateStatus::Installed { .. }
        ) {
            return self.status();
        }
        let _checking = i.check.lock().await;
        if i.checks.load(Ordering::SeqCst) != seen {
            // A lookup finished while this call waited: its result is this call's too.
            return self.status();
        }
        let status = self.lookup().await;
        i.checks.fetch_add(1, Ordering::SeqCst);
        status
    }

    async fn lookup(&self) -> UpdateStatus {
        let i = &self.inner;
        let found = tokio::time::timeout(
            LOOKUP_TIMEOUT,
            latest(&i.http, &i.opts.api_base, &i.current, TARGET),
        )
        .await
        .unwrap_or_else(|_| {
            Err(UpdateError::Failed(format!(
                "the release server did not answer within {} seconds",
                LOOKUP_TIMEOUT.as_secs()
            )))
        });
        self.record_check();
        let status = match found {
            Ok(Some(release)) => {
                let reason = cannot_install_reason(&i.opts.exe, i.opts.allow_dev);
                let status = UpdateStatus::Available {
                    version: release.version.to_string(),
                    can_install: reason.is_none(),
                    reason,
                };
                *i.release.lock().unwrap_or_else(|e| e.into_inner()) = Some(release);
                status
            }
            Ok(None) => {
                *i.release.lock().unwrap_or_else(|e| e.into_inner()) = None;
                UpdateStatus::UpToDate
            }
            Err(e) => {
                warn!("update check failed: {e}");
                *i.release.lock().unwrap_or_else(|e| e.into_inner()) = None;
                UpdateStatus::Failed {
                    version: None,
                    error: e.to_string(),
                }
            }
        };
        self.set_checked(status)
    }

    /// Installs the newer release now: looks it up unless the last check found one,
    /// downloads and verifies it, sanity-checks the new binary and swaps it in. Then the
    /// status is `Installed` and [`restart_requested`](Self::restart_requested) fires.
    /// A call made while another runs waits for it and returns its outcome. Nothing
    /// newer is `Failed`.
    pub async fn install_now(&self) -> Result<semver::Version, UpdateError> {
        self.install(Gate::Forced, true)
            .await
            .expect("a forced install never gives way")
    }

    /// For a daemon about to idle-exit: installs the update the last check found, if
    /// automatic installs are on ([`auto`](Self::auto)) and it can be installed (true:
    /// the binary was swapped).
    /// Unlike [`install_now`](Self::install_now) this does not fire
    /// [`restart_requested`](Self::restart_requested): the daemon exits as planned and
    /// the next client starts the new binary. (An install already running is waited
    /// for and may fire it, but by then the daemon has chosen to exit.) A download or a
    /// client that appears meanwhile wins: nothing is swapped and the status is back to
    /// `Available`.
    pub async fn install_before_exit(&self) -> bool {
        if !self.auto() || !self.installable() {
            return false;
        }
        info!("installing the update before exiting");
        matches!(self.install(Gate::Idle, false).await, Some(Ok(_)))
    }

    /// Whether the daemon may install by itself: `[update] check` and `auto_install`
    /// both on. With `check = false` puppy only contacts GitHub when asked to, so a
    /// release found by an explicit check is never downloaded on its own.
    fn auto(&self) -> bool {
        self.inner.cfg.check && self.inner.cfg.auto_install
    }

    /// Whether `gate` still allows swapping the binary in now.
    fn gate_open(&self, gate: Gate) -> bool {
        let i = &self.inner;
        let no_clients = || {
            i.clients
                .get()
                .is_none_or(|c| c.load(Ordering::SeqCst) == 0)
        };
        match gate {
            Gate::Forced => true,
            Gate::Quiet => i.manager.is_quiet() && no_clients(),
            Gate::Idle => i.manager.is_idle() && no_clients(),
        }
    }

    /// Whether the last check found a release this binary can install.
    fn installable(&self) -> bool {
        matches!(
            self.status(),
            UpdateStatus::Available {
                can_install: true,
                ..
            }
        )
    }

    /// [`install_now`](Self::install_now) behind `gate`; `restart`: fire the restart token
    /// on success. `None`: the gate closed before the swap (the attempt gave way and
    /// counts as none: callers that waited for it make their own).
    async fn install(&self, gate: Gate, restart: bool) -> Option<Result<Version, UpdateError>> {
        let i = &self.inner;
        let seen = i.attempts.load(Ordering::SeqCst);
        let mut last = i.install.lock().await;
        if i.attempts.load(Ordering::SeqCst) != seen
            && let Some(outcome) = last.clone()
        {
            // An attempt finished while this call waited: it was this call's too.
            return Some(outcome.into());
        }
        if let UpdateStatus::Installed { version } = self.status()
            && let Ok(v) = Version::parse(&version)
        {
            return Some(Ok(v));
        }
        let result = self.attempt(gate).await?;
        *last = Some(Outcome::from(&result));
        i.attempts.fetch_add(1, Ordering::SeqCst);
        drop(last);
        if restart && result.is_ok() {
            i.restart.cancel();
        }
        Some(result)
    }

    /// One install attempt; the caller holds the install lock. `None`: `gate` closed
    /// after the new binary was verified; it stays staged and the status is restored.
    async fn attempt(&self, gate: Gate) -> Option<Result<Version, UpdateError>> {
        let i = &self.inner;
        if let Some(reason) = cannot_install_reason(&i.opts.exe, i.opts.allow_dev) {
            // The release is known but cannot go in here: say so, which also stops
            // automatic installs from retrying it.
            self.change_status(|cur| match cur {
                UpdateStatus::Available { version, .. } => UpdateStatus::Available {
                    version: version.clone(),
                    can_install: false,
                    reason: Some(reason.clone()),
                },
                other => other.clone(),
            });
            return Some(Err(UpdateError::NotPossible(reason)));
        }
        let cached = i.release.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let release = match cached {
            Some(r) => r,
            None => {
                if let UpdateStatus::Failed { error, .. } = self.check_now().await {
                    return Some(Err(UpdateError::Failed(error)));
                }
                let found = i.release.lock().unwrap_or_else(|e| e.into_inner()).clone();
                match found {
                    Some(r) => r,
                    None => {
                        return Some(Err(UpdateError::Failed(format!(
                            "puppy {} is the latest release; nothing to install",
                            i.current
                        ))));
                    }
                }
            }
        };
        let version = release.version.clone();
        let before = self.status();
        self.set_status(UpdateStatus::Installing {
            version: version.to_string(),
        });
        info!(from = %i.current, to = %version, "installing an update");
        // Taken: kept again only if this attempt gives way too.
        let staged = i
            .staged
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
            .filter(|(v, bin)| *v == version && bin.is_file());
        let steps = async {
            let bin = match staged {
                Some((_, bin)) => bin,
                // Empties the work dir first, so a binary staged for another version goes.
                None => fetch(&i.http, &release, &i.work_dir).await?,
            };
            sanity_check(&bin, &version).await?;
            if !self.gate_open(gate) {
                *i.staged.lock().unwrap_or_else(|e| e.into_inner()) = Some((version.clone(), bin));
                return Ok(false);
            }
            // Tells the next daemon what it was updated from; only a notice, so failing
            // to write it is only logged.
            let marker = i.work_dir.join(UPDATED_FROM);
            if let Err(e) = std::fs::write(&marker, i.current.to_string()) {
                warn!(path = %marker.display(), "cannot write the update marker: {e}");
            }
            let (bin, exe) = (bin, i.opts.exe.clone());
            let swapped = tokio::task::spawn_blocking(move || replace_exe(&bin, &exe))
                .await
                .map_err(|e| UpdateError::Failed(format!("the swap did not finish: {e}")))
                .and_then(|r| r);
            if swapped.is_err() {
                let _ = std::fs::remove_file(&marker);
            }
            swapped.map(|()| true)
        };
        let result = tokio::time::timeout(INSTALL_TIMEOUT, steps)
            .await
            .unwrap_or_else(|_| {
                Err(UpdateError::Failed(format!(
                    "the update did not finish within {} minutes",
                    INSTALL_TIMEOUT.as_secs() / 60
                )))
            });
        Some(match result {
            Ok(false) => {
                info!(version = %version, "work or a client appeared; the update waits");
                self.set_status(before);
                return None;
            }
            Ok(true) => {
                info!(version = %version, "update installed");
                self.set_status(UpdateStatus::Installed {
                    version: version.to_string(),
                });
                Ok(version)
            }
            Err(e) => {
                warn!(version = %version, "update failed: {e}");
                self.set_status(UpdateStatus::Failed {
                    version: Some(version.to_string()),
                    error: e.to_string(),
                });
                Err(e)
            }
        })
    }

    /// Runs until `stop`: checks for releases on schedule, and installs an update by
    /// itself once the daemon has been quiet long enough.
    pub async fn run(self, clients: Arc<AtomicUsize>, stop: CancellationToken) {
        let _ = self.inner.clients.set(clients.clone());
        tokio::select! {
            _ = stop.cancelled() => {}
            _ = async { tokio::join!(self.check_on_schedule(), self.install_when_quiet(&clients)) } => {}
        }
    }

    /// Checks for releases: first `check_delay` after start if one is due, then
    /// whenever the last check is `check_every` old. Nothing at all with
    /// `[update] check = false`.
    async fn check_on_schedule(&self) {
        if !self.inner.cfg.check {
            return;
        }
        let mut wait = self.inner.opts.check_delay;
        loop {
            tokio::time::sleep(wait).await;
            let until_due = self.until_due();
            if until_due.is_zero() {
                self.check_now().await;
                wait = self.inner.opts.check_every;
            } else {
                wait = until_due;
            }
        }
    }

    /// With automatic installs on ([`auto`](Self::auto)): once an installable release
    /// is known and the daemon has been quiet (no download queued, starting, active
    /// or waiting for a retry; seeding does not count) with no client connected for
    /// `settle` without a break, installs it. Returns after a successful install; after a failed one
    /// it waits for a check to find a release again.
    async fn install_when_quiet(&self, clients: &AtomicUsize) {
        let i = &self.inner;
        if !self.auto() {
            return;
        }
        let mut tick = tokio::time::interval(QUIET_CHECK);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut quiet_since: Option<tokio::time::Instant> = None;
        loop {
            let now = tick.tick().await;
            if !(self.installable() && i.manager.is_quiet() && clients.load(Ordering::SeqCst) == 0)
            {
                quiet_since = None;
                continue;
            }
            if now.duration_since(*quiet_since.get_or_insert(now)) < i.opts.settle {
                continue;
            }
            info!(settle = ?i.opts.settle, "quiet: installing the update");
            if let Some(Ok(_)) = self.install(Gate::Quiet, true).await {
                return;
            }
            // Failed (waits for a new check), or gave way to new work (waits for quiet).
            quiet_since = None;
        }
    }

    /// How long until a check is due (zero: due now). Due when `update.json` is
    /// missing, unreadable, or from the future.
    fn until_due(&self) -> Duration {
        let every = self.inner.opts.check_every;
        let last = std::fs::read(&self.inner.state_file)
            .ok()
            .and_then(|b| serde_json::from_slice::<UpdateState>(&b).ok())
            .map(|s| s.last_check);
        let now = unix_now();
        match last {
            Some(last) if last <= now => {
                every.saturating_sub(Duration::from_secs(now.saturating_sub(last)))
            }
            _ => Duration::ZERO,
        }
    }

    /// Writes the time of this check to `update.json`; failing is only logged.
    fn record_check(&self) {
        let path = &self.inner.state_file;
        let json = serde_json::to_vec(&UpdateState {
            last_check: unix_now(),
        })
        .expect("serializable");
        if let Err(e) = std::fs::write(path, json) {
            warn!(path = %path.display(), "cannot record the update check: {e}");
        }
    }

    /// Sets the result of a lookup and returns it, unless an install has started or
    /// finished meanwhile: then that status stays and is returned.
    fn set_checked(&self, status: UpdateStatus) -> UpdateStatus {
        self.change_status(|cur| {
            if matches!(
                cur,
                UpdateStatus::Installing { .. } | UpdateStatus::Installed { .. }
            ) {
                cur.clone()
            } else {
                status
            }
        })
    }

    /// Changes the status and tells subscribers (only when it really changed).
    fn set_status(&self, status: UpdateStatus) {
        self.change_status(|_| status);
    }

    /// Replaces the status with `next(current)` and returns it. Subscribers hear of a
    /// change while the status is locked, so they see changes in order.
    fn change_status(&self, next: impl FnOnce(&UpdateStatus) -> UpdateStatus) -> UpdateStatus {
        let mut cur = self.inner.status.lock().unwrap_or_else(|e| e.into_inner());
        let status = next(&cur);
        if *cur != status {
            *cur = status.clone();
            self.inner.manager.publish(Event::Update {
                status: status.clone(),
            });
        }
        status
    }
}
