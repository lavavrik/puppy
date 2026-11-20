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
    /// Bytes had per file (librqbit's file indices; empty while initializing).
    pub file_progress: Vec<u64>,
    /// Bytes uploaded by the current live session (0 when not live).
    pub uploaded: u64,
}

impl Stats {
    /// Counts only the selected files (`(librqbit file index, length)`) in the sizes:
    /// librqbit's own totals are whole pieces, including the parts of pieces that lie
    /// in unselected files.
    pub fn restrict(&mut self, selected: &[(usize, u64)]) {
        let total: u64 = selected.iter().map(|&(_, len)| len).sum();
        let done: u64 = if self.finished {
            total
        } else {
            selected
                .iter()
                .map(|&(i, len)| self.file_progress.get(i).copied().unwrap_or(0).min(len))
                .sum()
        };
        self.total_bytes = total;
        // While initializing, progress is bytes checked (shown as 0 by `status`).
        if !matches!(self.phase, Phase::Initializing { .. }) {
            self.progress_bytes = done;
        }
    }
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
            file_progress: s.file_progress.clone(),
            uploaded: s.uploaded_bytes,
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
        note: None,
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
        note: None,
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
    // No peer source at all (DHT off, no trackers, no peers): retrying cannot help.
    if m.contains("no known way to resolve peers") {
        return false;
    }
    if STORAGE.iter().any(|k| m.contains(k)) {
        return false;
    }
    NETWORK.iter().any(|k| m.contains(k))
}

/// `(peers, seeds, leechers)` of a live torrent: connected peers, how many of them have
/// every piece, and the rest. All 0 when the torrent is not live.
pub(crate) fn peer_counts(s: &TorrentStats) -> (u32, u32, u32) {
    match &s.live {
        Some(l) => {
            let live = l.snapshot.peer_stats.live;
            let seeds = l.snapshot.peer_stats.live_seeders.min(live);
            (live, seeds, live - seeds)
        }
        None => (0, 0, 0),
    }
}

/// A piece map synthesised from per-file progress: the fallback before a restored
/// torrent's exact map is known (see [`fallback_map`]). MSB first: piece 0 is bit 7 of
/// byte 0.
///
/// Each file's progress is taken as an in-order prefix of that file (clamped to its
/// length); a piece is set iff every byte it covers (possibly across several files) lies
/// inside those prefixes. An approximation: a file filled out of order shows fewer
/// pieces than it really has. Missing `file_progress` entries count as 0.
pub(crate) fn have_bitfield(
    piece_len: u64,
    total: u64,
    file_lengths: &[u64],
    file_progress: &[u64],
) -> Vec<u8> {
    if piece_len == 0 {
        return Vec::new();
    }
    let pieces = total.div_ceil(piece_len);
    let mut out = vec![0u8; pieces.div_ceil(8) as usize];
    // Have-ranges `[start, end)` in torrent offsets, ascending; adjacent ones merge.
    let mut have: Vec<(u64, u64)> = Vec::new();
    let mut offset = 0u64;
    for (i, &len) in file_lengths.iter().enumerate() {
        let got = file_progress.get(i).copied().unwrap_or(0).min(len);
        if got > 0 {
            match have.last_mut() {
                Some(last) if last.1 == offset => last.1 = offset + got,
                _ => have.push((offset, offset + got)),
            }
        }
        offset += len;
    }
    // A piece is had iff one range covers it entirely.
    let mut r = 0;
    for piece in 0..pieces {
        let start = piece * piece_len;
        let end = (start + piece_len).min(total);
        while r < have.len() && have[r].1 < end {
            r += 1;
        }
        if r < have.len() && have[r].0 <= start {
            out[(piece / 8) as usize] |= 0x80 >> (piece % 8);
        }
    }
    out
}

/// librqbit's have-bitfield (MSB first, as `api_dump_haves` returns it) trimmed to
/// `piece_count` bits: one bit per piece, byte padded, padding bits clear.
pub(crate) fn exact_bitfield(raw: &[u8], piece_count: u32) -> Vec<u8> {
    let n = piece_count as usize;
    let mut out = raw.get(..n.div_ceil(8)).unwrap_or(raw).to_vec();
    out.resize(n.div_ceil(8), 0);
    if !n.is_multiple_of(8)
        && let Some(last) = out.last_mut()
    {
        *last &= !(0xFFu8 >> (n % 8));
    }
    out
}

/// The pieces of a completed torrent: every piece that overlaps a wanted file
/// (`selected`: librqbit file indices; `None`: every file). MSB first.
pub(crate) fn selected_pieces(
    piece_len: u64,
    total: u64,
    file_lengths: &[u64],
    selected: Option<&[usize]>,
) -> Vec<u8> {
    if piece_len == 0 {
        return Vec::new();
    }
    let pieces = total.div_ceil(piece_len);
    let mut out = vec![0u8; pieces.div_ceil(8) as usize];
    let mut offset = 0u64;
    for (i, &len) in file_lengths.iter().enumerate() {
        let wanted = selected.is_none_or(|s| s.contains(&i));
        if wanted && len > 0 {
            let first = offset / piece_len;
            let last = (offset + len - 1) / piece_len;
            for p in first..=last.min(pieces.saturating_sub(1)) {
                out[(p / 8) as usize] |= 0x80 >> (p % 8);
            }
        }
        offset += len;
    }
    out
}

/// The piece map when librqbit cannot give its own (not live, not paused: checking,
/// failed): a completed torrent (`completed`: its [`selected_pieces`]) has those pieces
/// and whatever the last exact map had; else the last exact map; else (after a restart,
/// before it went live) the per-file synthesis.
pub(crate) fn fallback_map(
    last: Option<&[u8]>,
    completed: Option<Vec<u8>>,
    synth: impl FnOnce() -> Vec<u8>,
) -> Vec<u8> {
    match (completed, last) {
        (Some(mut done), last) => {
            for (d, l) in done.iter_mut().zip(last.unwrap_or_default()) {
                *d |= l;
            }
            done
        }
        (None, Some(last)) => last.to_vec(),
        (None, None) => synth(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn have_bitfield_from_file_progress() {
        // Single file, 10 bytes, pieces of 4: [0,4) [4,8) [8,10).
        let f = |p| have_bitfield(4, 10, &[10], &[p]);
        assert_eq!(f(0), vec![0b0000_0000]);
        assert_eq!(f(3), vec![0b0000_0000]);
        assert_eq!(f(4), vec![0b1000_0000]);
        assert_eq!(f(9), vec![0b1100_0000]);
        assert_eq!(f(10), vec![0b1110_0000], "last partial piece once complete");
        assert_eq!(f(99), vec![0b1110_0000], "progress above length is clamped");

        // Two files of 6 and 6 bytes, pieces of 4: piece 1 [4,8) spans the boundary.
        let m = |a, b| have_bitfield(4, 12, &[6, 6], &[a, b]);
        assert_eq!(
            m(6, 0),
            vec![0b1000_0000],
            "piece 1 needs file 2's first bytes"
        );
        assert_eq!(m(6, 2), vec![0b1100_0000]);
        assert_eq!(
            m(0, 6),
            vec![0b0010_0000],
            "piece 2 [8,12) lies in file 2 only"
        );
        assert_eq!(m(6, 6), vec![0b1110_0000]);
        assert_eq!(m(2, 2), vec![0]);
        // A gap in file 1 blocks the piece across the boundary.
        assert_eq!(m(4, 6), vec![0b1010_0000]);
        assert_eq!(m(5, 6), vec![0b1010_0000]);
    }

    #[test]
    fn have_bitfield_edge_cases() {
        // Zero-length files are skipped without breaking continuity.
        let b = have_bitfield(4, 8, &[4, 0, 0, 4], &[4, 0, 0, 4]);
        assert_eq!(b, vec![0b1100_0000]);
        let b = have_bitfield(4, 8, &[0, 4, 0, 4], &[0, 4, 0, 0]);
        assert_eq!(b, vec![0b1000_0000]);
        // Missing progress entries count as 0; no pieces for an empty torrent.
        assert_eq!(have_bitfield(4, 8, &[4, 4], &[4]), vec![0b1000_0000]);
        assert_eq!(have_bitfield(4, 0, &[], &[]), Vec::<u8>::new());
        assert_eq!(have_bitfield(0, 10, &[10], &[10]), Vec::<u8>::new());
        // More than 8 pieces: padding bits stay clear, order is MSB first.
        assert_eq!(have_bitfield(1, 10, &[10], &[9]), vec![0xFF, 0b1000_0000]);
        assert_eq!(have_bitfield(1, 10, &[10], &[10]), vec![0xFF, 0b1100_0000]);
        assert_eq!(have_bitfield(1, 10, &[10], &[3]), vec![0b1110_0000, 0]);
        // Piece length larger than the torrent: one piece.
        assert_eq!(have_bitfield(100, 10, &[10], &[10]), vec![0x80]);
        assert_eq!(have_bitfield(100, 10, &[10], &[9]), vec![0]);
    }

    #[test]
    fn exact_bitfield_is_trimmed() {
        assert_eq!(exact_bitfield(&[0xFF, 0xFF], 9), vec![0xFF, 0x80]);
        assert_eq!(exact_bitfield(&[0b1010_1111], 4), vec![0b1010_0000]);
        assert_eq!(exact_bitfield(&[0xF0], 8), vec![0xF0]);
        assert_eq!(exact_bitfield(&[], 3), vec![0], "short input padded");
        assert_eq!(exact_bitfield(&[0xFF], 0), Vec::<u8>::new());
    }

    #[test]
    fn selected_pieces_overlap_wanted_files() {
        // Files of 6, 6 and 4 bytes, pieces of 4: [0,4) [4,8) [8,12) [12,16).
        let l = [6, 6, 4];
        assert_eq!(selected_pieces(4, 16, &l, None), vec![0b1111_0000]);
        assert_eq!(
            selected_pieces(4, 16, &l, Some(&[0])),
            vec![0b1100_0000],
            "file 0 touches pieces 0 and 1"
        );
        assert_eq!(selected_pieces(4, 16, &l, Some(&[2])), vec![0b0001_0000]);
        assert_eq!(selected_pieces(4, 16, &l, Some(&[1])), vec![0b0110_0000]);
        // Empty files mark nothing.
        assert_eq!(selected_pieces(4, 8, &[4, 0, 4], Some(&[1])), vec![0]);
        assert_eq!(selected_pieces(0, 8, &[8], None), Vec::<u8>::new());
    }

    #[test]
    fn fallbacks_in_order() {
        let synth = || vec![0b1000_0000];
        // Completed: the selected pieces, plus what the last exact map had.
        assert_eq!(
            fallback_map(Some(&[0b0000_0001]), Some(vec![0b1110_0000]), synth),
            vec![0b1110_0001]
        );
        assert_eq!(
            fallback_map(None, Some(vec![0b1110_0000]), synth),
            vec![0b1110_0000]
        );
        // Not completed: the last exact map, else the synthesis.
        assert_eq!(
            fallback_map(Some(&[0b0100_0000]), None, synth),
            vec![0b0100_0000]
        );
        assert_eq!(fallback_map(None, None, synth), vec![0b1000_0000]);
    }

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
            file_progress: Vec::new(),
            uploaded: 0,
        }
    }

    #[test]
    fn restricted_sizes_count_selected_files_only() {
        let mut s = Stats {
            file_progress: vec![100, 50, 400],
            ..stats(Phase::Live)
        };
        // Files 0 (len 100) and 2 (len 500) selected.
        s.restrict(&[(0, 100), (2, 500)]);
        assert_eq!((s.total_bytes, s.progress_bytes), (600, 500));
        let st = status(&s, "a".into(), PathBuf::from("/a"));
        assert_eq!((st.total_bytes, st.done_bytes), (Some(600), 500));
        let mut done = Stats {
            finished: true,
            ..stats(Phase::Paused)
        };
        done.restrict(&[(1, 10)]);
        assert_eq!((done.total_bytes, done.progress_bytes), (10, 10));
        let mut init = stats(Phase::Initializing { paused: false });
        init.restrict(&[(0, 100)]);
        assert_eq!(init.total_bytes, 100);
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
            "fetching the torrent metadata: no known way to resolve peers (no DHT, no trackers, no initial_peers)",
        ] {
            assert!(!is_transient_error(permanent), "{permanent}");
        }
    }
}
