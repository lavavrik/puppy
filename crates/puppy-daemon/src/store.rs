//! Persistent daemon state (`state.json`): downloads, queue order, next id, runtime settings.
//!
//! The file holds request contexts (cookies/auth), so it is written atomically and privately.

use puppy_core::fsutil::atomic_write;
use puppy_core::types::{AddRequest, AttentionReason, DownloadId};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const CURRENT_VERSION: u32 = 1;
const DEBOUNCE: Duration = Duration::from_millis(300);

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct StoredDownload {
    pub id: DownloadId,
    pub request: AddRequest,
    pub limit: Option<u64>,
    pub state: StoredState,
    pub name: Option<String>,
    pub path: Option<PathBuf>,
    pub total_bytes: Option<u64>,
    pub created_unix: u64,
    pub finished_unix: Option<u64>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum StoredState {
    Queued,
    Active,
    Paused,
    Completed,
    Failed { reason: String },
    NeedsAttention { reason: AttentionReason },
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct RuntimeSettings {
    pub max_active: u16,
    pub global_down_limit: Option<u64>,
    pub global_up_limit: Option<u64>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct PersistedState {
    pub version: u32,
    pub next_id: DownloadId,
    pub order: Vec<DownloadId>,
    pub downloads: Vec<StoredDownload>,
    pub settings: Option<RuntimeSettings>,
}

impl Default for PersistedState {
    fn default() -> Self {
        Self {
            version: CURRENT_VERSION,
            next_id: 1,
            order: Vec::new(),
            downloads: Vec::new(),
            settings: None,
        }
    }
}

#[derive(Default)]
struct Pending {
    /// Latest state waiting to be written (latest wins).
    state: Option<PersistedState>,
    /// A background debounce task is alive.
    running: bool,
}

struct Inner {
    path: PathBuf,
    pending: Mutex<Pending>,
    /// Serializes writes: at most one in flight.
    write_lock: tokio::sync::Mutex<()>,
    writes: AtomicUsize,
}

/// Handle to the state file. Cheap to clone-share via `Arc` by the caller.
///
/// `save_soon` spawns a background task, so it must be called within a Tokio runtime.
pub struct Store {
    inner: Arc<Inner>,
}

impl Inner {
    /// Writes the pending state, if any. Caller must hold `write_lock`.
    /// On failure the state is put back (unless a newer one arrived) and the error returned.
    async fn write_pending(&self) -> std::io::Result<()> {
        let Some(st) = self.pending.lock().unwrap().state.take() else {
            return Ok(());
        };
        let res = self.write(&st).await;
        if res.is_err() {
            let mut p = self.pending.lock().unwrap();
            if p.state.is_none() {
                p.state = Some(st);
            }
        }
        res
    }

    async fn write(&self, st: &PersistedState) -> std::io::Result<()> {
        let bytes = serde_json::to_vec_pretty(st).map_err(std::io::Error::other)?;
        let res = atomic_write(&self.path, &bytes, true).await;
        if res.is_ok() {
            self.writes.fetch_add(1, Ordering::SeqCst);
        }
        res
    }

    async fn debounce_task(self: Arc<Self>) {
        loop {
            tokio::time::sleep(DEBOUNCE).await;
            let _g = self.write_lock.lock().await;
            if let Err(e) = self.write_pending().await {
                // State stays pending; the next save_soon/flush retries.
                tracing::warn!("failed to save {}: {e}", self.path.display());
                self.pending.lock().unwrap().running = false;
                return;
            }
            let mut p = self.pending.lock().unwrap();
            if p.state.is_none() {
                p.running = false;
                return;
            }
        }
    }
}

impl Store {
    pub fn new(path: PathBuf) -> Self {
        Self {
            inner: Arc::new(Inner {
                path,
                pending: Mutex::new(Pending::default()),
                write_lock: tokio::sync::Mutex::new(()),
                writes: AtomicUsize::new(0),
            }),
        }
    }

    /// Number of successful writes so far (test hook).
    #[cfg(test)]
    pub(crate) fn write_count(&self) -> usize {
        self.inner.writes.load(Ordering::SeqCst)
    }

    /// Missing => default. Corrupt or version != 1 => quarantined as
    /// `state.json.corrupt-<unix-seconds>[-n]`, warned, default.
    pub async fn load(&self) -> PersistedState {
        let path = &self.inner.path;
        let bytes = match tokio::fs::read(path).await {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return PersistedState::default(),
            Err(e) => {
                tracing::warn!("cannot read {}: {e}", path.display());
                return PersistedState::default();
            }
        };
        match serde_json::from_slice::<PersistedState>(&bytes) {
            Ok(st) if st.version == CURRENT_VERSION => st,
            Ok(st) => {
                tracing::warn!("{} has unsupported version {}", path.display(), st.version);
                self.quarantine().await;
                PersistedState::default()
            }
            Err(e) => {
                tracing::warn!("{} is corrupt: {e}", path.display());
                self.quarantine().await;
                PersistedState::default()
            }
        }
    }

    async fn quarantine(&self) {
        let path = &self.inner.path;
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let base = format!("{}.corrupt-{ts}", path.display());
        let mut dest = PathBuf::from(&base);
        let mut n = 0u32;
        while tokio::fs::try_exists(&dest).await.unwrap_or(false) {
            n += 1;
            dest = PathBuf::from(format!("{base}-{n}"));
        }
        match tokio::fs::rename(path, &dest).await {
            Ok(()) => tracing::warn!("quarantined state file to {}", dest.display()),
            Err(e) => tracing::warn!("failed to quarantine {}: {e}", path.display()),
        }
    }

    /// Writes immediately (atomic, private). Supersedes any pending debounced state.
    pub async fn save_now(&self, st: &PersistedState) -> std::io::Result<()> {
        let _g = self.inner.write_lock.lock().await;
        self.inner.pending.lock().unwrap().state = None;
        self.inner.write(st).await
    }

    /// Debounced: coalesces calls within 300 ms; the latest state wins. Needs a Tokio runtime.
    pub fn save_soon(&self, st: PersistedState) {
        let mut p = self.inner.pending.lock().unwrap();
        p.state = Some(st);
        if !p.running {
            p.running = true;
            tokio::spawn(self.inner.clone().debounce_task());
        }
    }

    /// Waits for any in-flight write and writes any pending state now.
    /// A failure is logged and the state stays pending.
    pub async fn flush(&self) {
        let _g = self.inner.write_lock.lock().await;
        if let Err(e) = self.inner.write_pending().await {
            tracing::warn!("failed to flush {}: {e}", self.inner.path.display());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::new(dir.path().join("state.json"));
        (dir, s)
    }

    fn corrupt_files(dir: &std::path::Path) -> Vec<PathBuf> {
        std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().path())
            .filter(|p| {
                p.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with("state.json.corrupt-")
            })
            .collect()
    }

    fn sample(next_id: u64) -> PersistedState {
        PersistedState {
            next_id,
            order: vec![1, 2],
            downloads: vec![StoredDownload {
                id: 1,
                request: AddRequest {
                    source: puppy_core::types::Source::Http {
                        url: "http://x/f.bin".parse().unwrap(),
                    },
                    output_dir: None,
                    filename: None,
                    context: puppy_core::types::RequestContext {
                        cookies: Some("sid=secret".into()),
                        ..Default::default()
                    },
                },
                limit: Some(5),
                state: StoredState::Failed {
                    reason: "boom".into(),
                },
                name: Some("f.bin".into()),
                path: Some("/tmp/f.bin".into()),
                total_bytes: Some(10),
                created_unix: 100,
                finished_unix: None,
            }],
            settings: Some(RuntimeSettings {
                max_active: 3,
                global_down_limit: None,
                global_up_limit: Some(9),
            }),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn missing_state_is_default() {
        let (_d, s) = store();
        assert_eq!(s.load().await, PersistedState::default());
        assert_eq!(PersistedState::default().next_id, 1);
    }

    #[tokio::test]
    async fn roundtrip() {
        let (_d, s) = store();
        let st = sample(7);
        s.save_now(&st).await.unwrap();
        assert_eq!(s.load().await, st);
    }

    #[tokio::test]
    async fn corrupt_state_is_quarantined() {
        let (d, s) = store();
        std::fs::write(d.path().join("state.json"), b"garbage{").unwrap();
        assert_eq!(s.load().await, PersistedState::default());
        assert!(!d.path().join("state.json").exists());
        assert_eq!(corrupt_files(d.path()).len(), 1);
        // A second corrupt file within the same second gets a suffix, not an overwrite.
        std::fs::write(d.path().join("state.json"), b"garbage2").unwrap();
        s.load().await;
        assert_eq!(corrupt_files(d.path()).len(), 2);
    }

    #[tokio::test]
    async fn future_version_is_quarantined() {
        let (d, s) = store();
        let mut st = sample(3);
        st.version = 2;
        std::fs::write(
            d.path().join("state.json"),
            serde_json::to_vec(&st).unwrap(),
        )
        .unwrap();
        assert_eq!(s.load().await, PersistedState::default());
        assert_eq!(corrupt_files(d.path()).len(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn save_soon_coalesces() {
        let (_d, s) = store();
        for i in 1..=10 {
            s.save_soon(sample(i));
        }
        tokio::time::sleep(Duration::from_millis(400)).await;
        s.flush().await;
        assert_eq!(s.write_count(), 1);
        assert_eq!(s.load().await.next_id, 10);
    }

    #[tokio::test(start_paused = true)]
    async fn flush_writes_pending_immediately() {
        let (_d, s) = store();
        s.save_soon(sample(4));
        s.flush().await;
        assert_eq!(s.load().await.next_id, 4);
        // The background task finds nothing left to write.
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert_eq!(s.write_count(), 1);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn state_file_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let (d, s) = store();
        s.save_now(&sample(2)).await.unwrap();
        let mode = std::fs::metadata(d.path().join("state.json"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }
}
