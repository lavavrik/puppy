//! Restoring a download from its sidecar after a crash or restart.

use super::files::file_len;
use super::transfer::spawn_run;
use super::{Env, HttpDownload, Plan, SegEntry, file_name};
use crate::config::HttpConfig;
use crate::engine::{Added, EngineError, EngineEvent};
use crate::http::probe::RemoteInfo;
use crate::http::resume::{HttpResumeState, part_path, sidecar_path};
use crate::limiter::TokenBucket;
use crate::types::{
    AddRequest, AttentionReason, DownloadId, DownloadState, RequestContext, Source,
};
use std::io;
use std::path::Path;
use tokio::sync::broadcast;

impl Plan {
    /// The plan recorded in a sidecar. Single streams restart from zero.
    fn restored(state: &HttpResumeState) -> Self {
        if state.single_stream || state.total.is_none() {
            return Plan::single(state.total, false);
        }
        Plan {
            single: false,
            fell_back: false,
            segments: state
                .segments
                .iter()
                .map(|s| SegEntry::new(s.start, s.end, s.done))
                .collect(),
        }
    }

    /// A finished file of `len` bytes.
    fn finished(len: u64) -> Self {
        Plan {
            single: false,
            fell_back: false,
            segments: (len > 0)
                .then(|| SegEntry::new(0, len, len))
                .into_iter()
                .collect(),
        }
    }

    /// Nothing known (no sidecar); never run.
    fn unknown() -> Self {
        Plan {
            single: true,
            fell_back: false,
            segments: Vec::new(),
        }
    }
}

impl HttpDownload {
    /// Continues a download from its sidecar after a crash or restart, but only once the
    /// remote file is proven unchanged (see [`crate::http::validate::validate_remote`]). Outcomes that need the
    /// user (`NeedsAttention`) or a retry (`Failed`, from a transient validation error;
    /// `resume` retries) are returned as `Ok` with the download in that state. Files are
    /// never deleted here, except a stale sidecar next to an already finished file.
    ///
    /// Without a readable sidecar the returned download carries a synthetic placeholder
    /// request (a `file://` URL of `final_path`); it must not be persisted as the user's
    /// request.
    pub async fn restore(
        id: DownloadId,
        final_path: &Path,
        client: reqwest::Client,
        cfg: HttpConfig,
        global: TokenBucket,
        events: broadcast::Sender<EngineEvent>,
    ) -> Result<(Self, Added), EngineError> {
        let env = Env::new(id, client, cfg, global, events);
        env.emit(DownloadState::Starting);
        let final_path = final_path.to_path_buf();
        let sidecar = sidecar_path(&final_path);
        let loaded = HttpResumeState::load(&final_path).await;
        let part_len = file_len(&part_path(&final_path)).await?;

        // Finalizing renames `.part` before deleting the sidecar: a final file with a
        // sidecar but no `.part` was completed (and fsynced) before the crash.
        if part_len.is_none()
            && file_len(&sidecar).await?.is_some()
            && let Some(len) = file_len(&final_path).await?
        {
            // The file is complete unless the sidecar says otherwise.
            if let Ok(state) = &loaded
                && let Some(total) = state.total
                && total != len
            {
                let (request, remote) = from_sidecar(state, &final_path);
                let (inner, added) = env.build(request, remote, final_path, Plan::unknown());
                inner.needs_attention(AttentionReason::ValidationFailed {
                    detail: format!("final file is {len} bytes, expected {total}"),
                });
                return Ok((HttpDownload { inner }, added));
            }
            match tokio::fs::remove_file(&sidecar).await {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => tracing::warn!(path = %sidecar.display(), "cannot delete: {e}"),
            }
            let (request, mut remote) = match &loaded {
                Ok(state) => from_sidecar(state, &final_path),
                Err(_) => placeholder(&final_path),
            };
            remote.total = Some(len);
            let (inner, added) = env.build(request, remote, final_path, Plan::finished(len));
            inner.set_state(DownloadState::Completed);
            let _ = inner.events.send(EngineEvent::Completed { id });
            return Ok((HttpDownload { inner }, added));
        }

        let state = match loaded {
            Ok(s) => s,
            Err(e) => {
                let (request, remote) = placeholder(&final_path);
                let (inner, added) = env.build(request, remote, final_path, Plan::unknown());
                inner.needs_attention(AttentionReason::ValidationFailed {
                    detail: format!("cannot load resume sidecar: {e}"),
                });
                return Ok((HttpDownload { inner }, added));
            }
        };
        let (request, remote) = from_sidecar(&state, &final_path);
        let (inner, added) = env.build(request, remote, final_path, Plan::restored(&state));
        let reason = match (part_len, state.total) {
            (None, _) => Some(AttentionReason::ValidationFailed {
                detail: "partial file is missing".into(),
            }),
            (Some(_), None) => Some(AttentionReason::NotResumable),
            _ if state.single_stream => Some(AttentionReason::NotResumable),
            (Some(len), Some(total)) => check_layout(&state, total, len)
                .err()
                .map(|detail| AttentionReason::ValidationFailed { detail }),
        };
        if let Some(reason) = reason {
            inner.needs_attention(reason);
            return Ok((HttpDownload { inner }, added));
        }
        // Nobody else holds this download yet, so only the timeout can end validation.
        let interrupt = inner.interrupt.subscribe();
        let mut c = inner.control.lock().await;
        if let Ok(true) = inner.validate(interrupt).await {
            c.run = Some(spawn_run(&inner));
        }
        drop(c);
        Ok((HttpDownload { inner }, added))
    }
}

/// Request and remote facts recorded in a sidecar.
fn from_sidecar(state: &HttpResumeState, final_path: &Path) -> (AddRequest, RemoteInfo) {
    let remote = RemoteInfo {
        final_url: state.final_url.clone(),
        total: state.total,
        accepts_ranges: !state.single_stream,
        etag: state.etag.clone(),
        etag_strong: state.etag_strong,
        last_modified: state.last_modified.clone(),
        filename: file_name(final_path),
    };
    (state.request.clone(), remote)
}

/// Stand-ins when the sidecar cannot be read: such a download is only ever shown or
/// cancelled, never run.
fn placeholder(final_path: &Path) -> (AddRequest, RemoteInfo) {
    let url = url::Url::from_file_path(final_path)
        .unwrap_or_else(|()| url::Url::parse("file:///").expect("static URL"));
    let request = AddRequest {
        source: Source::Http { url: url.clone() },
        output_dir: final_path.parent().map(Path::to_path_buf),
        filename: Some(file_name(final_path)),
        context: RequestContext::default(),
    };
    let remote = RemoteInfo {
        final_url: url,
        total: None,
        accepts_ranges: false,
        etag: None,
        etag_strong: false,
        last_modified: None,
        filename: file_name(final_path),
    };
    (request, remote)
}

/// The sidecar's segments must tile `[0, total)` and `.part` must be `total` bytes long.
fn check_layout(state: &HttpResumeState, total: u64, part_len: u64) -> Result<(), String> {
    let mut cursor = 0;
    for s in &state.segments {
        if s.start != cursor {
            return Err(format!("resume sidecar: gap or overlap at offset {cursor}"));
        }
        cursor = s.end;
    }
    if cursor != total {
        return Err(format!(
            "resume sidecar: segments end at {cursor}, file is {total} bytes"
        ));
    }
    if part_len != total {
        return Err(format!(
            "partial file is {part_len} bytes, expected {total}"
        ));
    }
    Ok(())
}
