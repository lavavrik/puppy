//! The engine's poll task: every 500 ms, maps each torrent's state, broadcasts
//! `StateChanged` on change and `Completed` once, and runs seeding: starts it at
//! completion (or stops at once, both targets 0), advances the seeding time, stops it
//! once a target is met (`SeedingFinished`) and records the counters.

use std::sync::Weak;
use std::time::{Duration, Instant};

use puppy_core::engine::EngineEvent;
use puppy_core::types::DownloadState;
use tokio_util::sync::CancellationToken;

use crate::engine::{Shared, Slot, View};
use crate::seeding::{self, SeedPhase};

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

fn is_seeding(s: &DownloadState) -> bool {
    matches!(s, DownloadState::Seeding { .. })
}

/// Whether `next` is a change worth `StateChanged`: a seeding torrent's counters move
/// all the time; only its targets count.
fn changed(last: Option<&DownloadState>, next: &DownloadState) -> bool {
    match (last, next) {
        (
            Some(DownloadState::Seeding {
                ratio_target: r0,
                time_target_secs: t0,
                ..
            }),
            DownloadState::Seeding {
                ratio_target: r1,
                time_target_secs: t1,
                ..
            },
        ) => r0 != r1 || t0 != t1,
        (last, next) => last != Some(next),
    }
}

/// One round: diff states, run seeding, stop torrents done seeding, then broadcast.
/// librqbit's stats are read outside the torrents lock.
pub(crate) async fn poll_once(shared: &Shared) {
    let views: Vec<_> = {
        let map = shared.lock();
        map.iter()
            .filter_map(|(&id, e)| shared.view(e).map(|v| (id, v)))
            .collect()
    };
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
    let observed: Vec<_> = views
        .into_iter()
        .map(|(id, v)| {
            let (st, obs) = v.observe();
            (id, st, obs)
        })
        .collect();
    // After the statuses (a completion seen now has all its writes counted), before
    // finished torrents are stopped below.
    shared.note_writes();

    let now = Instant::now();
    let mut events = Vec::new();
    let mut to_stop = Vec::new();
    let mut to_persist = Vec::new();
    let mut records = Vec::new();
    {
        let mut map = shared.lock();
        for (id, mut status, obs) in observed {
            // Removed meanwhile: nothing to report.
            let Some(e) = map.get_mut(&id) else {
                continue;
            };
            if let Some(o) = &obs {
                e.seed.observe_upload(o.uploaded);
            }
            let mut record = false;
            let mut stop = false;
            let mut finished_seeding = false;
            let complete = matches!(
                status.state,
                DownloadState::Completed | DownloadState::Seeding { .. }
            );
            let newly_complete = complete && !e.completed_sent;
            if newly_complete {
                e.completed_sent = true;
                if e.selected.is_some() {
                    to_persist.push(id);
                }
                if e.seed.phase == SeedPhase::Downloading {
                    let seed = is_seeding(&status.state);
                    e.seed.complete(seed, seeding::now_unix());
                    // Both targets 0: paused before `Completed` goes out, so nothing is
                    // uploaded once it is seen.
                    stop = !seed;
                    record = true;
                }
            }
            if e.seed.phase == SeedPhase::Seeding {
                let seeding_now = is_seeding(&status.state);
                e.seed
                    .tick(now, seeding_now && obs.is_some_and(|o| o.seeding_live));
                let targets = shared.targets(&e.seed.policy);
                let total = status.total_bytes.unwrap_or(0);
                let ratio = seeding::ratio(e.seed.uploaded(), total);
                if seeding_now && targets.met(ratio, e.seed.seeded_secs()) {
                    e.seed.stop();
                    stop = true;
                    finished_seeding = true;
                    record = true;
                    status.state = DownloadState::Completed;
                } else if e.seed.persist_due(now) {
                    record = true;
                }
            }
            if e.seed.finish_pending {
                e.seed.finish_pending = false;
                finished_seeding = true;
                record = true;
            }
            if stop && let Slot::Managed(h) = &e.slot {
                to_stop.push((id, h.clone()));
            }
            if record {
                records.push((id, e.seed.record(now)));
            }
            if changed(e.last_state.as_ref(), &status.state) {
                e.last_state = Some(status.state.clone());
                events.push(EngineEvent::StateChanged {
                    id,
                    state: status.state.clone(),
                });
            }
            if newly_complete {
                events.push(EngineEvent::Completed { id });
            }
            if finished_seeding {
                events.push(EngineEvent::SeedingFinished { id });
            }
        }
    }
    for (id, h) in to_stop {
        if let Err(e) = shared.pause_managed(&h).await {
            tracing::warn!(id, "stopping a completed torrent: {e}");
        }
    }
    shared.stamp_placeholders();
    for id in to_persist {
        shared.persist_choice(id);
    }
    for (id, rec) in records {
        shared.write_seed(id, &rec);
    }
    for ev in events {
        let _ = shared.events.send(ev);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seeding_counters_are_not_a_change() {
        let s = |secs, target| DownloadState::Seeding {
            uploaded: secs * 10,
            ratio: 0.1,
            seeded_secs: secs,
            ratio_target: Some(1.0),
            time_target_secs: target,
        };
        assert!(!changed(Some(&s(1, Some(60))), &s(2, Some(60))));
        assert!(changed(Some(&s(1, Some(60))), &s(1, Some(30))), "targets");
        assert!(changed(None, &s(1, None)));
        assert!(changed(Some(&s(1, None)), &DownloadState::Completed));
        assert!(!changed(
            Some(&DownloadState::Completed),
            &DownloadState::Completed
        ));
    }
}
