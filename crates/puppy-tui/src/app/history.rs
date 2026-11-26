//! Rolling download-speed history for the sparklines: one ring per download plus one
//! for the global throughput, one sample per second (the highest speed among the
//! snapshots of that second).

use std::collections::{HashMap, HashSet, VecDeque};
use std::time::{Duration, Instant};

use puppy_core::types::DownloadId;
use puppy_proto::{DownloadSnapshot, Totals};

/// One-second samples kept per ring.
pub const HISTORY_LEN: usize = 120;
/// What one sample covers.
pub const SAMPLE_PERIOD: Duration = Duration::from_secs(1);

#[derive(Clone, Debug, Default)]
pub struct History {
    per: HashMap<DownloadId, VecDeque<u64>>,
    global: VecDeque<u64>,
    /// When the newest sample's second began (`None` before the first snapshot).
    second: Option<Instant>,
}

fn push(ring: &mut VecDeque<u64>, v: u64) {
    if ring.len() == HISTORY_LEN {
        ring.pop_front();
    }
    ring.push_back(v);
}

/// Folds `v` into the newest sample (`fresh`: a new second begins).
fn fold(ring: &mut VecDeque<u64>, v: u64, fresh: bool) {
    match ring.back_mut() {
        Some(last) if !fresh => *last = (*last).max(v),
        _ => push(ring, v),
    }
}

impl History {
    /// Records one snapshot taken at `now`: each download's `down_bps` and the global
    /// `down_bps` go into the current second's sample (the highest wins). Downloads
    /// absent from the snapshot are forgotten. Seconds without snapshots (the daemon
    /// was away) get no samples.
    pub fn record(&mut self, downloads: &[DownloadSnapshot], totals: &Totals, now: Instant) {
        self.prune(downloads);
        let fresh = match self.second {
            Some(start) if now.saturating_duration_since(start) < SAMPLE_PERIOD => false,
            Some(start) => {
                // Stay on whole seconds from the first snapshot.
                let elapsed = now.saturating_duration_since(start);
                let whole = elapsed.as_secs();
                self.second = Some(start + Duration::from_secs(whole));
                true
            }
            None => {
                self.second = Some(now);
                true
            }
        };
        for d in downloads {
            let ring = self.per.entry(d.id).or_default();
            // New this second: its first sample.
            let new = ring.is_empty();
            fold(ring, d.down_bps, fresh || new);
        }
        fold(&mut self.global, totals.down_bps, fresh);
    }

    /// Forgets downloads not in `downloads` without recording a sample.
    pub fn prune(&mut self, downloads: &[DownloadSnapshot]) {
        let live: HashSet<DownloadId> = downloads.iter().map(|d| d.id).collect();
        self.per.retain(|id, _| live.contains(id));
    }

    /// The download's samples, oldest first.
    pub fn get(&self, id: DownloadId) -> Option<&VecDeque<u64>> {
        self.per.get(&id)
    }

    /// Global throughput samples, oldest first.
    pub fn global(&self) -> &VecDeque<u64> {
        &self.global
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use puppy_proto::{EngineKindTag, WireState};
    use std::time::{Duration, Instant};

    fn d(id: DownloadId, bps: u64) -> DownloadSnapshot {
        DownloadSnapshot {
            id,
            kind: EngineKindTag::Http,
            source: String::new(),
            name: String::new(),
            path: None,
            state: WireState::Active,
            total_bytes: None,
            done_bytes: 0,
            down_bps: bps,
            up_bps: 0,
            eta_secs: None,
            limit: None,
            queue_position: None,
            connections: None,
            peers: None,
            next_retry_unix: None,
            note: None,
            seed: None,
            checksum: None,
            mirrors: Vec::new(),
            expected_size: None,
        }
    }

    #[test]
    fn one_sample_per_second_keeping_the_max() {
        let t0 = Instant::now();
        let ms = |n: u64| t0 + Duration::from_millis(n);
        let tot = |bps: u64| Totals {
            down_bps: bps,
            ..Totals::default()
        };
        let mut h = History::default();
        // Four snapshots in the first second: one sample, their max.
        for (at, v) in [(0, 5), (250, 9), (500, 3), (750, 7)] {
            h.record(&[d(1, v)], &tot(v * 10), ms(at));
        }
        assert_eq!(h.get(1).unwrap(), &[9]);
        assert_eq!(h.global(), &[90]);
        // The next second starts a new sample.
        h.record(&[d(1, 2)], &tot(20), ms(1000));
        h.record(&[d(1, 4)], &tot(40), ms(1900));
        assert_eq!(h.get(1).unwrap(), &[9, 4]);
        assert_eq!(h.global(), &[90, 40]);
        // A download that appears mid-second gets its own first sample.
        h.record(&[d(1, 1), d(2, 6)], &tot(10), ms(1950));
        assert_eq!(h.get(2).unwrap(), &[6]);
        assert_eq!(h.get(1).unwrap(), &[9, 4]);
        // A gap (no snapshots) does not invent samples; seconds stay aligned.
        h.record(&[d(1, 8)], &tot(80), ms(5100));
        h.record(&[d(1, 1)], &tot(10), ms(5999));
        h.record(&[d(1, 2)], &tot(20), ms(6000));
        assert_eq!(h.get(1).unwrap(), &[9, 4, 8, 2]);
    }

    #[test]
    fn history_ring_caps_at_120() {
        let mut h = History::default();
        let t0 = Instant::now();
        let sec = |i: u64| t0 + Duration::from_secs(i);
        for i in 0..121 {
            h.record(&[d(1, i)], &Totals::default(), sec(i));
        }
        let r = h.get(1).unwrap();
        assert_eq!(r.len(), HISTORY_LEN);
        assert_eq!((r[0], r[119]), (1, 120));
        assert_eq!(h.global().len(), HISTORY_LEN);
        h.record(&[d(2, 5)], &Totals::default(), sec(121));
        assert!(h.get(1).is_none());
        assert_eq!(h.get(2).unwrap().len(), 1);
    }
}
