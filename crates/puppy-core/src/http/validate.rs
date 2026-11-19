//! Remote validation before continuing a download from its sidecar: the server's file
//! must be provably the one the `.part` holds bytes of.

use crate::config::HttpConfig;
use crate::engine::EngineError;
use crate::http::probe::{apply_context, timed};
use crate::http::resume::HttpResumeState;
use crate::http::worker::parse_content_range;
use futures_util::StreamExt;
use reqwest::{StatusCode, header};
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Validation {
    Ok,
    Changed,
    NotResumable,
}

/// Strong validator present: HEAD/range-GET with If-Range; 206 ⇒ Ok, 200 ⇒ Changed.
/// No strong validator: total must equal recorded total AND `cfg.spot_check_bytes` fetched at up to
/// 3 offsets inside completed regions (start, middle, end of completed bytes) must byte-equal the `.part` file.
/// Unknown total ⇒ NotResumable.
///
/// A strong ETag is sent as `If-Range` and alone suffices. Without any ETag, Last-Modified
/// is sent as `If-Range` but (1 s granularity) the spot check runs as well; a weak ETag
/// counts as no validator. Network and unexpected HTTP errors are `Err` (transient), never
/// `Changed`; so is exceeding `cfg.validate_timeout_ms`.
pub async fn validate_remote(
    client: &reqwest::Client,
    state: &HttpResumeState,
    part: &Path,
    cfg: &HttpConfig,
) -> Result<Validation, EngineError> {
    let limit = Duration::from_millis(cfg.validate_timeout_ms);
    tokio::time::timeout(limit, validate(client, state, part, cfg))
        .await
        .unwrap_or_else(|_| Err(EngineError::Network("validation timed out".into())))
}

async fn validate(
    client: &reqwest::Client,
    state: &HttpResumeState,
    part: &Path,
    cfg: &HttpConfig,
) -> Result<Validation, EngineError> {
    let total = match state.total {
        Some(t) if !state.single_stream => t,
        _ => return Ok(Validation::NotResumable),
    };
    let (if_range, spot_check) = match &state.etag {
        Some(e) if state.etag_strong => (Some(e.as_str()), false),
        Some(_) => (None, true),
        None => (state.last_modified.as_deref(), true),
    };
    let read_timeout = Duration::from_millis(cfg.read_timeout_ms);
    let completed: Vec<(u64, u64)> = state
        .segments
        .iter()
        .filter(|s| s.done > 0)
        .map(|s| (s.start, s.start + s.done))
        .collect();
    // Windows to compare against `.part`; without completed bytes only the total and
    // validator are checked, with a one-byte probe.
    let windows = if spot_check {
        spot_windows(&completed, cfg.spot_check_bytes.max(1))
    } else {
        completed
            .first()
            .map(|&(s, _)| vec![(s, s + 1)])
            .unwrap_or_default()
    };
    if windows.is_empty() {
        return probe_only(client, state, if_range, total, read_timeout).await;
    }
    for (start, end) in windows {
        let got = match fetch(client, state, if_range, (start, end), read_timeout).await? {
            Fetched::Range { total: t, .. } if t != Some(total) => return Ok(Validation::Changed),
            Fetched::Range { data, .. } => data,
            // Completed bytes lie inside the recorded total: the file shrank.
            Fetched::Unsatisfiable { .. } => return Ok(Validation::Changed),
            Fetched::Other(v) => return Ok(v),
        };
        let path = part.to_path_buf();
        let local = tokio::task::spawn_blocking(move || read_range(&path, start, end))
            .await
            .map_err(std::io::Error::other)??;
        if got != local {
            return Ok(Validation::Changed);
        }
    }
    Ok(Validation::Ok)
}

/// Nothing completed yet: confirms the validator and total with `bytes=0-0`.
async fn probe_only(
    client: &reqwest::Client,
    state: &HttpResumeState,
    if_range: Option<&str>,
    total: u64,
    read_timeout: Duration,
) -> Result<Validation, EngineError> {
    Ok(
        match fetch(client, state, if_range, (0, 1), read_timeout).await? {
            Fetched::Range { total: t, .. } if t == Some(total) => Validation::Ok,
            Fetched::Range { .. } => Validation::Changed,
            // An empty file cannot satisfy any range; `bytes */0` confirms it is still empty.
            Fetched::Unsatisfiable { total: Some(0) } if total == 0 => Validation::Ok,
            Fetched::Unsatisfiable { .. } => Validation::Changed,
            Fetched::Other(v) => v,
        },
    )
}

enum Fetched {
    /// 206 for exactly the requested range.
    Range { total: Option<u64>, data: Vec<u8> },
    /// 416, with the total from `Content-Range: bytes */<total>`.
    Unsatisfiable { total: Option<u64> },
    /// The answer already decides the validation.
    Other(Validation),
}

/// GETs `[start, end)` (with `If-Range` when given); `read_timeout` bounds each wait.
async fn fetch(
    client: &reqwest::Client,
    state: &HttpResumeState,
    if_range: Option<&str>,
    (start, end): (u64, u64),
    read_timeout: Duration,
) -> Result<Fetched, EngineError> {
    let mut rb = client
        .get(state.final_url.clone())
        .header(header::RANGE, format!("bytes={}-{}", start, end - 1));
    if let Some(v) = if_range {
        rb = rb.header(header::IF_RANGE, v);
    }
    let resp = timed(
        read_timeout,
        apply_context(rb, &state.request.context).send(),
    )
    .await?
    .map_err(|e| EngineError::Network(e.to_string()))?;
    let content_range = resp
        .headers()
        .get(header::CONTENT_RANGE)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    match resp.status() {
        StatusCode::PARTIAL_CONTENT => {}
        // Under If-Range a full body means the validator no longer matches; without one,
        // the server stopped honouring ranges.
        StatusCode::OK if if_range.is_some() => return Ok(Fetched::Other(Validation::Changed)),
        StatusCode::OK => return Ok(Fetched::Other(Validation::NotResumable)),
        StatusCode::RANGE_NOT_SATISFIABLE => {
            let total = content_range
                .as_deref()
                .and_then(|v| v.rsplit('/').next())
                .and_then(|t| t.trim().parse().ok());
            return Ok(Fetched::Unsatisfiable { total });
        }
        s => {
            return Err(EngineError::Http { status: s.as_u16() });
        }
    }
    let Some((s, total)) = content_range.as_deref().and_then(parse_content_range) else {
        return Ok(Fetched::Other(Validation::Changed));
    };
    if s != start {
        return Ok(Fetched::Other(Validation::Changed));
    }
    let want = usize::try_from(end - start).unwrap_or(usize::MAX);
    let mut data = Vec::with_capacity(want);
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = timed(read_timeout, stream.next()).await? {
        let chunk = chunk.map_err(|e| EngineError::Network(e.to_string()))?;
        if data.len() + chunk.len() > want {
            // More than requested: not the range we asked for.
            return Ok(Fetched::Other(Validation::Changed));
        }
        data.extend_from_slice(&chunk);
    }
    if data.len() < want {
        return Err(EngineError::Network(
            "validation response ended early".into(),
        ));
    }
    Ok(Fetched::Range { total, data })
}

fn read_range(path: &Path, start: u64, end: u64) -> std::io::Result<Vec<u8>> {
    let mut f = std::fs::File::open(path)?;
    f.seek(SeekFrom::Start(start))?;
    let mut buf = vec![0; usize::try_from(end - start).map_err(std::io::Error::other)?];
    f.read_exact(&mut buf)?;
    Ok(buf)
}

/// Up to three windows of at most `len` bytes at the start, middle and end of the
/// completed bytes, each inside one completed range; duplicates removed.
fn spot_windows(completed: &[(u64, u64)], len: u64) -> Vec<(u64, u64)> {
    let sum: u64 = completed.iter().map(|(s, e)| e - s).sum();
    if sum == 0 {
        return Vec::new();
    }
    // Maps an offset within the completed bytes to (range, absolute offset).
    let locate = |mut k: u64| {
        for &(s, e) in completed {
            if k < e - s {
                return ((s, e), s + k);
            }
            k -= e - s;
        }
        unreachable!("offset inside completed bytes")
    };
    let window = |(s, e): (u64, u64), centre: u64| {
        let n = len.min(e - s);
        let from = centre.saturating_sub(n / 2).clamp(s, e - n);
        (from, from + n)
    };
    let (first, _) = locate(0);
    let (mid_range, mid) = locate(sum / 2);
    let (last, _) = locate(sum - 1);
    let mut out = vec![
        window(first, first.0),
        window(mid_range, mid),
        window(last, last.1),
    ];
    out.dedup();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spot_windows_start_middle_end() {
        assert_eq!(
            spot_windows(&[(0, 1000)], 10),
            vec![(0, 10), (495, 505), (990, 1000)]
        );
    }

    #[test]
    fn spot_windows_clamped_to_completed_ranges() {
        // Completed: [0,100) and [500,520); middle of 120 completed bytes is offset 60.
        assert_eq!(
            spot_windows(&[(0, 100), (500, 520)], 64),
            vec![(0, 64), (28, 92), (500, 520)]
        );
    }

    #[test]
    fn spot_windows_dedupes_small_regions() {
        assert_eq!(spot_windows(&[(10, 15)], 64), vec![(10, 15)]);
        assert!(spot_windows(&[], 64).is_empty());
    }
}
