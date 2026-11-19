//! Filesystem steps of a download: reserving `<final>.part` and finalizing it.

use super::{Inner, SegEntry};
use crate::config::HttpConfig;
use crate::engine::EngineError;
use crate::fsutil::{sanitize_filename, sync_parent_dir, unique_path};
use crate::http::probe::{RemoteInfo, probe};
use crate::http::resume::{part_path, sidecar_path};
use crate::http::segments::Segment;
use crate::types::AddRequest;
use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};

/// Probes the remote and reserves `<final>.part` in the output directory.
pub(super) async fn prepare(
    client: &reqwest::Client,
    url: &url::Url,
    req: &AddRequest,
    cfg: &HttpConfig,
) -> Result<(RemoteInfo, PathBuf), EngineError> {
    let remote = probe(client, url, &req.context).await?;
    let dir = req
        .output_dir
        .clone()
        .unwrap_or_else(|| cfg.default_dir.clone());
    let name = req
        .filename
        .as_deref()
        .map(sanitize_filename)
        .unwrap_or_else(|| remote.filename.clone());
    let total = remote.total;
    let final_path = blocking(move || create_part(&dir, &name, total)).await?;
    Ok((remote, final_path))
}

/// Picks a free name and creates its `.part` with `create_new` (retrying if another
/// process takes the name in between), preallocated to `total` when known.
fn create_part(dir: &Path, name: &str, total: Option<u64>) -> io::Result<PathBuf> {
    fs::create_dir_all(dir)?;
    for _ in 0..1000 {
        let final_path = unique_path(dir, name);
        let part = part_path(&final_path);
        let file = match OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&part)
        {
            Ok(f) => f,
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        };
        if let Some(t) = total
            && let Err(e) = file.set_len(t)
        {
            drop(file);
            let _ = fs::remove_file(&part);
            return Err(e);
        }
        return Ok(final_path);
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "no free file name found",
    ))
}

pub(super) fn open_part(final_path: &Path) -> io::Result<File> {
    // Read+write: `sync_data` needs a write-capable handle on Windows.
    OpenOptions::new()
        .read(true)
        .write(true)
        .open(part_path(final_path))
}

/// Length of a regular file, `None` if there is none.
pub(super) async fn file_len(path: &Path) -> io::Result<Option<u64>> {
    match tokio::fs::metadata(path).await {
        Ok(m) if m.is_file() => Ok(Some(m.len())),
        Ok(_) => Ok(None),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

pub(super) async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> io::Result<T> + Send + 'static,
) -> io::Result<T> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(io::Error::other)?
}

/// fsyncs the data, renames `.part` to the final name, then removes the sidecar.
/// Must only run after `transfer` returned (no open handles).
pub(super) async fn finalize(inner: &Inner) -> io::Result<()> {
    let total = inner.remote.total;
    let len = {
        let st = inner.lock();
        let segs: Vec<Segment> = st.plan.segments.iter().map(SegEntry::snapshot).collect();
        if st.plan.single {
            let done = segs[0].done;
            if total.is_some_and(|t| t != done) {
                return Err(io::Error::other("single stream ended short"));
            }
            done
        } else {
            let t = total.unwrap_or(0);
            let mut cursor = 0;
            for s in &segs {
                if s.start != cursor || !s.is_complete() {
                    return Err(io::Error::other("segments do not cover the file"));
                }
                cursor = s.end;
            }
            if cursor != t {
                return Err(io::Error::other("segments do not cover the file"));
            }
            t
        }
    };
    let final_path = inner.final_path.clone();
    blocking(move || {
        let part = part_path(&final_path);
        let file = OpenOptions::new().read(true).write(true).open(&part)?;
        if total.is_none() {
            file.set_len(len)?;
        } else if file.metadata()?.len() != len {
            return Err(io::Error::other("partial file has the wrong length"));
        }
        file.sync_all()?;
        drop(file); // Windows: no handle may be open during rename.
        fs::rename(&part, &final_path)?;
        sync_parent_dir(&final_path)?;
        // Renaming first means a crash here leaves the finished file plus a stale sidecar,
        // never an unrenamed file without its sidecar.
        if let Err(e) = fs::remove_file(sidecar_path(&final_path))
            && e.kind() != io::ErrorKind::NotFound
        {
            tracing::warn!("cannot remove resume sidecar: {e}");
        }
        Ok(())
    })
    .await
}
