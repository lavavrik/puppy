//! Fetching a metalink document over HTTP(S), bounded in size and time.

use super::{ACCEPT, MAX_DOCUMENT_BYTES, MetalinkError};
use crate::config::HttpConfig;
use crate::http::probe::{apply_context, build_client};
use crate::types::{RequestContext, masked_url};
use reqwest::header;
use std::time::Duration;

/// The whole fetch (connecting, redirects, the body) must finish within this.
pub const FETCH_TIMEOUT: Duration = Duration::from_secs(60);

/// A fetched document.
#[derive(Debug, Clone)]
pub struct Fetched {
    pub bytes: Vec<u8>,
    /// The response's `Content-Type`, if it had one.
    pub content_type: Option<String>,
    /// The URL after redirects.
    pub final_url: url::Url,
}

/// GETs `url` with `context` (headers, cookies, user agent) through `cfg`'s proxy and
/// an `Accept` header asking for Metalink. A body larger than [`MAX_DOCUMENT_BYTES`] is
/// refused as soon as that shows (from `Content-Length` or while reading). The error is
/// a sentence for the user.
pub async fn fetch(
    cfg: &HttpConfig,
    url: &url::Url,
    context: &RequestContext,
) -> Result<Fetched, String> {
    let shown = masked_url(url);
    match tokio::time::timeout(FETCH_TIMEOUT, fetch_inner(cfg, url, context)).await {
        Ok(r) => r,
        Err(_) => Err(format!(
            "fetching {shown} took longer than {} s",
            FETCH_TIMEOUT.as_secs()
        )),
    }
}

async fn fetch_inner(
    cfg: &HttpConfig,
    url: &url::Url,
    context: &RequestContext,
) -> Result<Fetched, String> {
    let client = build_client(cfg).map_err(|e| e.to_string())?;
    let mut rb = client.get(url.clone());
    if !context
        .headers
        .iter()
        .any(|(n, _)| n.trim().eq_ignore_ascii_case("accept"))
    {
        rb = rb.header(header::ACCEPT, ACCEPT);
    }
    let mut resp = apply_context(rb, context)
        .send()
        .await
        .map_err(|e| format!("cannot fetch the metalink: {e}"))?;
    let status = resp.status();
    if !status.is_success() {
        return Err(format!(
            "cannot fetch the metalink: HTTP {}",
            status.as_u16()
        ));
    }
    let too_big = || MetalinkError::TooBig.to_string();
    if resp
        .content_length()
        .is_some_and(|n| n > MAX_DOCUMENT_BYTES as u64)
    {
        return Err(too_big());
    }
    let content_type = resp
        .headers()
        .get(header::CONTENT_TYPE)
        .map(|v| String::from_utf8_lossy(v.as_bytes()).trim().to_string())
        .filter(|s| !s.is_empty());
    let final_url = resp.url().clone();
    let mut bytes = Vec::new();
    while let Some(chunk) = resp
        .chunk()
        .await
        .map_err(|e| format!("cannot fetch the metalink: {e}"))?
    {
        if bytes.len() + chunk.len() > MAX_DOCUMENT_BYTES {
            return Err(too_big());
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(Fetched {
        bytes,
        content_type,
        final_url,
    })
}
