//! Restoring a download from its sidecar after a crash or restart.

use super::files::file_len;
use super::mirrors::{MirrorRt, Validators, locations, restore_records};
use super::transfer::spawn_run;
use super::{Env, HttpDownload, Plan, SegEntry, file_name};
use crate::config::HttpConfig;
use crate::engine::{AddOptions, Added, EngineError, EngineEvent};
use crate::http::probe::RemoteInfo;
use crate::http::resume::{HttpResumeState, context_hash, part_path, sidecar_path};
use crate::limiter::TokenBucket;
use crate::types::{AddRequest, AttentionReason, DownloadId, DownloadState, Source};
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
    /// `req` is the request the download was added with: its source must match the
    /// sidecar's (else `NeedsAttention`), and it supplies the request context, which the
    /// sidecar does not store. With `opts.start_paused` only the local files are checked
    /// (no network) and the download is left `Paused`; `resume` validates the remote.
    #[allow(clippy::too_many_arguments)]
    pub async fn restore(
        id: DownloadId,
        req: AddRequest,
        final_path: &Path,
        opts: AddOptions,
        client: reqwest::Client,
        cfg: HttpConfig,
        global: TokenBucket,
        events: broadcast::Sender<EngineEvent>,
    ) -> Result<(Self, Added), EngineError> {
        let Source::Http { url } = &req.source else {
            return Err(EngineError::Unsupported("not an HTTP source"));
        };
        let url = url.clone();
        let start_paused = opts.start_paused;
        let env = Env::new(id, opts, client, cfg, global, events);
        env.emit(DownloadState::Starting);
        let final_path = final_path.to_path_buf();
        let sidecar = sidecar_path(&final_path);
        let loaded = HttpResumeState::load(&final_path).await;
        let foreign = matches!(&loaded, Ok(s) if s.request.source != req.source);
        let loaded = loaded.and_then(|state| check_source(state, &req));
        let part_len = file_len(&part_path(&final_path)).await?;

        // Finalizing renames `.part` before deleting the sidecar: a final file with a
        // sidecar but no `.part` was completed (and fsynced) before the crash.
        if part_len.is_none()
            && file_len(&sidecar).await?.is_some()
            && let Some(len) = file_len(&final_path).await?
        {
            if foreign {
                let remote = unknown_remote(&url, &final_path);
                let mirrors = locations(&req);
                let (inner, added) = env.build(req, remote, mirrors, final_path, Plan::unknown());
                inner.needs_attention(AttentionReason::ValidationFailed {
                    detail: "resume sidecar belongs to a different source".into(),
                });
                return Ok((HttpDownload { inner }, added));
            }
            // The file is complete unless the sidecar says otherwise.
            if let Ok(state) = &loaded
                && let Some(total) = state.total
                && total != len
            {
                let remote = from_sidecar(state, &final_path);
                let mirrors = mirrors_from(&req, state);
                let (inner, added) = env.build(req, remote, mirrors, final_path, Plan::unknown());
                inner.needs_attention(AttentionReason::ValidationFailed {
                    detail: format!("final file is {len} bytes, expected {total}"),
                });
                return Ok((HttpDownload { inner }, added));
            }
            let (mut remote, mirrors) = match &loaded {
                Ok(state) => (from_sidecar(state, &final_path), mirrors_from(&req, state)),
                Err(_) => (unknown_remote(&url, &final_path), locations(&req)),
            };
            remote.total = Some(len);
            let (inner, added) = env.build(req, remote, mirrors, final_path, Plan::finished(len));
            inner.lock().finalized = true;
            if inner.request.checksum.is_some() {
                // Not verified yet (or it failed): the sidecar stays until it passes.
                // Verifying is local, so it runs right away unless started paused.
                if start_paused {
                    inner.set_state(DownloadState::Paused);
                } else {
                    inner.control.lock().await.run = Some(spawn_run(&inner));
                }
                return Ok((HttpDownload { inner }, added));
            }
            match tokio::fs::remove_file(&sidecar).await {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => tracing::warn!(path = %sidecar.display(), "cannot delete: {e}"),
            }
            inner.set_state(DownloadState::Completed);
            let _ = inner.events.send(EngineEvent::Completed { id });
            return Ok((HttpDownload { inner }, added));
        }

        let state = match loaded {
            Ok(s) => s,
            Err(e) => {
                let remote = unknown_remote(&url, &final_path);
                let mirrors = locations(&req);
                let (inner, added) = env.build(req, remote, mirrors, final_path, Plan::unknown());
                inner.needs_attention(AttentionReason::ValidationFailed {
                    detail: format!("cannot load resume sidecar: {e}"),
                });
                return Ok((HttpDownload { inner }, added));
            }
        };
        if state.context_hash != context_hash(&req.context) {
            // Cookies or tokens may legitimately have been refreshed; the remote file is
            // validated before any byte is reused either way.
            tracing::info!(id, "request context differs from the one recorded");
        }
        let remote = from_sidecar(&state, &final_path);
        let mirrors = mirrors_from(&req, &state);
        let (inner, added) = env.build(req, remote, mirrors, final_path, Plan::restored(&state));
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
        if start_paused {
            inner.set_state(DownloadState::Paused);
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

/// The sidecar must belong to the caller's request.
fn check_source(state: HttpResumeState, req: &AddRequest) -> Result<HttpResumeState, EngineError> {
    if state.request.source != req.source {
        return Err(EngineError::Invalid(
            "resume sidecar belongs to a different source".into(),
        ));
    }
    Ok(state)
}

/// The request's locations with the validators the sidecar recorded for each (the
/// primary's are its top-level fields). Mirrors are those of the request (the daemon's
/// record), matched to the sidecar's by URL.
fn mirrors_from(req: &AddRequest, state: &HttpResumeState) -> Vec<MirrorRt> {
    let mut mirrors = locations(req);
    if let Some(primary) = mirrors.first_mut() {
        primary.v = Validators {
            final_url: state.final_url.clone(),
            etag: state.etag.clone(),
            etag_strong: state.etag_strong,
            last_modified: state.last_modified.clone(),
        };
    }
    restore_records(&mut mirrors, &state.mirrors);
    mirrors
}

/// Remote facts recorded in a sidecar.
fn from_sidecar(state: &HttpResumeState, final_path: &Path) -> RemoteInfo {
    RemoteInfo {
        final_url: state.final_url.clone(),
        total: state.total,
        accepts_ranges: !state.single_stream,
        etag: state.etag.clone(),
        etag_strong: state.etag_strong,
        last_modified: state.last_modified.clone(),
        filename: file_name(final_path),
    }
}

/// Nothing known about the remote (no usable sidecar): such a download is only ever
/// shown or cancelled, never run.
fn unknown_remote(url: &url::Url, final_path: &Path) -> RemoteInfo {
    RemoteInfo {
        final_url: url.clone(),
        total: None,
        accepts_ranges: false,
        etag: None,
        etag_strong: false,
        last_modified: None,
        filename: file_name(final_path),
    }
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
