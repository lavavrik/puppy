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

/// Errors from loading the state file.
#[derive(thiserror::Error, Debug)]
pub enum StoreError {
    /// The file exists but could not be read or quarantined; the caller must not
    /// overwrite it.
    #[error("state file {} is unreadable: {source}", path.display())]
    Unreadable {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    /// The file was written by a newer daemon (state format `found`; this one reads up
    /// to `supported`). It is left untouched; the caller must not overwrite it.
    #[error(
        "state file {} is from a newer puppy (format {found}, this one supports {supported}); \
         upgrade puppy",
        path.display()
    )]
    TooNew {
        path: PathBuf,
        found: u32,
        supported: u32,
    },
}

const CURRENT_VERSION: u32 = 1;
const DEBOUNCE: Duration = Duration::from_millis(300);
/// A failed debounced write is retried after this long, doubling up to
/// [`RETRY_MAX`].
const RETRY_FIRST: Duration = Duration::from_secs(1);
const RETRY_MAX: Duration = Duration::from_secs(30);

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
    /// `Event::ChoosingFiles` was emitted for this download (once per download, also
    /// across restarts; a download restored as choosing files counts as asked). Cleared
    /// when the choice is made or the download is restarted.
    #[serde(default)]
    pub choose_files_sent: bool,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum StoredState {
    Queued,
    Active,
    Paused,
    Completed,
    Failed {
        reason: String,
        /// Retried automatically (also after a restart).
        #[serde(default)]
        transient: bool,
    },
    NeedsAttention {
        reason: AttentionReason,
    },
    /// A torrent waiting for its files to be chosen (restored still choosing).
    ChoosingFiles,
    /// A torrent whose data is complete, seeding (restored seeding: its counters are
    /// the engine's).
    Seeding,
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
    /// `close` has run: nothing more is written.
    closed: bool,
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

    /// Writes the pending state after [`DEBOUNCE`], again while more arrives, and
    /// keeps retrying a failed write (after 1 s, doubling up to 30 s) until it
    /// succeeds, nothing is pending any more, or the store is closed.
    async fn debounce_task(self: Arc<Self>) {
        let mut delay = DEBOUNCE;
        let mut retry = RETRY_FIRST;
        loop {
            tokio::time::sleep(delay).await;
            let _g = self.write_lock.lock().await;
            if self.pending.lock().unwrap().closed {
                self.pending.lock().unwrap().running = false;
                return;
            }
            let res = self.write_pending().await;
            let mut p = self.pending.lock().unwrap();
            if p.state.is_none() {
                p.running = false;
                return;
            }
            delay = match res {
                Ok(()) => {
                    retry = RETRY_FIRST;
                    DEBOUNCE
                }
                Err(e) => {
                    // The state stays pending (unless a newer one arrived) and this
                    // task stays the one writing it.
                    tracing::warn!(
                        "failed to save {} (retrying in {retry:?}): {e}",
                        self.path.display()
                    );
                    let d = retry;
                    retry = (retry * 2).min(RETRY_MAX);
                    d
                }
            };
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

    /// Missing => default. A newer format (`version` above the supported one, whatever
    /// the rest looks like) => [`StoreError::TooNew`], file untouched. Corrupt or an
    /// older version => quarantined as `state.json.corrupt-<unix-seconds>[-n]`, warned,
    /// default. Any other read error, or a failed quarantine, is an error so the caller
    /// never overwrites the user's real list.
    pub async fn load(&self) -> Result<PersistedState, StoreError> {
        let path = &self.inner.path;
        let unreadable = |source| StoreError::Unreadable {
            path: path.clone(),
            source,
        };
        let bytes = match tokio::fs::read(path).await {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(PersistedState::default());
            }
            Err(e) => return Err(unreadable(e)),
        };
        let version = serde_json::from_slice::<serde_json::Value>(&bytes)
            .ok()
            .and_then(|v| v.get("version")?.as_u64());
        if let Some(found) = version
            && found > u64::from(CURRENT_VERSION)
        {
            return Err(StoreError::TooNew {
                path: path.clone(),
                found: u32::try_from(found).unwrap_or(u32::MAX),
                supported: CURRENT_VERSION,
            });
        }
        match serde_json::from_slice::<PersistedState>(&bytes) {
            Ok(st) if st.version == CURRENT_VERSION => Ok(st),
            Ok(st) => {
                tracing::warn!("{} has unsupported version {}", path.display(), st.version);
                self.quarantine().await.map_err(unreadable)?;
                Ok(PersistedState::default())
            }
            Err(e) => {
                tracing::warn!("{} is corrupt: {e}", path.display());
                self.quarantine().await.map_err(unreadable)?;
                Ok(PersistedState::default())
            }
        }
    }

    async fn quarantine(&self) -> std::io::Result<()> {
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
        tokio::fs::rename(path, &dest).await?;
        tracing::warn!("quarantined state file to {}", dest.display());
        Ok(())
    }

    /// Writes immediately (atomic, private). Supersedes any pending debounced state.
    /// A no-op once the store is closed.
    pub async fn save_now(&self, st: &PersistedState) -> std::io::Result<()> {
        let _g = self.inner.write_lock.lock().await;
        {
            let mut p = self.inner.pending.lock().unwrap();
            if p.closed {
                tracing::debug!("not saving: the store is closed");
                return Ok(());
            }
            p.state = None;
        }
        let res = self.inner.write(st).await;
        if res.is_err() {
            let mut p = self.inner.pending.lock().unwrap();
            if p.state.is_none() {
                p.state = Some(st.clone());
            }
        }
        res
    }

    /// Debounced: coalesces calls within 300 ms; the latest state wins. Needs a Tokio
    /// runtime. A no-op once the store is closed.
    pub fn save_soon(&self, st: PersistedState) {
        let mut p = self.inner.pending.lock().unwrap();
        if p.closed {
            tracing::debug!("not saving: the store is closed");
            return;
        }
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

    /// Final flush: writes any pending state now, after which `save_soon` and
    /// `save_now` do nothing (so nothing can overwrite the final state, e.g. a late
    /// background save after shutdown). A failed final write is logged.
    pub async fn close(&self) {
        let _g = self.inner.write_lock.lock().await;
        if let Err(e) = self.inner.write_pending().await {
            tracing::warn!("failed to save {}: {e}", self.inner.path.display());
        }
        let mut p = self.inner.pending.lock().unwrap();
        p.closed = true;
        p.state = None;
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
                    files: Default::default(),
                    seed: None,
                    checksum: None,
                },
                limit: Some(5),
                state: StoredState::Failed {
                    reason: "boom".into(),
                    transient: true,
                },
                name: Some("f.bin".into()),
                path: Some("/tmp/f.bin".into()),
                total_bytes: Some(10),
                created_unix: 100,
                finished_unix: None,
                choose_files_sent: false,
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
        assert_eq!(s.load().await.unwrap(), PersistedState::default());
        assert_eq!(PersistedState::default().next_id, 1);
    }

    #[tokio::test]
    async fn roundtrip() {
        let (_d, s) = store();
        let st = sample(7);
        s.save_now(&st).await.unwrap();
        assert_eq!(s.load().await.unwrap(), st);
    }

    #[tokio::test]
    async fn corrupt_state_is_quarantined() {
        let (d, s) = store();
        std::fs::write(d.path().join("state.json"), b"garbage{").unwrap();
        assert_eq!(s.load().await.unwrap(), PersistedState::default());
        assert!(!d.path().join("state.json").exists());
        assert_eq!(corrupt_files(d.path()).len(), 1);
        // A second corrupt file within the same second gets a suffix, not an overwrite.
        std::fs::write(d.path().join("state.json"), b"garbage2").unwrap();
        s.load().await.unwrap();
        assert_eq!(corrupt_files(d.path()).len(), 2);
    }

    /// A newer daemon's state file is refused, never quarantined or overwritten.
    #[tokio::test]
    async fn future_version_is_refused() {
        let (d, s) = store();
        let f = d.path().join("state.json");
        // Possibly with fields this version does not know.
        let mut v = serde_json::to_value(sample(3)).unwrap();
        v["version"] = (CURRENT_VERSION + 1).into();
        v["new_field"] = true.into();
        let bytes = serde_json::to_vec(&v).unwrap();
        std::fs::write(&f, &bytes).unwrap();
        match s.load().await {
            Err(StoreError::TooNew {
                found, supported, ..
            }) => {
                assert_eq!((found, supported), (CURRENT_VERSION + 1, CURRENT_VERSION));
            }
            other => panic!("{other:?}"),
        }
        assert!(corrupt_files(d.path()).is_empty());
        assert_eq!(std::fs::read(&f).unwrap(), bytes);
        // Even a newer file this version cannot parse at all.
        std::fs::write(&f, br#"{"version": 9, "downloads": "changed shape"}"#).unwrap();
        assert!(matches!(
            s.load().await,
            Err(StoreError::TooNew { found: 9, .. })
        ));
        assert!(corrupt_files(d.path()).is_empty());
    }

    #[tokio::test]
    async fn old_version_is_quarantined() {
        let (d, s) = store();
        let mut st = sample(3);
        st.version = 0;
        std::fs::write(
            d.path().join("state.json"),
            serde_json::to_vec(&st).unwrap(),
        )
        .unwrap();
        assert_eq!(s.load().await.unwrap(), PersistedState::default());
        assert_eq!(corrupt_files(d.path()).len(), 1);
    }

    /// A state file from before checksums (no `checksum` in its requests) loads; one
    /// written now without checksums has none either (an older daemon reads it as is),
    /// and a checksum survives a round trip.
    #[tokio::test]
    async fn checksum_field_is_optional_and_round_trips() {
        let (d, s) = store();
        let old = r#"{"version":1,"next_id":2,"order":[1],"settings":null,"downloads":[
            {"id":1,"request":{"source":{"kind":"http","url":"http://x/f.bin"},
             "output_dir":null,"filename":null,"context":{"headers":[],"cookies":null,
             "referer":null,"user_agent":null,"basic_auth":null}},
             "limit":null,"state":{"kind":"completed"},"name":"f.bin","path":"/tmp/f.bin",
             "total_bytes":10,"created_unix":1,"finished_unix":2}]}"#;
        std::fs::write(d.path().join("state.json"), old).unwrap();
        let st = s.load().await.unwrap();
        assert_eq!(st.downloads.len(), 1);
        assert_eq!(st.downloads[0].request.checksum, None);
        assert!(corrupt_files(d.path()).is_empty());

        let plain = serde_json::to_string(&sample(3)).unwrap();
        assert!(!plain.contains("checksum"), "{plain}");

        let mut st = sample(3);
        st.downloads[0].request.checksum = Some(
            "sha-1=A9993E364706816ABA3E25717850C26C9CD0D89D"
                .parse()
                .unwrap(),
        );
        s.save_now(&st).await.unwrap();
        let loaded = s.load().await.unwrap();
        assert_eq!(loaded, st);
        let raw = std::fs::read_to_string(d.path().join("state.json")).unwrap();
        assert!(raw.contains(r#""algo": "sha-1""#), "{raw}");
        assert!(
            raw.contains("a9993e364706816aba3e25717850c26c9cd0d89d"),
            "{raw}"
        );
    }

    #[test]
    fn failed_without_transient_parses() {
        let s: StoredState = serde_json::from_str(r#"{"kind":"failed","reason":"x"}"#).unwrap();
        assert_eq!(
            s,
            StoredState::Failed {
                reason: "x".into(),
                transient: false
            }
        );
    }

    #[test]
    fn torrent_states_round_trip() {
        for (state, json) in [
            (StoredState::ChoosingFiles, r#"{"kind":"choosing_files"}"#),
            (StoredState::Seeding, r#"{"kind":"seeding"}"#),
        ] {
            assert_eq!(serde_json::to_string(&state).unwrap(), json);
            assert_eq!(serde_json::from_str::<StoredState>(json).unwrap(), state);
        }
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
        assert_eq!(s.load().await.unwrap().next_id, 10);
    }

    #[tokio::test(start_paused = true)]
    async fn flush_writes_pending_immediately() {
        let (_d, s) = store();
        s.save_soon(sample(4));
        s.flush().await;
        assert_eq!(s.load().await.unwrap().next_id, 4);
        // The background task finds nothing left to write.
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert_eq!(s.write_count(), 1);
    }

    #[cfg(unix)]
    fn chmod(p: &std::path::Path, mode: u32) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(p, std::fs::Permissions::from_mode(mode)).unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn unreadable_state_is_error() {
        let (d, s) = store();
        let f = d.path().join("state.json");
        std::fs::write(&f, b"{}").unwrap();
        chmod(&f, 0o000);
        if std::fs::read(&f).is_ok() {
            return; // running as root: cannot make the file unreadable
        }
        assert!(matches!(s.load().await, Err(StoreError::Unreadable { .. })));
        chmod(&f, 0o600);
        assert!(f.exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn quarantine_failure_is_error() {
        let (d, s) = store();
        let f = d.path().join("state.json");
        std::fs::write(&f, b"garbage").unwrap();
        chmod(d.path(), 0o500); // rename needs write access to the directory
        let probe = d.path().join("probe");
        if std::fs::write(&probe, b"").is_ok() {
            chmod(d.path(), 0o700);
            return; // running as root: directory permissions are not enforced
        }
        let res = s.load().await;
        chmod(d.path(), 0o700);
        assert!(matches!(res, Err(StoreError::Unreadable { .. })));
        assert_eq!(std::fs::read(&f).unwrap(), b"garbage");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn failed_write_retried_by_flush() {
        let (d, s) = store();
        let probe = d.path().join("probe");
        chmod(d.path(), 0o500);
        if std::fs::write(&probe, b"").is_ok() {
            chmod(d.path(), 0o700);
            return; // running as root
        }
        s.save_soon(sample(6));
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert_eq!(s.write_count(), 0);
        chmod(d.path(), 0o700);
        s.flush().await;
        assert_eq!(s.write_count(), 1);
        assert_eq!(s.load().await.unwrap().next_id, 6);
    }

    #[tokio::test]
    async fn saves_after_close_are_ignored() {
        let (_d, s) = store();
        s.save_soon(sample(3));
        s.close().await;
        assert_eq!(s.load().await.unwrap().next_id, 3, "close flushes");
        s.save_soon(sample(4));
        s.save_now(&sample(5)).await.unwrap();
        tokio::time::sleep(Duration::from_millis(500)).await;
        s.flush().await;
        assert_eq!(s.write_count(), 1);
        assert_eq!(s.load().await.unwrap().next_id, 3);
    }

    /// A failed debounced write is retried by itself (no flush or later save needed).
    #[cfg(unix)]
    #[tokio::test]
    async fn failed_debounced_write_is_retried() {
        let (d, s) = store();
        let probe = d.path().join("probe");
        chmod(d.path(), 0o500);
        if std::fs::write(&probe, b"").is_ok() {
            chmod(d.path(), 0o700);
            return; // running as root
        }
        s.save_soon(sample(6));
        tokio::time::sleep(Duration::from_millis(600)).await;
        assert_eq!(s.write_count(), 0);
        chmod(d.path(), 0o700);
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while s.write_count() == 0 {
            assert!(std::time::Instant::now() < deadline, "never retried");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert_eq!(s.load().await.unwrap().next_id, 6);
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
