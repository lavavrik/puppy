//! Token-bucket rate limiter. Each chunk is acquired against both the
//! per-download bucket and the engine-global bucket.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::Notify;
use tokio::time::Instant;

const MIN_BURST: f64 = 16.0 * 1024.0;

struct State {
    rate: Option<u64>,
    /// May go negative (debt) when a request exceeds the burst capacity.
    tokens: f64,
    last: Instant,
}

struct Inner {
    state: Mutex<State>,
    changed: Notify,
}

#[derive(Clone)]
pub struct TokenBucket(Arc<Inner>);

fn capacity(rate: u64) -> f64 {
    (rate as f64 / 4.0).max(MIN_BURST)
}

impl TokenBucket {
    /// `None` means unlimited. A limited bucket starts empty.
    pub fn new(bps: Option<u64>) -> Self {
        let bps = bps.filter(|&r| r > 0);
        TokenBucket(Arc::new(Inner {
            state: Mutex::new(State {
                rate: bps,
                tokens: 0.0,
                last: Instant::now(),
            }),
            changed: Notify::new(),
        }))
    }

    pub fn set_rate(&self, bps: Option<u64>) {
        let bps = bps.filter(|&r| r > 0);
        {
            let mut s = self.0.state.lock().unwrap();
            refill(&mut s);
            s.rate = bps;
            match bps {
                Some(r) => s.tokens = s.tokens.min(capacity(r)),
                None => s.tokens = 0.0,
            }
        }
        self.0.changed.notify_waiters();
    }

    pub async fn acquire(&self, n: u64) {
        loop {
            let notified = self.0.changed.notified();
            tokio::pin!(notified);
            // Register before inspecting state so a concurrent set_rate is not missed.
            notified.as_mut().enable();
            let wait = {
                let mut s = self.0.state.lock().unwrap();
                let Some(rate) = s.rate else { return };
                refill(&mut s);
                // Requests larger than the burst only need a full burst; the
                // remainder is taken as debt that later callers wait out.
                let need = (n as f64).min(capacity(rate));
                if s.tokens >= need {
                    s.tokens -= n as f64;
                    return;
                }
                Duration::from_secs_f64((need - s.tokens) / rate as f64)
            };
            tokio::select! {
                _ = tokio::time::sleep(wait) => {}
                _ = &mut notified => {}
            }
        }
    }
}

fn refill(s: &mut State) {
    let now = Instant::now();
    if let Some(rate) = s.rate {
        let dt = now.saturating_duration_since(s.last).as_secs_f64();
        s.tokens = (s.tokens + dt * rate as f64).min(capacity(rate));
    }
    s.last = now;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn unlimited_never_waits() {
        let b = TokenBucket::new(None);
        let t = Instant::now();
        for _ in 0..100 {
            b.acquire(1_000_000).await;
        }
        assert_eq!(t.elapsed(), Duration::ZERO);
    }

    #[tokio::test(start_paused = true)]
    async fn limits_rate() {
        let b = TokenBucket::new(Some(1000));
        let t = Instant::now();
        for _ in 0..10 {
            b.acquire(500).await;
        }
        let e = t.elapsed().as_secs_f64();
        assert!((3.75..=5.25).contains(&e), "elapsed {e}");
    }

    #[tokio::test(start_paused = true)]
    async fn zero_rate_is_unlimited() {
        let t = Instant::now();
        let b = TokenBucket::new(Some(0));
        b.acquire(100_000_000).await;
        let b = TokenBucket::new(Some(1000));
        b.set_rate(Some(0));
        b.acquire(100_000_000).await;
        assert_eq!(t.elapsed(), Duration::ZERO);
    }

    #[tokio::test(start_paused = true)]
    async fn oversized_acquire_completes() {
        let b = TokenBucket::new(Some(1000));
        let t = Instant::now();
        b.acquire(100_000).await;
        assert!(t.elapsed() < Duration::from_secs(30));
    }

    #[tokio::test(start_paused = true)]
    async fn set_rate_takes_effect() {
        let b = TokenBucket::new(Some(1000));
        let b2 = b.clone();
        let h = tokio::spawn(async move { b2.acquire(1_000_000).await });
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(!h.is_finished());
        let t = Instant::now();
        b.set_rate(None);
        h.await.unwrap();
        assert!(t.elapsed() < Duration::from_millis(50));
    }

    #[tokio::test(start_paused = true)]
    async fn concurrent_acquirers_both_progress() {
        let b = TokenBucket::new(Some(1000));
        let mk = |b: TokenBucket| {
            tokio::spawn(async move {
                for _ in 0..4 {
                    b.acquire(250).await;
                }
            })
        };
        let (x, y) = (mk(b.clone()), mk(b));
        x.await.unwrap();
        y.await.unwrap();
    }
}
