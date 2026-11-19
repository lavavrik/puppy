//! Exponentially weighted speed meter (tau = 2 s).

use std::time::Instant;

const TAU: f64 = 2.0;

pub struct SpeedMeter {
    rate: f64,
    last: Option<Instant>,
}

impl Default for SpeedMeter {
    fn default() -> Self {
        Self::new()
    }
}

impl SpeedMeter {
    pub fn new() -> Self {
        SpeedMeter {
            rate: 0.0,
            last: None,
        }
    }

    pub fn record(&mut self, bytes: u64, now: Instant) {
        match self.last {
            None => self.last = Some(now),
            Some(last) => {
                let dt = now.saturating_duration_since(last).as_secs_f64();
                if dt <= 0.0 {
                    // Same instant: fold into the rate as an impulse over a tiny window.
                    self.rate += bytes as f64 / TAU;
                    return;
                }
                let alpha = 1.0 - (-dt / TAU).exp();
                self.rate += alpha * (bytes as f64 / dt - self.rate);
                self.last = Some(now);
            }
        }
    }

    pub fn bps(&self, now: Instant) -> u64 {
        let Some(last) = self.last else { return 0 };
        let dt = now.saturating_duration_since(last).as_secs_f64();
        (self.rate * (-dt / TAU).exp()).round() as u64
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn meter_converges() {
        let base = Instant::now();
        let mut m = SpeedMeter::new();
        m.record(0, base);
        for i in 1..=100u32 {
            m.record(1000, base + Duration::from_millis(100) * i);
        }
        let end = base + Duration::from_secs(10);
        let v = m.bps(end) as f64;
        assert!((9_000.0..=11_000.0).contains(&v), "bps {v}");
        assert!(m.bps(end + Duration::from_secs(10)) < 500);
    }
}
