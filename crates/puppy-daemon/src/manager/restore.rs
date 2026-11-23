//! Restoring the stored downloads when the manager starts.

use super::schedule::{Job, JobKind};
use super::{Entry, Inner};
use crate::store::{PersistedState, StoredState};
use puppy_core::engine::{AddOptions, StartMode};
use puppy_core::types::{DownloadId, DownloadState, RequestContext};
use std::collections::HashSet;

impl Inner {
    /// Rebuilds the state from `persisted` and returns the restore jobs to spawn:
    /// - Completed: the record only.
    /// - Queued: stays queued (the scheduler restores it from its path, if any, when it
    ///   gets a slot).
    /// - Paused/Failed/NeedsAttention with a path: restored into the engine paused (no
    ///   network) by a job, keeping their state, unless the local files show the
    ///   download completed or needing attention.
    /// - Active with a path: queued at the front of the queue (in their original order)
    ///   and restored paused by a job; the scheduler then resumes them, which validates
    ///   the remote. While restoring they hold a slot, so later queued downloads do not
    ///   overtake them.
    /// - Seeding (a torrent): restored into the engine by a job, which has it go on
    ///   seeding (live, holding no slot) unless its targets are met already (then
    ///   `Completed`) or its data fails the check (then `NeedsAttention`).
    /// - ChoosingFiles (a torrent): restored by a job, still choosing (queued at the
    ///   front if its engine has its choice already); without a path: queued.
    /// - Active without a path (never got past the probe): queued.
    /// - Paused/Failed/NeedsAttention without a path: kept as they are.
    /// - No engine accepts the source: failed.
    ///
    /// Restore jobs run in parallel; until one returns its download shows the stored
    /// state and commands follow the usual job rules. Restore errors leave the download
    /// failed (retried automatically if the cause is transient).
    pub(super) fn restore(&self, persisted: PersistedState) -> Vec<Job> {
        let mut jobs = Vec::new();
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
            let Some(mut rec) = by_id.remove(&id) else {
                continue;
            };
            st.next_id = st.next_id.max(id + 1);
            if matches!(rec.state, StoredState::Completed) {
                // Older state files kept the context of finished downloads.
                rec.request.context = RequestContext::default();
            }
            let engine = self
                .engines
                .iter()
                .position(|e| e.accepts(&rec.request.source));
            // Handed back to its engine now (Queued ones wait for the scheduler).
            let restore = engine.is_some()
                && rec.path.is_some()
                && !matches!(rec.state, StoredState::Completed | StoredState::Queued);
            let state = match (&rec.state, engine) {
                (StoredState::Completed, _) => DownloadState::Completed,
                // Nothing to seed from.
                (StoredState::Seeding, _) if !restore => DownloadState::Completed,
                // Never got its contents recorded: added again (and asked again).
                (StoredState::ChoosingFiles, Some(_)) if !restore => DownloadState::Queued,
                (_, None) => DownloadState::Failed {
                    reason: "unsupported source".into(),
                    transient: false,
                },
                (stored, Some(_)) => live_state(stored),
            };
            if matches!(rec.state, StoredState::Active) && restore {
                front.push(id);
            } else {
                rest.push(id);
            }
            let kind = restore.then(|| JobKind::Restore {
                req: Box::new(rec.request.clone()),
                mode: StartMode::Restore {
                    path: rec.path.clone().expect("checked above"),
                },
                opts: AddOptions {
                    start_paused: true,
                    limit: rec.limit,
                },
            });
            let was_choosing = matches!(rec.state, StoredState::ChoosingFiles);
            let mut entry = Entry::new(rec, state, engine);
            entry.rec.choose_files_sent |= was_choosing;
            if super::is_transient_failure(&entry.state) {
                // Failed transiently before the restart: retried like any other.
                self.arm_retry(id, &mut entry);
            }
            if let Some(kind) = kind
                && let Some(job) = Self::begin_restore(id, &mut entry, kind)
            {
                jobs.push(job);
            }
            st.entries.insert(id, entry);
        }
        front.extend(rest);
        st.order = front;
        jobs
    }
}

/// The live state for a stored download that is not completed.
fn live_state(stored: &StoredState) -> DownloadState {
    match stored {
        StoredState::Failed { reason, transient } => DownloadState::Failed {
            reason: reason.clone(),
            transient: *transient,
        },
        StoredState::NeedsAttention { reason } => DownloadState::NeedsAttention {
            reason: reason.clone(),
        },
        // Active ones are queued: the scheduler resumes them.
        StoredState::Queued | StoredState::Active => DownloadState::Queued,
        StoredState::Paused => DownloadState::Paused,
        StoredState::Completed => DownloadState::Completed,
        StoredState::ChoosingFiles => DownloadState::ChoosingFiles,
        StoredState::Seeding => super::seeding_placeholder(),
    }
}
