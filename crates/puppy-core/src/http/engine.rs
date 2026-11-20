//! `HttpEngine`: the `Engine` implementation managing many [`HttpDownload`]s by id.
//!
//! Dropping the engine does not stop running downloads (their tasks are detached):
//! `pause` or `remove` them first.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use tokio::sync::broadcast;

use crate::config::HttpConfig;
use crate::engine::{AddOptions, Added, Engine, EngineError, EngineEvent, EngineKind, StartMode};
use crate::http::download::HttpDownload;
use crate::http::probe::build_client;
use crate::limiter::TokenBucket;
use crate::types::{AddRequest, DownloadId, DownloadStatus, EngineDetail, Source};

pub struct HttpEngine {
    client: reqwest::Client,
    cfg: HttpConfig,
    global: TokenBucket,
    /// `None` marks an id reserved by an `add` still probing/validating.
    downloads: Mutex<HashMap<DownloadId, Option<Arc<HttpDownload>>>>,
    events: broadcast::Sender<EngineEvent>,
}

impl HttpEngine {
    pub fn new(cfg: HttpConfig) -> Result<Self, EngineError> {
        let client = build_client(&cfg)?;
        let (events, _) = broadcast::channel(256);
        Ok(Self {
            client,
            cfg,
            global: TokenBucket::new(None),
            downloads: Mutex::new(HashMap::new()),
            events,
        })
    }

    fn get(&self, id: DownloadId) -> Result<Arc<HttpDownload>, EngineError> {
        self.downloads
            .lock()
            .unwrap()
            .get(&id)
            .and_then(Clone::clone)
            .ok_or(EngineError::NotFound(id))
    }
}

type Downloads = Mutex<HashMap<DownloadId, Option<Arc<HttpDownload>>>>;

/// Releases a reserved id if `add` fails or its future is dropped mid-await.
struct Reservation<'a> {
    map: &'a Downloads,
    id: DownloadId,
    armed: bool,
}

impl Drop for Reservation<'_> {
    fn drop(&mut self) {
        if self.armed {
            self.map.lock().unwrap().remove(&self.id);
        }
    }
}

#[async_trait::async_trait]
impl Engine for HttpEngine {
    fn kind(&self) -> EngineKind {
        EngineKind::Http
    }

    fn accepts(&self, source: &Source) -> bool {
        matches!(source, Source::Http { url } if matches!(url.scheme(), "http" | "https"))
    }

    async fn add(
        &self,
        id: DownloadId,
        req: AddRequest,
        mode: StartMode,
        opts: AddOptions,
    ) -> Result<Added, EngineError> {
        if !self.accepts(&req.source) {
            return Err(EngineError::Unsupported("not an HTTP(S) source"));
        }
        {
            let mut map = self.downloads.lock().unwrap();
            if map.contains_key(&id) {
                return Err(EngineError::Invalid(format!("duplicate download id {id}")));
            }
            map.insert(id, None);
        }
        let (client, cfg, global, events) = (
            self.client.clone(),
            self.cfg.clone(),
            self.global.clone(),
            self.events.clone(),
        );
        let mut guard = Reservation {
            map: &self.downloads,
            id,
            armed: true,
        };
        let result = match mode {
            StartMode::Fresh => {
                HttpDownload::start_fresh(id, req, opts, client, cfg, global, events).await
            }
            StartMode::Restore { path } => {
                HttpDownload::restore(id, req, &path, opts, client, cfg, global, events).await
            }
        };
        let (dl, added) = result?; // the guard releases the id on error
        self.downloads
            .lock()
            .unwrap()
            .insert(id, Some(Arc::new(dl)));
        guard.armed = false;
        Ok(added)
    }

    async fn pause(&self, id: DownloadId) -> Result<(), EngineError> {
        self.get(id)?.pause().await;
        Ok(())
    }

    async fn resume(&self, id: DownloadId) -> Result<(), EngineError> {
        self.get(id)?.resume().await
    }

    async fn remove(&self, id: DownloadId, delete_files: bool) -> Result<(), EngineError> {
        let dl = self.get(id)?;
        dl.cancel(delete_files).await;
        self.downloads.lock().unwrap().remove(&id);
        Ok(())
    }

    fn status(&self, id: DownloadId) -> Option<DownloadStatus> {
        self.get(id).ok().map(|d| d.status())
    }

    fn detail(&self, id: DownloadId) -> Option<EngineDetail> {
        self.get(id).ok().map(|d| d.detail())
    }

    fn set_download_limit(&self, id: DownloadId, bps: Option<u64>) -> Result<(), EngineError> {
        self.get(id)?.set_limit(bps);
        Ok(())
    }

    fn set_global_download_budget(&self, bps: Option<u64>) {
        self.global.set_rate(bps);
    }

    fn subscribe(&self) -> broadcast::Receiver<EngineEvent> {
        self.events.subscribe()
    }
}
