//! librqbit torrent stats → puppy's `DownloadStatus` / `DownloadState` (pure).

use std::path::PathBuf;

use librqbit::{TorrentStats, TorrentStatsState};
use puppy_core::types::{DownloadState, DownloadStatus};

/// librqbit's state of a managed torrent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Phase {
    /// Checking existing data before starting (`paused`: stays paused afterwards).
    Initializing {
        paused: bool,
    },
    Live,
    Paused,
    Error,
}

/// The parts of librqbit's `TorrentStats` puppy uses, in plain types.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Stats {
    pub phase: Phase,
    pub error: Option<String>,
    /// Bytes downloaded and verified.
    pub progress_bytes: u64,
    pub total_bytes: u64,
    pub finished: bool,
    pub down_bps: u64,
    pub up_bps: u64,
    /// Connected peers (live torrents only).
    pub peers: u32,
}

impl From<&TorrentStats> for Stats {
    fn from(s: &TorrentStats) -> Self {
        let phase = match s.state {
            TorrentStatsState::Initializing { paused } => Phase::Initializing { paused },
            TorrentStatsState::Live => Phase::Live,
            TorrentStatsState::Paused => Phase::Paused,
            TorrentStatsState::Error => Phase::Error,
        };
        let (down_bps, up_bps, peers) = match &s.live {
            Some(l) => (
                l.download_speed.as_bytes(),
                l.upload_speed.as_bytes(),
                l.snapshot.peer_stats.live,
            ),
            None => (0, 0, 0),
        };
        Stats {
            phase,
            error: s.error.clone(),
            progress_bytes: s.progress_bytes,
            total_bytes: s.total_bytes,
            finished: s.finished,
            down_bps,
            up_bps,
            peers,
        }
    }
}

/// The puppy state of a managed torrent. A finished torrent is `Completed` whether
/// librqbit still has it live or (stopped seeding) paused.
pub(crate) fn state(s: &Stats) -> DownloadState {
    match s.phase {
        Phase::Error => {
            let reason = s
                .error
                .clone()
                .unwrap_or_else(|| "torrent error".to_string());
            let transient = is_transient_error(&reason);
            DownloadState::Failed { reason, transient }
        }
        _ if s.finished => DownloadState::Completed,
        Phase::Initializing { paused: true } | Phase::Paused => DownloadState::Paused,
        Phase::Initializing { paused: false } => DownloadState::Starting,
        Phase::Live => DownloadState::Active,
    }
}

/// The full status of a managed torrent.
pub(crate) fn status(s: &Stats, name: String, path: PathBuf) -> DownloadStatus {
    let state = state(s);
    let active = state == DownloadState::Active;
    // While initializing, librqbit's progress counts bytes *checked*, not bytes had.
    let done_bytes = match s.phase {
        Phase::Initializing { .. } => 0,
        _ => s.progress_bytes.min(s.total_bytes),
    };
    let down_bps = if active { s.down_bps } else { 0 };
    let eta_secs =
        (down_bps > 0).then(|| s.total_bytes.saturating_sub(done_bytes).div_ceil(down_bps));
    DownloadStatus {
        state,
        name,
        path,
        total_bytes: Some(s.total_bytes),
        done_bytes,
        down_bps,
        up_bps: if active { s.up_bps } else { 0 },
        eta_secs,
        peers: Some(if s.phase == Phase::Live { s.peers } else { 0 }),
    }
}

/// The status of a magnet whose metadata is not known yet (or never resolved).
pub(crate) fn pending_status(state: DownloadState, name: String, path: PathBuf) -> DownloadStatus {
    DownloadStatus {
        state,
        name,
        path,
        total_bytes: None,
        done_bytes: 0,
        down_bps: 0,
        up_bps: 0,
        eta_secs: None,
        peers: Some(0),
    }
}

/// Best-effort classification of a librqbit error message (librqbit reports errors as
/// `anyhow` text only): network trouble (peers, trackers, DHT, timeouts, metadata that
/// could not be fetched) may go away by itself ⇒ transient; storage (disk full,
/// permissions, files) and parse/validation errors would fail again as is ⇒ not
/// transient. Storage wins when both kinds are mentioned; unknown ⇒ not transient.
pub(crate) fn is_transient_error(msg: &str) -> bool {
    const STORAGE: &[&str] = &[
        "no space",
        "disk",
        "permission denied",
        "read-only",
        "storage",
        "file",
        "directory",
        "decod",
        "invalid",
        "path traversal",
    ];
    const NETWORK: &[&str] = &[
        "peer",
        "tracker",
        "dht",
        "network",
        "connect",
        "connection",
        "timed out",
        "timeout",
        "metainfo",
        "metadata",
        "address stream",
        "resolve",
    ];
    let m = msg.to_lowercase();
    if STORAGE.iter().any(|k| m.contains(k)) {
        return false;
    }
    NETWORK.iter().any(|k| m.contains(k))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stats(phase: Phase) -> Stats {
        Stats {
            phase,
            error: None,
            progress_bytes: 300,
            total_bytes: 1000,
            finished: false,
            down_bps: 100,
            up_bps: 7,
            peers: 3,
        }
    }

    #[test]
    fn status_across_states() {
        let path = PathBuf::from("/dl/a");
        let st = |s: &Stats| status(s, "a".into(), path.clone());

        let init = st(&stats(Phase::Initializing { paused: false }));
        assert_eq!(init.state, DownloadState::Starting);
        assert_eq!(init.done_bytes, 0, "checked bytes are not progress");
        assert_eq!(init.total_bytes, Some(1000));
        assert_eq!((init.down_bps, init.up_bps, init.eta_secs), (0, 0, None));
        assert_eq!(init.peers, Some(0));

        let init_paused = st(&stats(Phase::Initializing { paused: true }));
        assert_eq!(init_paused.state, DownloadState::Paused);

        let live = st(&stats(Phase::Live));
        assert_eq!(live.state, DownloadState::Active);
        assert_eq!(live.done_bytes, 300);
        assert_eq!(live.total_bytes, Some(1000));
        assert_eq!((live.down_bps, live.up_bps), (100, 7));
        assert_eq!(live.eta_secs, Some(7), "700 bytes at 100 B/s");
        assert_eq!(live.peers, Some(3));
        assert_eq!(live.name, "a");
        assert_eq!(live.path, path);

        let stalled = st(&Stats {
            down_bps: 0,
            ..stats(Phase::Live)
        });
        assert_eq!(stalled.eta_secs, None);

        let paused = st(&stats(Phase::Paused));
        assert_eq!(paused.state, DownloadState::Paused);
        assert_eq!(paused.done_bytes, 300);
        assert_eq!(
            (paused.down_bps, paused.up_bps, paused.eta_secs),
            (0, 0, None)
        );

        let done = Stats {
            finished: true,
            progress_bytes: 1000,
            ..stats(Phase::Live)
        };
        assert_eq!(st(&done).state, DownloadState::Completed);
        assert_eq!(st(&done).done_bytes, 1000);
        assert_eq!(st(&done).up_bps, 0, "not reported as seeding");
        // Stopped seeding (paused by the engine): still Completed.
        let stopped = Stats {
            phase: Phase::Paused,
            ..done.clone()
        };
        assert_eq!(st(&stopped).state, DownloadState::Completed);

        let storage = Stats {
            error: Some("error writing to file: No space left on device".into()),
            ..stats(Phase::Error)
        };
        assert_eq!(
            st(&storage).state,
            DownloadState::Failed {
                reason: "error writing to file: No space left on device".into(),
                transient: false
            }
        );
        let unknown = st(&stats(Phase::Error));
        assert!(matches!(
            unknown.state,
            DownloadState::Failed {
                transient: false,
                ..
            }
        ));
        // An error wins over `finished`.
        let errored_done = Stats {
            finished: true,
            ..stats(Phase::Error)
        };
        assert!(matches!(
            st(&errored_done).state,
            DownloadState::Failed { .. }
        ));
    }

    #[test]
    fn pending() {
        let p = pending_status(
            DownloadState::Starting,
            "magnet 01234567".into(),
            "/dl/x".into(),
        );
        assert_eq!(p.state, DownloadState::Starting);
        assert_eq!((p.total_bytes, p.done_bytes, p.eta_secs), (None, 0, None));
    }

    #[test]
    fn error_classification() {
        for transient in [
            "input address stream exhausted, no way to discover torrent metainfo",
            "no known way to resolve peers (no DHT, no trackers, no initial_peers)",
            "error connecting to tracker: connection refused",
            "timed out",
        ] {
            assert!(is_transient_error(transient), "{transient}");
        }
        for permanent in [
            "error opening file \"/dl/a\": Permission denied (os error 13)",
            "No space left on device (os error 28)",
            "error decoding torrent",
            "path traversal in torrent name detected",
            "something unexpected",
        ] {
            assert!(!is_transient_error(permanent), "{permanent}");
        }
    }
}
