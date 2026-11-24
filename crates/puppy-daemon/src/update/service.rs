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
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};
use url::Url;

/// How long a release lookup may take.
const LOOKUP_TIMEOUT: Duration = Duration::from_secs(30);
/// How long a whole install attempt (download, verify, sanity check, swap) may take.
const INSTALL_TIMEOUT: Duration = Duration::from_secs(10 * 60);

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
    restart: CancellationToken,
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
                restart: CancellationToken::new(),
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
    /// of the check in `update.json`. While an install runs or after one finished, it
    /// only returns the status.
    pub async fn check_now(&self) -> UpdateStatus {
        // Holding the install lock keeps a check from overwriting `Installing`.
        let Ok(_installing) = self.inner.install.try_lock() else {
            return self.status();
        };
        if matches!(self.status(), UpdateStatus::Installed { .. }) {
            return self.status();
        }
        self.lookup().await
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
        self.set_status(status.clone());
        status
    }

    /// Installs the newer release now: looks it up unless the last check found one,
    /// downloads and verifies it, sanity-checks the new binary and swaps it in. Then the
    /// status is `Installed` and [`restart_requested`](Self::restart_requested) fires.
    /// A call made while another runs waits for it and returns its outcome. Nothing
    /// newer is `Failed`.
    pub async fn install_now(&self) -> Result<semver::Version, UpdateError> {
        let i = &self.inner;
        let seen = i.attempts.load(Ordering::SeqCst);
        let mut last = i.install.lock().await;
        if i.attempts.load(Ordering::SeqCst) != seen
            && let Some(outcome) = last.clone()
        {
            // An attempt finished while this call waited: it was this call's too.
            return outcome.into();
        }
        if let UpdateStatus::Installed { version } = self.status()
            && let Ok(v) = Version::parse(&version)
        {
            return Ok(v);
        }
        let result = self.attempt().await;
        *last = Some(Outcome::from(&result));
        i.attempts.fetch_add(1, Ordering::SeqCst);
        drop(last);
        if result.is_ok() {
            i.restart.cancel();
        }
        result
    }

    /// One install attempt; the caller holds the install lock.
    async fn attempt(&self) -> Result<Version, UpdateError> {
        let i = &self.inner;
        if let Some(reason) = cannot_install_reason(&i.opts.exe, i.opts.allow_dev) {
            return Err(UpdateError::NotPossible(reason));
        }
        let cached = i.release.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let release = match cached {
            Some(r) => r,
            None => {
                if let UpdateStatus::Failed { error, .. } = self.lookup().await {
                    return Err(UpdateError::Failed(error));
                }
                i.release
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .clone()
                    .ok_or_else(|| {
                        UpdateError::Failed(format!(
                            "puppy {} is the latest release; nothing to install",
                            i.current
                        ))
                    })?
            }
        };
        let version = release.version.clone();
        self.set_status(UpdateStatus::Installing {
            version: version.to_string(),
        });
        info!(from = %i.current, to = %version, "installing an update");
        let steps = async {
            let bin = fetch(&i.http, &release, &i.work_dir).await?;
            sanity_check(&bin, &version).await?;
            let (bin, exe) = (bin, i.opts.exe.clone());
            tokio::task::spawn_blocking(move || replace_exe(&bin, &exe))
                .await
                .map_err(|e| UpdateError::Failed(format!("the swap did not finish: {e}")))?
        };
        let result = tokio::time::timeout(INSTALL_TIMEOUT, steps)
            .await
            .unwrap_or_else(|_| {
                Err(UpdateError::Failed(format!(
                    "the update did not finish within {} minutes",
                    INSTALL_TIMEOUT.as_secs() / 60
                )))
            });
        match result {
            Ok(()) => {
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
        }
    }

    /// Checks for releases until `stop`: first `check_delay` after start if one is due,
    /// then whenever the last check is `check_every` old. Nothing at all with
    /// `[update] check = false`.
    pub async fn run(self, _clients: Arc<AtomicUsize>, stop: CancellationToken) {
        if !self.inner.cfg.check {
            return;
        }
        let mut wait = self.inner.opts.check_delay;
        loop {
            tokio::select! {
                _ = stop.cancelled() => return,
                _ = tokio::time::sleep(wait) => {}
            }
            let until_due = self.until_due();
            if until_due.is_zero() {
                tokio::select! {
                    _ = stop.cancelled() => return,
                    _ = self.check_now() => {}
                }
                // TASK 5: the automatic install goes here (when `auto_install`, the
                // release can be installed, and `is_quiet` with no `_clients` has held
                // for `settle`).
                wait = self.inner.opts.check_every;
            } else {
                wait = until_due;
            }
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
        match last {
            Some(last) if last <= unix_now() => {
                every.saturating_sub(Duration::from_secs(unix_now() - last))
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

    /// Changes the status and tells subscribers (only when it really changed).
    fn set_status(&self, status: UpdateStatus) {
        let mut cur = self.inner.status.lock().unwrap_or_else(|e| e.into_inner());
        if *cur == status {
            return;
        }
        *cur = status.clone();
        drop(cur);
        self.inner.manager.publish(Event::Update { status });
    }
}
