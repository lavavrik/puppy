//! Snapshots of the download list for clients.

use super::{Entry, Inner, Manager, ManagerError, is_active, is_finished, is_seeding};
use puppy_core::engine::EngineKind;
use puppy_core::types::{DownloadId, DownloadState, EngineDetail, Source};
use puppy_proto::{DownloadSnapshot, EngineKindTag, Totals};

/// How a source is shown: the URL (with any password masked), the magnet URI or the
/// .torrent path.
fn source_display(source: &Source) -> String {
    match source {
        Source::Http { url } if url.password().is_some() => {
            let mut shown = url.clone();
            // Only fails for URLs that cannot have credentials, which this one has.
            let _ = shown.set_password(Some("***"));
            shown.to_string()
        }
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
    /// otherwise the stored record. The state is always the manager's (a seeding one's
    /// counters are the engine's latest); a seeding download shows all its data done.
    ///
    /// The engine detail is fetched only if `with_detail` or for HTTP downloads (their
    /// connection count comes from it); a torrent's peer count comes from its status, so
    /// listing torrents never builds their piece bitfields.
    fn snap(
        &self,
        id: DownloadId,
        e: &Entry,
        queue_position: Option<usize>,
        with_detail: bool,
    ) -> (DownloadSnapshot, Option<EngineDetail>) {
        let engine = e.engine.filter(|_| e.held).map(|i| &self.engines[i]);
        let status = engine.and_then(|en| en.status(id));
        let detail = engine
            .filter(|en| with_detail || en.kind() == EngineKind::Http)
            .and_then(|en| en.detail(id));
        let (connections, detail_peers) = match &detail {
            Some(EngineDetail::Http { segments, .. }) => {
                let active = segments.iter().filter(|s| s.active).count();
                (Some(u16::try_from(active).unwrap_or(u16::MAX)), None)
            }
            Some(EngineDetail::Torrent { peers, .. }) => (None, Some(*peers)),
            Some(EngineDetail::Unknown) | None => (None, None),
        };
        let peers = status.as_ref().and_then(|s| s.peers).or(detail_peers);
        let total_bytes = status
            .as_ref()
            .and_then(|s| s.total_bytes)
            .or(e.rec.total_bytes);
        let done_bytes = match (&status, &e.state) {
            (_, DownloadState::Seeding { .. }) => total_bytes.unwrap_or(0),
            (Some(s), _) => s.done_bytes,
            (None, DownloadState::Completed) => total_bytes.unwrap_or(0),
            (None, _) => 0,
        };
        let state = match &status {
            Some(s) if is_seeding(&e.state) && is_seeding(&s.state) => (&s.state).into(),
            _ => (&e.state).into(),
        };
        let snap = DownloadSnapshot {
            id,
            kind: self.kind_tag(e),
            source: source_display(&e.rec.request.source),
            name: status
                .as_ref()
                .filter(|s| !(e.is_placeholder_name(s.total_bytes) && e.rec.name.is_some()))
                .map(|s| s.name.clone())
                .or_else(|| e.rec.name.clone())
                .unwrap_or_else(|| fallback_name(e)),
            path: status
                .as_ref()
                .map(|s| s.path.clone())
                .or_else(|| e.rec.path.clone()),
            state,
            total_bytes,
            done_bytes,
            down_bps: status.as_ref().map_or(0, |s| s.down_bps),
            up_bps: status.as_ref().map_or(0, |s| s.up_bps),
            eta_secs: status.as_ref().and_then(|s| s.eta_secs),
            limit: e.rec.limit,
            queue_position,
            connections,
            peers,
            next_retry_unix: e.retry.map(|r| r.unix),
            note: status.as_ref().and_then(|s| s.note.clone()),
            seed: e.rec.request.seed.clone(),
        };
        (snap, detail)
    }
}

impl Manager {
    /// The name a snapshot shows for `id` (`None`: unknown id).
    pub fn display_name(&self, id: DownloadId) -> Option<String> {
        let inner = &self.inner;
        let st = inner.lock();
        let e = st.entries.get(&id)?;
        Some(inner.snap(id, e, None, false).0.name)
    }

    /// Every download in queue order, finished ones (completed or seeding) last (most
    /// recently finished first), plus totals (seeding counts as completed, choosing
    /// files as paused).
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
            let (snap, _) = inner.snap(*id, e, pos, false);
            totals.down_bps += snap.down_bps;
            totals.up_bps += snap.up_bps;
            match &e.state {
                s if is_active(s) => totals.active += 1,
                DownloadState::Queued => totals.queued += 1,
                DownloadState::Paused | DownloadState::ChoosingFiles => totals.paused += 1,
                s if is_finished(s) => totals.completed += 1,
                _ => totals.failed += 1,
            }
            if is_finished(&e.state) {
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
        Ok(inner.snap(id, e, pos, true))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn http(url: &str) -> Source {
        Source::Http {
            url: url.parse().unwrap(),
        }
    }

    #[test]
    fn passwords_are_masked() {
        assert_eq!(
            source_display(&http("https://user:s3cret@host/f.bin?x=1")),
            "https://user:***@host/f.bin?x=1"
        );
        assert_eq!(
            source_display(&http("https://user@host/f.bin")),
            "https://user@host/f.bin"
        );
        assert_eq!(
            source_display(&http("http://host/f.bin")),
            "http://host/f.bin"
        );
    }
}
