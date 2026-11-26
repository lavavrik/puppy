//! What depends on a location's protocol: probing it and validating it before a resume
//! go over HTTP(S) or FTP(S) as its URL says (segments too: see `transfer`). Locations
//! of one download may mix protocols; each is probed and validated on its own.

use super::mirrors::MirrorRt;
use crate::config::HttpConfig;
use crate::engine::EngineError;
use crate::http::probe::{RemoteInfo, probe};
use crate::http::resume::HttpResumeState;
use crate::http::validate::{Validation, validate_remote};
use crate::types::RequestContext;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

/// What requests to one location need, taken out of its [`MirrorRt`] (which lives under
/// the download's lock).
#[derive(Clone)]
pub(super) struct Remote {
    /// The location's URL as given.
    url: url::Url,
    context: RequestContext,
    ftp: Option<Arc<crate::ftp::Site>>,
}

impl MirrorRt {
    pub(super) fn remote(&self) -> Remote {
        Remote {
            url: self.url.clone(),
            context: self.context.clone(),
            ftp: self.ftp.clone(),
        }
    }
}

impl Remote {
    pub(super) fn is_ftp(&self) -> bool {
        self.ftp.is_some()
    }

    /// Size, range support, validators and name of the file at this location.
    pub(super) async fn probe(
        &self,
        client: &reqwest::Client,
        cfg: &HttpConfig,
    ) -> Result<RemoteInfo, EngineError> {
        match &self.ftp {
            Some(site) => crate::ftp::probe(site, cfg).await,
            None => {
                let read_timeout = Duration::from_millis(cfg.read_timeout_ms);
                probe(client, &self.url, &self.context, read_timeout).await
            }
        }
    }

    /// Whether this location still serves the file `part` holds bytes of, as `state`
    /// records it (with this location's validators; HTTP requests go to its
    /// `final_url`).
    pub(super) async fn validate(
        &self,
        client: &reqwest::Client,
        state: &HttpResumeState,
        part: &Path,
        cfg: &HttpConfig,
    ) -> Result<Validation, EngineError> {
        match &self.ftp {
            Some(site) => crate::ftp::validate::validate_remote(site, state, part, cfg).await,
            None => validate_remote(client, &self.context, &self.url, state, part, cfg).await,
        }
    }
}
