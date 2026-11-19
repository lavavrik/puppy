//! Pure segment planning and work-stealing split for ranged HTTP downloads.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Segment {
    pub start: u64,
    /// Exclusive end offset.
    pub end: u64,
    /// Bytes completed from `start`.
    pub done: u64,
}

impl Segment {
    pub fn remaining(&self) -> u64 {
        self.end.saturating_sub(self.cursor())
    }

    pub fn is_complete(&self) -> bool {
        self.remaining() == 0
    }

    pub fn cursor(&self) -> u64 {
        self.start + self.done
    }
}

/// Splits `total` bytes into at most `connections` contiguous segments, each at
/// least `min_segment` bytes (except when the file itself is smaller). The
/// remainder is absorbed by the last segment.
pub fn plan_segments(total: u64, connections: u16, min_segment: u64) -> Vec<Segment> {
    if total == 0 {
        return Vec::new();
    }
    let min_segment = min_segment.max(1);
    let by_size = (total / min_segment).max(1);
    let count = u64::from(connections.max(1)).min(by_size);
    let base = total / count;

    (0..count)
        .map(|i| {
            let start = i * base;
            let end = if i + 1 == count { total } else { start + base };
            Segment {
                start,
                end,
                done: 0,
            }
        })
        .collect()
}

/// Shrinks `victim` and returns a new segment covering the back half of its remaining range,
/// or None if remaining < 2*min_segment.
pub fn steal_split(victim: &mut Segment, min_segment: u64) -> Option<Segment> {
    let remaining = victim.remaining();
    if remaining < min_segment.saturating_mul(2) {
        return None;
    }
    let mid = victim.cursor() + remaining / 2;
    let new = Segment {
        start: mid,
        end: victim.end,
        done: 0,
    };
    victim.end = mid;
    Some(new)
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIB: u64 = 1024 * 1024;

    #[test]
    fn plan_even_split() {
        let segs = plan_segments(800, 8, 1);
        assert_eq!(segs.len(), 8);
        for (i, s) in segs.iter().enumerate() {
            assert_eq!(s.start, i as u64 * 100);
            assert_eq!(s.end, (i as u64 + 1) * 100);
            assert_eq!(s.done, 0);
        }
        assert_eq!(segs.last().unwrap().end, 800);
    }

    #[test]
    fn plan_respects_min_segment() {
        let segs = plan_segments(3 * MIB, 8, MIB);
        assert_eq!(segs.len(), 3);
    }

    #[test]
    fn plan_small_file_single() {
        let segs = plan_segments(10, 8, MIB);
        assert_eq!(
            segs,
            vec![Segment {
                start: 0,
                end: 10,
                done: 0
            }]
        );
    }

    #[test]
    fn plan_zero_length() {
        assert!(plan_segments(0, 8, 1).is_empty());
    }

    #[test]
    fn plan_remainder_goes_to_last() {
        let segs = plan_segments(803, 8, 1);
        assert_eq!(segs.len(), 8);
        assert_eq!(segs[0].start, 0);
        assert_eq!(segs.last().unwrap().end, 803);
        for pair in segs.windows(2) {
            assert_eq!(pair[0].end, pair[1].start, "gap or overlap");
        }
        let covered: u64 = segs.iter().map(|s| s.end - s.start).sum();
        assert_eq!(covered, 803);
    }

    #[test]
    fn steal_split_halves_remaining() {
        let mut victim = Segment {
            start: 0,
            end: 1000,
            done: 200,
        };
        let new = steal_split(&mut victim, 100).expect("split should happen");
        assert_eq!(
            victim,
            Segment {
                start: 0,
                end: 600,
                done: 200
            }
        );
        assert_eq!(
            new,
            Segment {
                start: 600,
                end: 1000,
                done: 0
            }
        );
    }

    #[test]
    fn steal_split_refuses_small() {
        let mut victim = Segment {
            start: 0,
            end: 150,
            done: 0,
        };
        let before = victim.clone();
        assert!(steal_split(&mut victim, 100).is_none());
        assert_eq!(victim, before);
    }
}
