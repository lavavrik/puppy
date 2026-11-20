//! Restoring the stored downloads when the manager starts.

use super::{Effects, Entry, Inner};
use crate::store::{PersistedState, StoredState};
use puppy_core::engine::{AddOptions, StartMode};
use puppy_core::types::{AddRequest, DownloadId, DownloadState};
use std::collections::HashSet;
use std::path::PathBuf;

/// A download to hand back to its engine, paused (local checks only, no network).
struct ToRestore {
    id: DownloadId,
    engine: usize,
    req: AddRequest,
    path: PathBuf,
    limit: Option<u64>,
}

impl Inner {
    /// Rebuilds the state from `persisted`:
    /// - Completed: the record only.
    /// - Queued: stays queued (the scheduler restores it from its path, if any, when it
    ///   gets a slot).
    /// - Paused/Failed/NeedsAttention with a path: restored into the engine paused (no
    ///   network) and keep their state, unless the local files show the download
    ///   completed or needing attention.
    /// - Active with a path: restored paused, then queued at the front of the queue (in
    ///   their original order); the scheduler resumes them, which validates the remote.
    /// - Active without a path (never got past the probe): queued.
    /// - Paused/Failed/NeedsAttention without a path: kept as they are.
    /// - No engine accepts the source: failed.
    ///
    /// Restore errors leave the download failed.
    pub(super) async fn restore(&self, persisted: PersistedState) {
        let mut to_restore = Vec::new();
        {
            let mut st = self.lock();
            let mut seen = HashSet::new();
            let mut front = Vec::new();
            let mut rest = Vec::new();
            let known: HashSet<DownloadId> = persisted.downloads.iter().map(|d| d.id).collect();
            // The stored order first, then any download it misses.
            let order = persisted
                .order
                .iter()
                .copied()
                .filter(|id| known.contains(id))
                .chain(persisted.downloads.iter().map(|d| d.id))
                .collect::<Vec<_>>();
            let mut by_id: std::collections::HashMap<_, _> =
                persisted.downloads.into_iter().map(|d| (d.id, d)).collect();
            for id in order {
                if !seen.insert(id) {
                    continue;
                }
                let Some(rec) = by_id.remove(&id) else {
                    continue;
                };
                st.next_id = st.next_id.max(id + 1);
                let engine = self
                    .engines
                    .iter()
                    .position(|e| e.accepts(&rec.request.source));
                let path = rec.path.clone();
                let state = match (&rec.state, engine, path) {
                    (StoredState::Completed, _, _) => DownloadState::Completed,
                    (_, None, _) => DownloadState::Failed {
                        reason: "unsupported source".into(),
                    },
                    (StoredState::Queued, ..) => DownloadState::Queued,
                    (StoredState::Active, Some(engine), Some(path)) => {
                        to_restore.push(ToRestore {
                            id,
                            engine,
                            req: rec.request.clone(),
                            path,
                            limit: rec.limit,
                        });
                        DownloadState::Queued
                    }
                    (StoredState::Active, _, None) => DownloadState::Queued,
                    (stored, Some(engine), Some(path)) => {
                        to_restore.push(ToRestore {
                            id,
                            engine,
                            req: rec.request.clone(),
                            path,
                            limit: rec.limit,
                        });
                        live_state(stored)
                    }
                    (stored, Some(_), None) => live_state(stored),
                };
                if matches!(rec.state, StoredState::Active) && rec.path.is_some() {
                    front.push(id);
                } else {
                    rest.push(id);
                }
                st.entries.insert(
                    id,
                    Entry {
                        rec,
                        state,
                        engine,
                        held: false,
                        job: None,
                    },
                );
            }
            front.extend(rest);
            st.order = front;
        }
        for r in to_restore {
            let opts = AddOptions {
                start_paused: true,
                limit: r.limit,
            };
            let mode = StartMode::Restore { path: r.path };
            let result = self.engines[r.engine].add(r.id, r.req, mode, opts).await;
            let mut fx = Effects::default();
            {
                let mut st = self.lock();
                let Some(e) = st.entries.get_mut(&r.id) else {
                    continue;
                };
                match result {
                    Ok(added) => {
                        e.held = true;
                        e.rec.name = Some(added.name);
                        e.rec.path = Some(added.path);
                        e.rec.total_bytes = added.total_bytes;
                        if let Some(status) = self.engines[r.engine].status(r.id) {
                            Self::record_status(e, &status);
                            let next = match status.state {
                                s @ DownloadState::Completed => Some(s),
                                // One already needing attention keeps its stored reason.
                                s @ DownloadState::NeedsAttention { .. }
                                    if !matches!(e.state, DownloadState::NeedsAttention { .. }) =>
                                {
                                    Some(s)
                                }
                                _ => None,
                            };
                            if let Some(next) = next {
                                self.transition(r.id, e, next, &mut fx);
                            }
                        }
                    }
                    Err(err) => {
                        tracing::warn!(id = r.id, "restoring failed: {err}");
                        e.state = DownloadState::Failed {
                            reason: format!("restoring failed: {err}"),
                        };
                    }
                }
            }
            self.apply(fx);
        }
    }
}

/// The live state for a stored paused/failed/needs-attention download.
fn live_state(stored: &StoredState) -> DownloadState {
    match stored {
        StoredState::Failed { reason } => DownloadState::Failed {
            reason: reason.clone(),
        },
        StoredState::NeedsAttention { reason } => DownloadState::NeedsAttention {
            reason: reason.clone(),
        },
        StoredState::Queued | StoredState::Active => DownloadState::Queued,
        StoredState::Paused => DownloadState::Paused,
        StoredState::Completed => DownloadState::Completed,
    }
}
