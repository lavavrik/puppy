//! The engine's poll task: every 500 ms, maps each torrent's state, broadcasts
//! `StateChanged` on change and `Completed` once, and stops seeding at completion.

use std::sync::Weak;
use std::time::Duration;

use puppy_core::engine::EngineEvent;
use puppy_core::types::DownloadState;
use tokio_util::sync::CancellationToken;

use crate::engine::{Shared, Slot};

pub(crate) const POLL_INTERVAL: Duration = Duration::from_millis(500);

/// Runs until `stop` is cancelled or the engine is gone.
pub(crate) async fn run(shared: Weak<Shared>, stop: CancellationToken) {
    let mut tick = tokio::time::interval(POLL_INTERVAL);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            _ = stop.cancelled() => return,
            _ = tick.tick() => {}
        }
        let Some(shared) = shared.upgrade() else {
            return;
        };
        poll_once(&shared).await;
    }
}

/// One round: diff states, stop finished torrents, then broadcast.
pub(crate) async fn poll_once(shared: &Shared) {
    let mut events = Vec::new();
    let mut to_stop = Vec::new();
    {
        let mut map = shared.lock();
        for (&id, e) in map.iter_mut() {
            let Some(status) = shared.entry_status(e) else {
                continue;
            };
            if e.last_state.as_ref() != Some(&status.state) {
                e.last_state = Some(status.state.clone());
                events.push(EngineEvent::StateChanged {
                    id,
                    state: status.state.clone(),
                });
            }
            if status.state == DownloadState::Completed && !e.completed_sent {
                e.completed_sent = true;
                events.push(EngineEvent::Completed { id });
                // Seeding policy v1: stop at completion (seed_ratio 0).
                if shared.opts.seed_ratio <= 0.0
                    && let Slot::Managed(h) = &e.slot
                {
                    to_stop.push((id, h.clone()));
                }
            }
        }
    }
    // Paused before `Completed` goes out: once it is seen, nothing is uploaded anymore.
    for (id, h) in to_stop {
        if let Err(e) = shared.pause_managed(&h).await {
            tracing::warn!(id, "stopping a completed torrent: {e}");
        }
    }
    for ev in events {
        let _ = shared.events.send(ev);
    }
}
