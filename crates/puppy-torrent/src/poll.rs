//! The engine's poll task: every 500 ms, maps each torrent's state, broadcasts
//! `StateChanged` on change and `Completed` once, and stops seeding at completion.

use std::sync::Weak;
use std::time::Duration;

use puppy_core::engine::EngineEvent;
use puppy_core::types::DownloadState;
use tokio_util::sync::CancellationToken;

use crate::engine::{Shared, Slot, View};

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

/// One round: diff states, stop finished torrents, then broadcast. librqbit's stats are
/// read outside the torrents lock.
pub(crate) async fn poll_once(shared: &Shared) {
    let views: Vec<_> = shared
        .lock()
        .iter()
        .filter_map(|(&id, e)| Shared::view(e).map(|v| (id, v)))
        .collect();
    let handles: Vec<_> = views
        .iter()
        .filter_map(|(id, v)| match v {
            View::Managed { handle, .. } => Some((*id, handle.clone())),
            View::Pending(_) => None,
        })
        .collect();
    for (id, h) in handles {
        if let Err(e) = shared.unstick(id, &h).await {
            tracing::warn!(id, "restarting a resumed torrent: {e}");
        }
    }
    let statuses: Vec<_> = views.into_iter().map(|(id, v)| (id, v.status())).collect();
    // After the statuses (a completion seen now has all its writes counted), before
    // finished torrents are stopped below.
    shared.note_writes();

    let mut events = Vec::new();
    let mut to_stop = Vec::new();
    let mut to_persist = Vec::new();
    {
        let mut map = shared.lock();
        for (id, status) in statuses {
            // Removed meanwhile: nothing to report.
            let Some(e) = map.get_mut(&id) else {
                continue;
            };
            if e.selected.is_some() && status.state == DownloadState::Completed && !e.completed_sent
            {
                to_persist.push(id);
            }
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
                // Seeding policy v1: stop at completion (seed_ratio 0; above 0 is
                // reserved and currently means "seed until removed").
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
    shared.stamp_placeholders();
    for id in to_persist {
        shared.persist_choice(id);
    }
    for ev in events {
        let _ = shared.events.send(ev);
    }
}
