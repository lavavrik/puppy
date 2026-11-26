//! Crash-safe resume sidecar: records how far each segment of a download got.

use std::future::Future;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::engine::EngineError;
use crate::fsutil::atomic_write;
use crate::http::segments::Segment;
use crate::types::{AddRequest, RequestContext};

/// Current sidecar format version. 2: request context no longer stored.
pub const VERSION: u32 = 2;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HttpResumeState {
    pub version: u32,
    /// The request with an empty `context`: cookies and credentials are never written
    /// next to the download (it may sit on a shared or unprotected volume).
    pub request: AddRequest,
    /// Fingerprint of the original request context ([`context_hash`]); only used to
    /// notice that a restore was given a different context.
    pub context_hash: u64,
    pub final_url: url::Url,
    pub total: Option<u64>,
    pub etag: Option<String>,
    pub etag_strong: bool,
    pub last_modified: Option<String>,
    pub final_path: PathBuf,
    pub segments: Vec<Segment>,
    /// One plain GET from offset 0 (no ranges, unknown length, or fallen back after a
    /// server ignored ranges): cannot be continued, only restarted.
    #[serde(default)]
    pub single_stream: bool,
    /// What identified the file at each mirror (`request.mirrors`) when last seen; the
    /// fields above are the primary's. Absent for a download without mirrors.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mirrors: Vec<MirrorResume>,
}

/// One mirror's validators in a sidecar. A mirror that never answered has none (and its
/// own URL as `final_url`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MirrorResume {
    pub url: url::Url,
    pub final_url: url::Url,
    #[serde(default)]
    pub etag: Option<String>,
    #[serde(default)]
    pub etag_strong: bool,
    #[serde(default)]
    pub last_modified: Option<String>,
}

/// The request as recorded in a sidecar: everything but its context.
pub fn without_context(req: &AddRequest) -> AddRequest {
    AddRequest {
        context: RequestContext::default(),
        ..req.clone()
    }
}

/// Stable, non-cryptographic fingerprint (FNV-1a 64 of the JSON) of a request context.
pub fn context_hash(ctx: &RequestContext) -> u64 {
    let json = serde_json::to_vec(ctx).unwrap_or_default();
    json.iter().fold(0xcbf2_9ce4_8422_2325u64, |h, &b| {
        (h ^ u64::from(b)).wrapping_mul(0x0000_0100_0000_01b3)
    })
}

fn append_ext(final_path: &Path, ext: &str) -> PathBuf {
    let mut s = final_path.as_os_str().to_os_string();
    s.push(ext);
    PathBuf::from(s)
}

/// `<final>.part`
pub fn part_path(final_path: &Path) -> PathBuf {
    append_ext(final_path, ".part")
}

/// `<final>.part.puppy`
pub fn sidecar_path(final_path: &Path) -> PathBuf {
    append_ext(final_path, ".part.puppy")
}

impl HttpResumeState {
    /// Loads and validates the sidecar. `Invalid` on unknown version, corruption or
    /// violated segment invariants.
    pub async fn load(final_path: &Path) -> Result<Self, EngineError> {
        let bytes = tokio::fs::read(sidecar_path(final_path)).await?;
        let state: Self = serde_json::from_slice(&bytes)
            .map_err(|e| EngineError::Invalid(format!("corrupt resume sidecar: {e}")))?;
        if state.version != VERSION {
            return Err(EngineError::Invalid(format!(
                "unsupported resume sidecar version {}",
                state.version
            )));
        }
        state.validate_segments()?;
        Ok(state)
    }

    fn validate_segments(&self) -> Result<(), EngineError> {
        let bad = |m: &str| Err(EngineError::Invalid(format!("resume sidecar: {m}")));
        let mut prev_end = 0u64;
        for s in &self.segments {
            if s.start > s.end {
                return bad("segment start exceeds end");
            }
            if s.done > s.end - s.start {
                return bad("segment done exceeds its length");
            }
            if s.start < prev_end {
                return bad("segments unsorted or overlapping");
            }
            prev_end = s.end;
        }
        Ok(())
    }

    /// fsyncs `part_file` first, then atomically writes the sidecar (mode 0600 on Unix).
    pub async fn flush(&self, part_file: &std::fs::File) -> Result<(), EngineError> {
        let sidecar = sidecar_path(&self.final_path);
        self.flush_with(
            || async {
                let file = part_file.try_clone()?;
                tokio::task::spawn_blocking(move || file.sync_data())
                    .await
                    .map_err(std::io::Error::other)?
            },
            |bytes| async move { atomic_write(&sidecar, &bytes, true).await },
        )
        .await
    }

    async fn flush_with<S, SF, W, WF>(&self, sync: S, write: W) -> Result<(), EngineError>
    where
        S: FnOnce() -> SF,
        SF: Future<Output = std::io::Result<()>>,
        W: FnOnce(Vec<u8>) -> WF,
        WF: Future<Output = std::io::Result<()>>,
    {
        let json = serde_json::to_vec(self)
            .map_err(|e| EngineError::Invalid(format!("serialize resume state: {e}")))?;
        sync().await?;
        write(json).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{RequestContext, Source};
    use std::cell::RefCell;

    fn state(final_path: &Path, segments: Vec<Segment>) -> HttpResumeState {
        let url = url::Url::parse("https://example.com/a.iso").unwrap();
        HttpResumeState {
            version: VERSION,
            request: AddRequest {
                source: Source::Http { url: url.clone() },
                output_dir: None,
                filename: None,
                context: RequestContext::default(),
                files: Default::default(),
                seed: None,
                checksum: None,
                mirrors: Vec::new(),
            },
            context_hash: context_hash(&RequestContext::default()),
            final_url: url,
            total: Some(100),
            etag: Some("\"abc\"".into()),
            etag_strong: true,
            last_modified: None,
            final_path: final_path.to_path_buf(),
            segments,
            single_stream: false,
            mirrors: Vec::new(),
        }
    }

    fn seg(start: u64, end: u64, done: u64) -> Segment {
        Segment { start, end, done }
    }

    async fn write_raw(final_path: &Path, s: &HttpResumeState) {
        tokio::fs::write(sidecar_path(final_path), serde_json::to_vec(s).unwrap())
            .await
            .unwrap();
    }

    /// A sidecar without mirrors has no `mirrors` key (an older puppy reads it as is);
    /// one with mirrors keeps their validators.
    #[tokio::test]
    async fn mirror_records_are_optional_and_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let final_path = dir.path().join("a.iso");
        let mut s = state(&final_path, vec![seg(0, 100, 10)]);
        let json = serde_json::to_string(&s).unwrap();
        assert!(!json.contains("mirrors"), "{json}");
        let m = url::Url::parse("https://mirror.example/a.iso").unwrap();
        s.request.mirrors = vec![crate::types::Mirror::new(m.clone())];
        s.mirrors = vec![MirrorResume {
            url: m.clone(),
            final_url: m,
            etag: Some("\"m\"".into()),
            etag_strong: true,
            last_modified: None,
        }];
        write_raw(&final_path, &s).await;
        assert_eq!(HttpResumeState::load(&final_path).await.unwrap(), s);
    }

    #[test]
    fn paths_append_to_full_name() {
        let p = Path::new("/d/a.iso");
        assert_eq!(part_path(p), Path::new("/d/a.iso.part"));
        assert_eq!(sidecar_path(p), Path::new("/d/a.iso.part.puppy"));
    }

    #[tokio::test]
    async fn sidecar_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let final_path = dir.path().join("a.iso");
        let part = std::fs::File::create(part_path(&final_path)).unwrap();
        let s = state(&final_path, vec![seg(0, 50, 10), seg(50, 100, 0)]);
        s.flush(&part).await.unwrap();
        assert_eq!(HttpResumeState::load(&final_path).await.unwrap(), s);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(sidecar_path(&final_path))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }

    #[tokio::test]
    async fn single_stream_defaults_to_false() {
        let dir = tempfile::tempdir().unwrap();
        let final_path = dir.path().join("a.iso");
        let mut json = serde_json::to_value(state(&final_path, vec![seg(0, 100, 0)])).unwrap();
        json.as_object_mut().unwrap().remove("single_stream");
        tokio::fs::write(sidecar_path(&final_path), json.to_string())
            .await
            .unwrap();
        assert!(
            !HttpResumeState::load(&final_path)
                .await
                .unwrap()
                .single_stream
        );
    }

    #[tokio::test]
    async fn load_rejects_unknown_version() {
        let dir = tempfile::tempdir().unwrap();
        let final_path = dir.path().join("a.iso");
        let mut s = state(&final_path, vec![seg(0, 100, 0)]);
        s.version = 99;
        write_raw(&final_path, &s).await;
        assert!(matches!(
            HttpResumeState::load(&final_path).await,
            Err(EngineError::Invalid(_))
        ));
    }

    #[tokio::test]
    async fn load_rejects_version_1() {
        let dir = tempfile::tempdir().unwrap();
        let final_path = dir.path().join("a.iso");
        let mut s = state(&final_path, vec![seg(0, 100, 0)]);
        s.version = 1;
        write_raw(&final_path, &s).await;
        assert!(matches!(
            HttpResumeState::load(&final_path).await,
            Err(EngineError::Invalid(_))
        ));
    }

    #[test]
    fn context_hash_distinguishes_contexts() {
        let a = RequestContext::default();
        let b = RequestContext {
            cookies: Some("session=1".into()),
            ..RequestContext::default()
        };
        assert_eq!(context_hash(&a), context_hash(&a.clone()));
        assert_ne!(context_hash(&a), context_hash(&b));
    }

    #[test]
    fn without_context_clears_secrets() {
        let req = AddRequest {
            source: Source::Http {
                url: url::Url::parse("https://example.com/a").unwrap(),
            },
            output_dir: None,
            filename: Some("a".into()),
            files: Default::default(),
            seed: None,
            context: RequestContext {
                cookies: Some("session=secret".into()),
                basic_auth: Some(("u".into(), Some("p".into()))),
                ..RequestContext::default()
            },
            checksum: None,
            mirrors: Vec::new(),
        };
        let s = without_context(&req);
        assert_eq!(s.context, RequestContext::default());
        assert_eq!(s.source, req.source);
        assert_eq!(s.filename, req.filename);
    }

    #[tokio::test]
    async fn load_rejects_corrupt_json() {
        let dir = tempfile::tempdir().unwrap();
        let final_path = dir.path().join("a.iso");
        tokio::fs::write(sidecar_path(&final_path), b"not json {{")
            .await
            .unwrap();
        assert!(matches!(
            HttpResumeState::load(&final_path).await,
            Err(EngineError::Invalid(_))
        ));
    }

    #[tokio::test]
    async fn load_rejects_invalid_segments() {
        let dir = tempfile::tempdir().unwrap();
        let final_path = dir.path().join("a.iso");
        let cases = [
            vec![seg(0, 50, 51)],                 // done > len
            vec![seg(60, 50, 0)],                 // start > end
            vec![seg(50, 100, 0), seg(0, 50, 0)], // unsorted
            vec![seg(0, 60, 0), seg(50, 100, 0)], // overlap
        ];
        for segs in cases {
            write_raw(&final_path, &state(&final_path, segs.clone())).await;
            assert!(
                matches!(
                    HttpResumeState::load(&final_path).await,
                    Err(EngineError::Invalid(_))
                ),
                "{segs:?}"
            );
        }
    }

    #[tokio::test]
    async fn flush_order_data_before_sidecar() {
        let dir = tempfile::tempdir().unwrap();
        let s = state(&dir.path().join("a.iso"), vec![seg(0, 100, 0)]);
        let log = RefCell::new(Vec::new());
        s.flush_with(
            || async {
                log.borrow_mut().push("sync_data");
                Ok(())
            },
            |_| async {
                log.borrow_mut().push("write_sidecar");
                Ok(())
            },
        )
        .await
        .unwrap();
        assert_eq!(*log.borrow(), ["sync_data", "write_sidecar"]);
    }
}
