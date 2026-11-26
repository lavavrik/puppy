//! Completing a finalized download: hashing it against the request's checksum (if any)
//! off the async runtime, then `Completed` or `Failed`.
//!
//! A download with a checksum keeps its sidecar next to the finished file until the
//! check has passed, so a restart in between (or after a mismatch) finds it finalized
//! and verifies it again.

use super::files::remove_sidecar_best_effort;
use super::{Inner, RunEnd};
use crate::checksum::hash_file;
use crate::engine::EngineEvent;
use crate::http::resume::sidecar_path;
use crate::types::DownloadState;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio_util::sync::CancellationToken;

/// Verifies the finished file (when the request has a checksum) and records the end:
/// `Completed` (sidecar removed), `Failed` on a mismatch or read error (file and sidecar
/// kept), or `Stopped` if `cancel` fired meanwhile (the hashing thread has stopped and
/// closed the file by then).
pub(super) async fn complete(inner: &Arc<Inner>, cancel: &CancellationToken) -> RunEnd {
    if let Some(sum) = inner.request.checksum.clone() {
        inner.hashed.store(0, Ordering::Relaxed);
        inner.set_state(DownloadState::Verifying);
        let path = inner.lock().path.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let mut task = tokio::task::spawn_blocking({
            let (inner, stop, path) = (inner.clone(), stop.clone(), path.clone());
            move || hash_file(&path, sum.algo, &inner.hashed, &stop)
        });
        let joined = tokio::select! {
            r = &mut task => r,
            _ = cancel.cancelled() => {
                stop.store(true, Ordering::Relaxed);
                let _ = task.await;
                return RunEnd::Stopped;
            }
        };
        let failed = match joined {
            Ok(Ok(Some(actual))) if actual == sum.hex => None,
            Ok(Ok(Some(actual))) => {
                tracing::warn!(id = inner.id, path = %path.display(), "checksum mismatch");
                Some(sum.mismatch(&actual))
            }
            // Only stopped through `stop`, which only a cancel sets.
            Ok(Ok(None)) => return RunEnd::Stopped,
            Ok(Err(e)) => Some(format!("verifying the checksum failed: {e}")),
            Err(e) => Some(format!("verifying the checksum failed: {e}")),
        };
        if let Some(reason) = failed {
            inner.set_state(DownloadState::Failed {
                reason,
                transient: false,
            });
            return RunEnd::Finished;
        }
        let sidecar = sidecar_path(&inner.final_path);
        let _ = tokio::task::spawn_blocking(move || remove_sidecar_best_effort(&sidecar)).await;
    }
    inner.set_state(DownloadState::Completed);
    let _ = inner.events.send(EngineEvent::Completed { id: inner.id });
    RunEnd::Finished
}
