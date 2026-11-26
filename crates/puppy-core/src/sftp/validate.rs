//! Validation of an SFTP location before a download continues from its sidecar, the
//! SFTP counterpart of [`crate::http::validate::validate_remote`]: the size must equal
//! the recorded one and, with a recorded modification time, the server must report the
//! same one. Without one, bytes already downloaded are fetched again at up to three
//! places and compared with `.part` (as FTP does without `MDTM`). Network trouble is an
//! error (a retry may get through), never `Changed`.

use super::{Site, mtime_validator};
use crate::config::HttpConfig;
use crate::engine::EngineError;
use crate::http::resume::HttpResumeState;
use crate::http::validate::{Validation, read_range, spot_windows};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

pub(crate) async fn validate_remote(
    site: &Arc<Site>,
    state: &HttpResumeState,
    part: &Path,
    cfg: &HttpConfig,
) -> Result<Validation, EngineError> {
    let limit = Duration::from_millis(cfg.validate_timeout_ms);
    tokio::time::timeout(limit, validate(site, state, part, cfg))
        .await
        .unwrap_or_else(|_| Err(EngineError::Network("validation timed out".into())))
}

async fn validate(
    site: &Arc<Site>,
    state: &HttpResumeState,
    part: &Path,
    cfg: &HttpConfig,
) -> Result<Validation, EngineError> {
    let total = match state.total {
        Some(t) if !state.single_stream => t,
        _ => return Ok(Validation::NotResumable),
    };
    let path = site.endpoint()?.path.clone();
    let mut lease = site.conn(cfg).await?;
    let attrs = lease.get().stat(&path, cfg).await?;
    lease.keep();
    if attrs.size.is_some_and(|n| n != total) {
        return Ok(Validation::Changed);
    }
    if let (Some(recorded), Some(now)) = (&state.last_modified, mtime_validator(attrs.mtime)) {
        if *recorded != now {
            return Ok(Validation::Changed);
        }
        if attrs.size.is_some() {
            return Ok(Validation::Ok);
        }
    }
    let completed: Vec<(u64, u64)> = state
        .segments
        .iter()
        .filter(|s| s.done > 0)
        .map(|s| (s.start, s.start + s.done))
        .collect();
    for (start, end) in spot_windows(&completed, cfg.spot_check_bytes.max(1)) {
        match compare(site, &path, start, end, part, cfg).await? {
            Validation::Ok => {}
            other => return Ok(other),
        }
    }
    Ok(Validation::Ok)
}

/// How an SFTP location without a modification time is checked before it serves a
/// range of a download with other locations: its size must be `total` and `[start,
/// end)` of `part` must match it.
pub(crate) async fn spot_check(
    site: &Arc<Site>,
    total: u64,
    start: u64,
    end: u64,
    part: &Path,
    cfg: &HttpConfig,
) -> Result<Validation, EngineError> {
    let check = async {
        let path = site.endpoint()?.path.clone();
        let mut lease = site.conn(cfg).await?;
        let size = lease.get().stat(&path, cfg).await?.size;
        lease.keep();
        if size.is_some_and(|n| n != total) {
            return Ok(Validation::Changed);
        }
        compare(site, &path, start, end, part, cfg).await
    };
    let limit = Duration::from_millis(cfg.validate_timeout_ms);
    tokio::time::timeout(limit, check)
        .await
        .unwrap_or_else(|_| Err(EngineError::Network("check timed out".into())))
}

/// Compares `[start, end)` of `part` with the same bytes of the file at `site`.
async fn compare(
    site: &Arc<Site>,
    path: &str,
    start: u64,
    end: u64,
    part: &Path,
    cfg: &HttpConfig,
) -> Result<Validation, EngineError> {
    let got = fetch(site, path, start, end, cfg).await?;
    if got.len() as u64 != end - start {
        // The file ends before bytes already downloaded: it shrank.
        return Ok(Validation::Changed);
    }
    let file = part.to_path_buf();
    let local = tokio::task::spawn_blocking(move || read_range(&file, start, end))
        .await
        .map_err(std::io::Error::other)??;
    Ok(if got == local {
        Validation::Ok
    } else {
        Validation::Changed
    })
}

/// The bytes `[start, end)` of the file (fewer if it ends before).
async fn fetch(
    site: &Arc<Site>,
    path: &str,
    start: u64,
    end: u64,
    cfg: &HttpConfig,
) -> Result<Vec<u8>, EngineError> {
    let mut lease = site.conn(cfg).await?;
    let conn = lease.get();
    let h = conn.open_file(path, cfg).await?;
    let mut data = Vec::new();
    let mut off = start;
    let read = Duration::from_millis(cfg.read_timeout_ms);
    let result = async {
        while off < end {
            let len =
                u32::try_from((end - off).min(u64::from(conn.read_len))).unwrap_or(conn.read_len);
            match crate::http::probe::timed(read, conn.read(&h, path, off, len)).await?? {
                Some(d) if d.len() as u64 > u64::from(len) => {
                    return Err(EngineError::Ssh(format!(
                        "SFTP: the server sent {} bytes for a {len}-byte read at offset {off}",
                        d.len()
                    )));
                }
                Some(d) if !d.is_empty() => {
                    off += d.len() as u64;
                    data.extend_from_slice(&d);
                }
                _ => break,
            }
        }
        Ok::<(), EngineError>(())
    }
    .await;
    conn.close_file(h, cfg).await;
    result?;
    lease.keep();
    data.truncate(usize::try_from(end - start).unwrap_or(usize::MAX));
    Ok(data)
}
