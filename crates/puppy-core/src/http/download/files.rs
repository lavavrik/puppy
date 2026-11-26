//! Filesystem steps of a download: reserving `<final>.part` and finalizing it.

use super::mirrors::{
    DropKind, Health, MirrorRt, Validators, kind_of, locations, size_differs, summary,
};
use super::{Inner, SegEntry};
use crate::config::HttpConfig;
use crate::engine::EngineError;
use crate::engine::EngineEvent;
use crate::fsutil::{
    mark_sparse, rename_no_replace, sanitize_filename, sync_parent_dir, unique_path,
};
use crate::http::probe::RemoteInfo;
use crate::http::resume::{part_path, sidecar_path};
use crate::http::segments::Segment;
use crate::types::AddRequest;
use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};

/// Probes every location of the file (the source and its mirrors, concurrently), keeps
/// the ones that agree (see [`choose`]) and reserves `<final>.part` in the output
/// directory.
pub(super) async fn prepare(
    client: &reqwest::Client,
    req: &AddRequest,
    cfg: &HttpConfig,
) -> Result<(RemoteInfo, Vec<MirrorRt>, PathBuf), EngineError> {
    let mut mirrors = locations(req);
    let remotes: Vec<_> = mirrors.iter().map(MirrorRt::remote).collect();
    let probes = futures_util::future::join_all(remotes.iter().map(|r| r.probe(client, cfg))).await;
    let remote = choose(&mut mirrors, probes, req.size)?;
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
    Ok((remote, mirrors, final_path))
}

/// Decides which locations a fresh download uses, from their probes (in the order of
/// `mirrors`, the primary first), and returns the facts about the file.
///
/// The file is what the primary says it is (or, if the primary did not answer, the
/// first mirror that did): its size and name. A location that failed its probe, or has
/// another size, is dropped. Of the rest, the ones that honour ranges are used; if none
/// does (or the size is unknown, so nothing can be compared), one stream goes to the
/// first and the others are dropped. Without any answer, the primary's error (or, with
/// mirrors and some transient error among them, a transient one naming them all).
///
/// With an `expected` size (a Metalink's), a location reporting another size is dropped
/// first, so a stale primary cannot outvote the mirrors; if every location that answered
/// had another size, the download fails (not transiently).
pub(super) fn choose(
    mirrors: &mut [MirrorRt],
    probes: Vec<Result<RemoteInfo, EngineError>>,
    expected: Option<u64>,
) -> Result<RemoteInfo, EngineError> {
    let mut answered = Vec::new();
    let mut first_err = None;
    let mut wrong_size = None;
    for (i, probe) in probes.into_iter().enumerate() {
        match probe {
            Ok(info) if expected.is_some() && info.total.is_some() && info.total != expected => {
                mirrors[i].drop_with(DropKind::Changed, size_unexpected(info.total, expected));
                wrong_size.get_or_insert(info.total);
            }
            Ok(info) => answered.push((i, info)),
            Err(e) => {
                mirrors[i].drop_with(kind_of(&e), format!("probe failed: {e}"));
                first_err.get_or_insert(e);
            }
        }
    }
    let Some((_, reference)) = answered.first().cloned() else {
        let Some(e) = first_err else {
            // Every location answered, with another size than expected.
            let got = wrong_size.flatten().unwrap_or_default();
            let want = expected.unwrap_or_default();
            return Err(EngineError::Invalid(if mirrors.len() == 1 {
                format!("the file is {got} bytes, not the expected {want}")
            } else {
                summary(mirrors)
            }));
        };
        let transient = mirrors.iter().any(|m| {
            matches!(
                m.health,
                Health::Dropped {
                    kind: DropKind::Transient,
                    ..
                }
            )
        });
        if mirrors.len() > 1 && transient {
            return Err(EngineError::Unavailable(summary(mirrors)));
        }
        return Err(e);
    };
    let total = reference.total;
    let mut agreeing = Vec::new();
    for (i, info) in answered {
        if total.is_some() && info.total == total {
            agreeing.push((i, info));
        } else if total.is_none() && agreeing.is_empty() {
            agreeing.push((i, info)); // the reference itself
        } else if total.is_none() {
            mirrors[i].drop_with(
                DropKind::Fatal,
                "the file's size is unknown, so mirrors cannot be compared",
            );
        } else {
            mirrors[i].drop_with(DropKind::Changed, size_differs(info.total, total));
        }
    }
    let ranged = total.is_some() && agreeing.iter().any(|(_, info)| info.accepts_ranges);
    for (n, (i, info)) in agreeing.iter().enumerate() {
        if ranged && !info.accepts_ranges {
            mirrors[*i].drop_with(DropKind::RangeUnsupported, "does not honour range requests");
        } else if !ranged && n > 0 {
            mirrors[*i].drop_with(
                DropKind::RangeUnsupported,
                "one stream only: no location honours range requests",
            );
        } else {
            mirrors[*i].v = Validators::of(info);
        }
    }
    Ok(RemoteInfo {
        accepts_ranges: ranged || reference.accepts_ranges,
        ..reference
    })
}

/// Why a location with `got` bytes is not the file of `want` bytes the request expects.
fn size_unexpected(got: Option<u64>, want: Option<u64>) -> String {
    let show = |n: Option<u64>| n.map_or("unknown".to_string(), |n| format!("{n} bytes"));
    format!(
        "its size ({}) is not the expected {}",
        show(got),
        show(want)
    )
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
        // NTFS would otherwise zero-fill up to every out-of-order write synchronously.
        if let Err(e) = mark_sparse(&file) {
            tracing::warn!(path = %part.display(), "cannot mark sparse: {e}");
        }
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

/// fsyncs the data, renames `.part` to the final name, then removes the sidecar (kept
/// when the request has a checksum: see `verify`), and marks the download finalized.
/// Never replaces a file that appeared at the final name meanwhile: the download is then
/// finalized under a free name, recorded as its `path` (with a `StateChanged` event).
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
    let keep_sidecar = inner.request.checksum.is_some();
    let target = blocking(move || {
        let part = part_path(&final_path);
        let file = OpenOptions::new().read(true).write(true).open(&part)?;
        if total.is_none() {
            file.set_len(len)?;
        } else if file.metadata()?.len() != len {
            return Err(io::Error::other("partial file has the wrong length"));
        }
        file.sync_all()?;
        drop(file); // Windows: no handle may be open during rename.
        let target = rename_part(&part, &final_path)?;
        if let Err(e) = sync_parent_dir(&target) {
            // The data itself is fsynced; at worst the rename is not yet durable.
            tracing::warn!(path = %target.display(), "cannot sync directory: {e}");
        }
        // Renaming first means a crash here leaves the finished file plus a stale sidecar,
        // never an unrenamed file without its sidecar. The download is already complete
        // here, so a sidecar that cannot be removed is only logged.
        if !keep_sidecar {
            remove_sidecar_best_effort(&sidecar_path(&final_path));
        }
        Ok(target)
    })
    .await?;
    inner.lock().finalized = true;
    if target != inner.final_path {
        let state = {
            let mut st = inner.lock();
            st.path = target;
            st.state.clone()
        };
        let _ = inner.events.send(EngineEvent::StateChanged {
            id: inner.id,
            state,
        });
    }
    Ok(())
}

/// Renames `part` to `final_path`, or to a free variant of it if that name is taken.
fn rename_part(part: &Path, final_path: &Path) -> io::Result<PathBuf> {
    let mut target = final_path.to_path_buf();
    for _ in 0..1000 {
        match rename_no_replace(part, &target) {
            Ok(()) => return Ok(target),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                // The sidecar goes first: after a crash, a sidecar next to a file at
                // `final_path` would claim that (foreign) file as this finished download.
                remove_if_exists(&sidecar_path(final_path))?;
                let dir = final_path.parent().unwrap_or(Path::new("."));
                let name = final_path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                target = unique_path(dir, &name);
            }
            Err(e) => return Err(e),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "no free file name found",
    ))
}

/// Removes a sidecar after a successful rename; failure is logged, never fatal.
pub(super) fn remove_sidecar_best_effort(path: &Path) {
    if let Err(e) = remove_if_exists(path) {
        tracing::warn!(path = %path.display(), "cannot remove sidecar after finishing: {e}");
    }
}

fn remove_if_exists(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::remove_sidecar_best_effort;

    #[test]
    fn missing_sidecar_is_fine() {
        let dir = tempfile::tempdir().unwrap();
        remove_sidecar_best_effort(&dir.path().join("absent.puppy"));
    }

    #[test]
    fn present_sidecar_is_removed() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("a.puppy");
        std::fs::write(&p, b"x").unwrap();
        remove_sidecar_best_effort(&p);
        assert!(!p.exists());
    }

    #[test]
    fn unremovable_sidecar_does_not_error_or_panic() {
        // remove_file on a directory fails on every platform.
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("a.puppy");
        std::fs::create_dir(&p).unwrap();
        remove_sidecar_best_effort(&p);
        assert!(p.exists());
    }
}
