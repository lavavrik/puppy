//! Snapshots of the download list for clients.

use super::{Entry, Inner, Manager, ManagerError, is_active};
use puppy_core::engine::EngineKind;
use puppy_core::types::{DownloadId, DownloadState, EngineDetail, Source};
use puppy_proto::{DownloadSnapshot, EngineKindTag, Totals};

/// How a source is shown: the URL, the magnet URI or the .torrent path.
fn source_display(source: &Source) -> String {
    match source {
        Source::Http { url } => url.to_string(),
        Source::Magnet { uri } => uri.clone(),
        Source::TorrentFile { path } => path.display().to_string(),
    }
}

/// A name for a download whose engine has not named it yet.
fn fallback_name(e: &Entry) -> String {
    if let Some(name) = &e.rec.request.filename {
        return name.clone();
    }
    match &e.rec.request.source {
        Source::Http { url } => url
            .path_segments()
            .and_then(|mut s| s.rfind(|seg| !seg.is_empty()))
            .map(str::to_string)
            .unwrap_or_else(|| url.to_string()),
        other => source_display(other),
    }
}

impl Inner {
    fn kind_tag(&self, e: &Entry) -> EngineKindTag {
        let kind = match e.engine {
            Some(engine) => self.engines[engine].kind(),
            None if matches!(e.rec.request.source, Source::Http { .. }) => EngineKind::Http,
            None => EngineKind::Torrent,
        };
        match kind {
            EngineKind::Http => EngineKindTag::Http,
            EngineKind::Torrent => EngineKindTag::Torrent,
        }
    }

    /// One download's snapshot: live numbers from the engine if it holds the download,
    /// otherwise the stored record. The state is always the manager's.
    fn snap(
        &self,
        id: DownloadId,
        e: &Entry,
        queue_position: Option<usize>,
    ) -> (DownloadSnapshot, Option<EngineDetail>) {
        let engine = e.engine.filter(|_| e.held).map(|i| &self.engines[i]);
        let status = engine.and_then(|en| en.status(id));
        let detail = engine.and_then(|en| en.detail(id));
        let (connections, peers) = match &detail {
            Some(EngineDetail::Http { segments, .. }) => {
                let active = segments.iter().filter(|s| s.active).count();
                (Some(u16::try_from(active).unwrap_or(u16::MAX)), None)
            }
            Some(EngineDetail::Torrent { peers, .. }) => (None, Some(*peers)),
            None => (None, None),
        };
        let total_bytes = status
            .as_ref()
            .and_then(|s| s.total_bytes)
            .or(e.rec.total_bytes);
        let done_bytes = match (&status, &e.state) {
            (Some(s), _) => s.done_bytes,
            (None, DownloadState::Completed) => total_bytes.unwrap_or(0),
            (None, _) => 0,
        };
        let snap = DownloadSnapshot {
            id,
            kind: self.kind_tag(e),
            source: source_display(&e.rec.request.source),
            name: status
                .as_ref()
                .map(|s| s.name.clone())
                .or_else(|| e.rec.name.clone())
                .unwrap_or_else(|| fallback_name(e)),
            path: status
                .as_ref()
                .map(|s| s.path.clone())
                .or_else(|| e.rec.path.clone()),
            state: (&e.state).into(),
            total_bytes,
            done_bytes,
            down_bps: status.as_ref().map_or(0, |s| s.down_bps),
            up_bps: status.as_ref().map_or(0, |s| s.up_bps),
            eta_secs: status.as_ref().and_then(|s| s.eta_secs),
            limit: e.rec.limit,
            queue_position,
            connections,
            peers,
        };
        (snap, detail)
    }
}

impl Manager {
    /// Every download in queue order, completed ones last (most recently finished
    /// first), plus totals.
    pub fn snapshot(&self) -> (Vec<DownloadSnapshot>, Totals) {
        let inner = &self.inner;
        let st = inner.lock();
        let mut list = Vec::with_capacity(st.order.len());
        let mut done = Vec::new();
        let mut totals = Totals::default();
        let mut queued = 0;
        for id in &st.order {
            let Some(e) = st.entries.get(id) else {
                continue;
            };
            let pos = (e.state == DownloadState::Queued).then(|| {
                queued += 1;
                queued - 1
            });
            let (snap, _) = inner.snap(*id, e, pos);
            totals.down_bps += snap.down_bps;
            totals.up_bps += snap.up_bps;
            match &e.state {
                s if is_active(s) => totals.active += 1,
                DownloadState::Queued => totals.queued += 1,
                DownloadState::Paused => totals.paused += 1,
                DownloadState::Completed => totals.completed += 1,
                _ => totals.failed += 1,
            }
            if e.state == DownloadState::Completed {
                done.push((e.rec.finished_unix, snap));
            } else {
                list.push(snap);
            }
        }
        done.sort_by(|a, b| b.0.cmp(&a.0).then(b.1.id.cmp(&a.1.id)));
        list.extend(done.into_iter().map(|(_, s)| s));
        (list, totals)
    }

    /// One download with its engine detail (if the engine holds it).
    pub fn get(
        &self,
        id: DownloadId,
    ) -> Result<(DownloadSnapshot, Option<EngineDetail>), ManagerError> {
        let inner = &self.inner;
        let st = inner.lock();
        let e = st.entries.get(&id).ok_or(ManagerError::NotFound(id))?;
        let pos = (e.state == DownloadState::Queued).then(|| {
            st.order
                .iter()
                .take_while(|x| **x != id)
                .filter(|x| {
                    st.entries
                        .get(x)
                        .is_some_and(|e| e.state == DownloadState::Queued)
                })
                .count()
        });
        Ok(inner.snap(id, e, pos))
    }
}
