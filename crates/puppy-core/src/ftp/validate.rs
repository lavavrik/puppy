//! Validation of an FTP location before a download continues from its sidecar, the FTP
//! counterpart of [`crate::http::validate::validate_remote`]: the server's file must be
//! provably the one `.part` holds bytes of.
//!
//! `SIZE` must equal the recorded size. With a recorded modification time (`MDTM`) the
//! server must report the same one, and that suffices. Without one (the server never
//! told, or no longer tells), bytes already downloaded are fetched again at up to three
//! places (start, middle, end of what is done; `spot_check_bytes` each) and compared
//! with `.part`. Network trouble and unexpected refusals are errors (a retry may get
//! through), never `Changed`.

use super::Site;
use super::session::RetrError;
use crate::config::HttpConfig;
use crate::engine::EngineError;
use crate::http::probe::timed;
use crate::http::resume::HttpResumeState;
use crate::http::validate::{Validation, read_range, spot_windows};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::AsyncReadExt;

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
    let mut lease = site.session(cfg).await?;
    let s = lease.get();
    let size = s.size(&path, cfg).await?;
    if size.is_some_and(|n| n != total) {
        return Ok(Validation::Changed);
    }
    let mdtm = match &state.last_modified {
        Some(_) => s.mdtm(&path, cfg).await?,
        None => None,
    };
    lease.keep();
    if let (Some(recorded), Some(now)) = (&state.last_modified, &mdtm) {
        if recorded != now {
            return Ok(Validation::Changed);
        }
        if size.is_some() {
            return Ok(Validation::Ok);
        }
    }
    // No validator to trust: compare downloaded bytes.
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

/// Compares `[start, end)` of `part` with the same bytes of the file at `site`.
async fn compare(
    site: &Arc<Site>,
    path: &str,
    start: u64,
    end: u64,
    part: &Path,
    cfg: &HttpConfig,
) -> Result<Validation, EngineError> {
    let got = match fetch(site, path, start, end, cfg).await? {
        Some(data) => data,
        None => return Ok(Validation::NotResumable),
    };
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

/// The FTP side of [`crate::http::validate::spot_check`]: how a location without a
/// modification time is checked before it serves a range of a download with other
/// locations. Its size must be `total` and `[start, end)` of `part` must match it.
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
        let mut lease = site.session(cfg).await?;
        let size = lease.get().size(&path, cfg).await?;
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

/// The bytes `[start, end)` of the file (fewer if it ends before), `None` if the server
/// refuses to restart a transfer there. The session is closed afterwards (the transfer
/// is cut short).
async fn fetch(
    site: &Arc<Site>,
    path: &str,
    start: u64,
    end: u64,
    cfg: &HttpConfig,
) -> Result<Option<Vec<u8>>, EngineError> {
    let read = Duration::from_millis(cfg.read_timeout_ms);
    let mut lease = site.session(cfg).await?;
    let mut stream = match lease.get().retr(path, start, site.pasv(), cfg).await {
        Ok((stream, switched)) => {
            if let Some(pasv) = switched {
                site.set_pasv(pasv);
            }
            stream
        }
        Err(RetrError::RestRefused) => return Ok(None),
        Err(RetrError::Other(e)) => return Err(e),
    };
    let want = usize::try_from(end - start).unwrap_or(usize::MAX);
    let mut data = vec![0u8; want];
    let mut got = 0;
    while got < want {
        let n = timed(read, stream.read(&mut data[got..]))
            .await?
            .map_err(|e| EngineError::Network(e.to_string()))?;
        if n == 0 {
            // Short: the file ends here if the server says the transfer is complete.
            timed(read, stream.finish())
                .await?
                .map_err(|e| EngineError::Network(format!("transfer ended early: {e}")))?;
            data.truncate(got);
            return Ok(Some(data));
        }
        got += n;
    }
    drop(stream);
    lease.close(cfg).await;
    Ok(Some(data))
}
