//! Seeding after completion: cumulative upload, seeding time, targets and the stop
//! decision ([`SeedTracker`]), kept across restarts in `<data_dir>/torrents/<id>.seed`
//! ([`SeedRecord`]).
//!
//! - Uploaded bytes are cumulative: the bytes of earlier librqbit sessions of the torrent
//!   (its counter starts at 0 whenever it goes live: after a pause, a restart) plus the
//!   current session's counter.
//! - Seeding time is the wall-clock time spent seeding while puppy runs (advanced by the
//!   poll task), not the time since completion.
//! - A target left `None` in the torrent's [`SeedPolicy`] is the engine's global value,
//!   read when it is used; 0 disables that trigger; both 0 stops at completion.

use std::path::Path;
use std::time::{Duration, Instant};

use puppy_core::types::{DownloadState, SeedPolicy};
use serde::{Deserialize, Serialize};

/// How often a seeding torrent's counters are written.
pub(crate) const PERSIST_EVERY: Duration = Duration::from_secs(30);

/// Where a torrent is in its seeding life.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SeedPhase {
    /// Its data is not complete yet (as far as puppy has seen).
    Downloading,
    /// Complete and uploading until a target is met.
    Seeding,
    /// Complete; seeding stopped (target met, both targets 0, or paused): `Completed`.
    Done,
}

/// The `.seed` sidecar.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct SeedRecord {
    /// Bytes uploaded in total (every earlier session included).
    #[serde(default)]
    pub base_uploaded: u64,
    /// Seconds spent seeding.
    #[serde(default)]
    pub seeded_secs: u64,
    /// When the data was complete (Unix seconds); `None`: still downloading.
    #[serde(default)]
    pub last_completed_unix: Option<u64>,
    /// Seeding has stopped (the torrent is `Completed`).
    #[serde(default)]
    pub stopped: bool,
}

impl SeedRecord {
    pub fn read(path: &Path) -> Option<SeedRecord> {
        let text = std::fs::read_to_string(path).ok()?;
        match serde_json::from_str(&text) {
            Ok(r) => Some(r),
            Err(e) => {
                tracing::warn!(path = %path.display(), "ignoring an unreadable seed record: {e}");
                None
            }
        }
    }

    /// Writes atomically (temporary file, then rename).
    pub fn write(&self, path: &Path) -> std::io::Result<()> {
        let json = serde_json::to_vec(self).map_err(std::io::Error::other)?;
        let tmp = path.with_extension("seed.tmp");
        std::fs::write(&tmp, json)?;
        std::fs::rename(&tmp, path)
    }
}

/// Effective targets: `(ratio, seconds)`, 0 disabling that trigger.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Targets {
    pub ratio: f32,
    pub time_secs: u64,
}

impl Targets {
    /// The torrent's own values where set, else the global ones.
    pub fn of(policy: &SeedPolicy, global_ratio: f32, global_time: Duration) -> Targets {
        let ratio = policy.ratio.unwrap_or(global_ratio);
        Targets {
            ratio: if ratio.is_finite() && ratio > 0.0 {
                ratio
            } else {
                0.0
            },
            time_secs: policy.time_secs.unwrap_or(global_time.as_secs()),
        }
    }

    /// Both triggers off: stop at completion.
    pub fn none(self) -> bool {
        self.ratio <= 0.0 && self.time_secs == 0
    }

    /// Whether seeding should stop with these counters.
    pub fn met(self, ratio: f32, seeded_secs: u64) -> bool {
        self.none()
            || (self.ratio > 0.0 && ratio >= self.ratio)
            || (self.time_secs > 0 && seeded_secs >= self.time_secs)
    }
}

/// Share ratio: uploaded / size, 0 for an empty torrent (always finite).
pub(crate) fn ratio(uploaded: u64, total: u64) -> f32 {
    if total == 0 {
        0.0
    } else {
        let r = (uploaded as f64 / total as f64) as f32;
        if r.is_finite() { r } else { f32::MAX }
    }
}

/// A torrent's seeding counters and phase.
#[derive(Debug, Clone)]
pub(crate) struct SeedTracker {
    /// The torrent's own targets (`None` fields: the global values).
    pub policy: SeedPolicy,
    pub phase: SeedPhase,
    /// Bytes uploaded by earlier librqbit sessions of the torrent.
    base_uploaded: u64,
    /// The current session's counter as last seen.
    session_uploaded: u64,
    seeded: Duration,
    /// When the seeding time was last advanced (`None`: not counting).
    last_tick: Option<Instant>,
    last_persist: Option<Instant>,
    completed_unix: Option<u64>,
    /// `SeedingFinished` still to be sent (seeding found finished at restore).
    pub finish_pending: bool,
}

/// What a status needs of a tracker (copied under the torrents lock).
#[derive(Debug, Clone, Copy)]
pub(crate) struct SeedView {
    pub phase: SeedPhase,
    base_uploaded: u64,
    session_uploaded: u64,
    pub seeded_secs: u64,
    pub targets: Targets,
}

impl SeedView {
    /// Cumulative uploaded bytes, given librqbit's current session counter. A counter
    /// below the one last seen is a new session (the previous one ended unseen).
    pub fn uploaded(&self, counter: u64) -> u64 {
        cumulative(self.base_uploaded, self.session_uploaded, counter)
    }

    /// The `Seeding` state with these counters.
    pub fn seeding_state(&self, counter: u64, total: u64) -> DownloadState {
        let uploaded = self.uploaded(counter);
        DownloadState::Seeding {
            uploaded,
            ratio: ratio(uploaded, total),
            seeded_secs: self.seeded_secs,
            ratio_target: (self.targets.ratio > 0.0).then_some(self.targets.ratio),
            time_target_secs: (self.targets.time_secs > 0).then_some(self.targets.time_secs),
        }
    }
}

fn cumulative(base: u64, last_seen: u64, counter: u64) -> u64 {
    if counter < last_seen {
        base.saturating_add(last_seen).saturating_add(counter)
    } else {
        base.saturating_add(counter)
    }
}

impl SeedTracker {
    /// A new download's tracker.
    pub fn new(policy: SeedPolicy) -> Self {
        Self {
            policy,
            phase: SeedPhase::Downloading,
            base_uploaded: 0,
            session_uploaded: 0,
            seeded: Duration::ZERO,
            last_tick: None,
            last_persist: None,
            completed_unix: None,
            finish_pending: false,
        }
    }

    /// A restored download's tracker, from its record (if any): stopped ⇒ `Done`,
    /// completed ⇒ `Seeding` (see [`Self::settle_restored`]), else downloading.
    pub fn restored(policy: SeedPolicy, rec: Option<SeedRecord>) -> Self {
        let mut t = Self::new(policy);
        if let Some(r) = rec {
            t.base_uploaded = r.base_uploaded;
            t.seeded = Duration::from_secs(r.seeded_secs);
            t.completed_unix = r.last_completed_unix;
            t.phase = match (r.stopped, r.last_completed_unix) {
                (true, _) => SeedPhase::Done,
                (false, Some(_)) => SeedPhase::Seeding,
                (false, None) => SeedPhase::Downloading,
            };
        }
        t
    }

    /// A restored seeding torrent whose target is met already (by its persisted
    /// counters, `total` its size) stops at once: `true` if it did.
    pub fn settle_restored(&mut self, targets: Targets, total: u64) -> bool {
        if self.phase == SeedPhase::Seeding
            && targets.met(ratio(self.base_uploaded, total), self.seeded.as_secs())
        {
            self.phase = SeedPhase::Done;
            self.finish_pending = true;
            return true;
        }
        false
    }

    pub fn view(&self, targets: Targets) -> SeedView {
        SeedView {
            phase: self.phase,
            base_uploaded: self.base_uploaded,
            session_uploaded: self.session_uploaded,
            seeded_secs: self.seeded.as_secs(),
            targets,
        }
    }

    /// Takes librqbit's session counter into account (0 when not live).
    pub fn observe_upload(&mut self, counter: u64) {
        if counter < self.session_uploaded {
            self.base_uploaded = self.base_uploaded.saturating_add(self.session_uploaded);
        }
        self.session_uploaded = counter;
    }

    pub fn uploaded(&self) -> u64 {
        self.base_uploaded.saturating_add(self.session_uploaded)
    }

    pub fn seeded_secs(&self) -> u64 {
        self.seeded.as_secs()
    }

    /// The data is complete: seeding starts (`seed`) or, with both targets 0, is done.
    pub fn complete(&mut self, seed: bool, now_unix: u64) {
        self.completed_unix = Some(now_unix);
        self.phase = if seed {
            SeedPhase::Seeding
        } else {
            SeedPhase::Done
        };
        self.last_tick = None;
        self.last_persist = None;
    }

    /// Advances the seeding time by the time since the last call while `counting` (the
    /// torrent is seeding and live); otherwise the clock stops.
    pub fn tick(&mut self, now: Instant, counting: bool) {
        if counting {
            if let Some(last) = self.last_tick {
                self.seeded += now.saturating_duration_since(last);
            }
            self.last_tick = Some(now);
        } else {
            self.last_tick = None;
        }
    }

    /// Seeding stops (`Completed`): `true` if it was seeding.
    pub fn stop(&mut self) -> bool {
        self.last_tick = None;
        let was = self.phase == SeedPhase::Seeding;
        self.phase = SeedPhase::Done;
        was
    }

    /// Whether the periodic write is due (seeding only).
    pub fn persist_due(&self, now: Instant) -> bool {
        self.phase == SeedPhase::Seeding
            && self
                .last_persist
                .is_none_or(|t| now.saturating_duration_since(t) >= PERSIST_EVERY)
    }

    /// Whether there is anything worth a record.
    pub fn worth_recording(&self) -> bool {
        self.phase != SeedPhase::Downloading || self.uploaded() > 0
    }

    pub fn record(&mut self, now: Instant) -> SeedRecord {
        self.last_persist = Some(now);
        SeedRecord {
            base_uploaded: self.uploaded(),
            seeded_secs: self.seeded.as_secs(),
            last_completed_unix: self.completed_unix,
            stopped: self.phase == SeedPhase::Done,
        }
    }
}

/// Now as Unix seconds.
pub(crate) fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn targets_inherit_and_disable() {
        let g = Duration::from_secs(7200);
        let t = Targets::of(&SeedPolicy::default(), 1.0, g);
        assert_eq!((t.ratio, t.time_secs), (1.0, 7200));
        let own = SeedPolicy {
            ratio: Some(0.0),
            time_secs: Some(60),
        };
        let t = Targets::of(&own, 1.0, g);
        assert_eq!((t.ratio, t.time_secs), (0.0, 60));
        assert!(!t.none());
        let nan = SeedPolicy {
            ratio: Some(f32::NAN),
            time_secs: Some(0),
        };
        assert!(Targets::of(&nan, 1.0, g).none());
    }

    #[test]
    fn whichever_first() {
        let t = Targets {
            ratio: 1.0,
            time_secs: 10,
        };
        assert!(!t.met(0.5, 5));
        assert!(t.met(1.0, 5), "ratio first");
        assert!(t.met(0.1, 10), "time first");
        let ratio_only = Targets {
            ratio: 1.0,
            time_secs: 0,
        };
        assert!(!ratio_only.met(0.9, 1_000_000));
        let none = Targets {
            ratio: 0.0,
            time_secs: 0,
        };
        assert!(none.met(0.0, 0), "both 0: stop at completion");
    }

    #[test]
    fn ratio_is_finite() {
        assert_eq!(ratio(5, 0), 0.0);
        assert_eq!(ratio(150, 100), 1.5);
        assert!(ratio(u64::MAX, 1).is_finite());
    }

    #[test]
    fn uploads_add_up_across_sessions() {
        let mut t = SeedTracker::new(SeedPolicy::default());
        t.observe_upload(100);
        t.observe_upload(300);
        assert_eq!(t.uploaded(), 300);
        // Paused: librqbit reports 0; the session's bytes stay counted.
        t.observe_upload(0);
        assert_eq!(t.uploaded(), 300);
        t.observe_upload(50);
        assert_eq!(t.uploaded(), 350);
        // A view computes the same without a call in between (new session unseen).
        let v = t.view(Targets {
            ratio: 0.0,
            time_secs: 0,
        });
        assert_eq!(v.uploaded(80), 380);
        assert_eq!(v.uploaded(10), 360, "a lower counter is a new session");
        let rec = t.record(Instant::now());
        assert_eq!(rec.base_uploaded, 350);
        let back = SeedTracker::restored(SeedPolicy::default(), Some(rec));
        assert_eq!(back.uploaded(), 350);
        assert_eq!(back.phase, SeedPhase::Downloading);
    }

    #[test]
    fn seeding_time_counts_only_while_counting() {
        let mut t = SeedTracker::new(SeedPolicy::default());
        t.complete(true, 1);
        let t0 = Instant::now();
        t.tick(t0, true);
        t.tick(t0 + Duration::from_millis(1500), true);
        assert_eq!(t.seeded_secs(), 1);
        t.tick(t0 + Duration::from_secs(5), false);
        t.tick(t0 + Duration::from_secs(9), true);
        assert_eq!(t.seeded_secs(), 1, "not counted while not seeding");
        t.tick(t0 + Duration::from_millis(9600), true);
        assert_eq!(t.seeded_secs(), 2);
    }

    #[test]
    fn restore_phases() {
        let rec = |stopped, done: Option<u64>| SeedRecord {
            base_uploaded: 500,
            seeded_secs: 20,
            last_completed_unix: done,
            stopped,
        };
        let p = SeedPolicy::default();
        assert_eq!(
            SeedTracker::restored(p.clone(), Some(rec(false, Some(1)))).phase,
            SeedPhase::Seeding
        );
        assert_eq!(
            SeedTracker::restored(p.clone(), Some(rec(true, Some(1)))).phase,
            SeedPhase::Done
        );
        assert_eq!(
            SeedTracker::restored(p.clone(), None).phase,
            SeedPhase::Downloading
        );
        let mut t = SeedTracker::restored(p, Some(rec(false, Some(1))));
        let far = Targets {
            ratio: 1.0,
            time_secs: 3600,
        };
        assert!(!t.settle_restored(far, 1000));
        let by_time = Targets {
            ratio: 1.0,
            time_secs: 20,
        };
        assert!(t.settle_restored(by_time, 1000));
        assert_eq!(t.phase, SeedPhase::Done);
        assert!(t.finish_pending);
    }

    #[test]
    fn record_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("1.seed");
        let r = SeedRecord {
            base_uploaded: 7,
            seeded_secs: 3,
            last_completed_unix: Some(9),
            stopped: true,
        };
        r.write(&path).unwrap();
        assert_eq!(SeedRecord::read(&path), Some(r));
        // Older/partial records: fields default.
        std::fs::write(&path, r#"{"base_uploaded":5}"#).unwrap();
        assert_eq!(SeedRecord::read(&path).unwrap().base_uploaded, 5);
        std::fs::write(&path, "nope").unwrap();
        assert_eq!(SeedRecord::read(&path), None);
    }
}
