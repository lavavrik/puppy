//! Rolling download-speed history for the sparklines: one ring per download plus one
//! for the global throughput, a sample per daemon snapshot.

use std::collections::{HashMap, VecDeque};

use puppy_core::types::DownloadId;
use puppy_proto::{DownloadSnapshot, Totals};

/// Samples kept per ring.
pub const HISTORY_LEN: usize = 120;

#[derive(Clone, Debug, Default)]
pub struct History {
    per: HashMap<DownloadId, VecDeque<u64>>,
    global: VecDeque<u64>,
}

fn push(ring: &mut VecDeque<u64>, v: u64) {
    if ring.len() == HISTORY_LEN {
        ring.pop_front();
    }
    ring.push_back(v);
}

impl History {
    /// Records one snapshot: each download's `down_bps` and the global `down_bps`.
    /// Downloads absent from the snapshot are forgotten.
    pub fn record(&mut self, downloads: &[DownloadSnapshot], totals: &Totals) {
        self.per
            .retain(|id, _| downloads.iter().any(|d| d.id == *id));
        for d in downloads {
            push(self.per.entry(d.id).or_default(), d.down_bps);
        }
        push(&mut self.global, totals.down_bps);
    }

    /// Forgets downloads not in `downloads` without recording a sample.
    pub fn prune(&mut self, downloads: &[DownloadSnapshot]) {
        self.per
            .retain(|id, _| downloads.iter().any(|d| d.id == *id));
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
        }
    }

    #[test]
    fn history_ring_caps_at_120() {
        let mut h = History::default();
        for i in 0..121 {
            h.record(&[d(1, i)], &Totals::default());
        }
        let r = h.get(1).unwrap();
        assert_eq!(r.len(), HISTORY_LEN);
        assert_eq!((r[0], r[119]), (1, 120));
        assert_eq!(h.global().len(), HISTORY_LEN);
        h.record(&[d(2, 5)], &Totals::default());
        assert!(h.get(1).is_none());
        assert_eq!(h.get(2).unwrap().len(), 1);
    }
}
