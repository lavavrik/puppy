//! Fetching a metalink document over HTTP(S), bounded in size and time.
//!
//! Redirects are followed by [`crate::http::redirect`], as for downloads, but scoped more
//! strictly: the user's headers, cookies, referer and credentials go to the origin of the
//! URL the user gave, and a hop on any other origin gets the user agent alone (as a
//! download's mirror on another origin does), since the document it serves decides where
//! the downloads go. A redirect from HTTPS to plain HTTP is refused: the document
//! supplies the hashes the downloads are checked against.

use super::{ACCEPT, MAX_DOCUMENT_BYTES, MetalinkError};
use crate::config::HttpConfig;
use crate::http::probe::build_client;
use crate::http::redirect::{RedirectError, follow};
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

/// GETs `url` with `context` (headers, cookies, user agent; on `url`'s origin only, see
/// the module) through `cfg`'s proxy and an `Accept` header asking for Metalink. A body
/// larger than [`MAX_DOCUMENT_BYTES`] is refused as soon as that shows (from
/// `Content-Length` or while reading). The error is a sentence for the user.
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

/// Whether a metalink fetch of `requested` may follow a redirect from `current` to
/// `next`: only to HTTP(S), and never from HTTPS down to plain HTTP.
fn check_redirect(requested: &url::Url, current: &url::Url, next: &url::Url) -> Result<(), String> {
    if !matches!(next.scheme(), "http" | "https") {
        return Err(format!(
            "cannot fetch the metalink: it redirects to a {}: URL",
            next.scheme()
        ));
    }
    if current.scheme() == "https" && next.scheme() == "http" {
        return Err(format!(
            "refused: the metalink at {} redirects from HTTPS to plain HTTP ({}), where its \
             hashes could be changed on the way",
            masked_url(requested),
            masked_url(next)
        ));
    }
    Ok(())
}

async fn fetch_inner(
    cfg: &HttpConfig,
    url: &url::Url,
    context: &RequestContext,
) -> Result<Fetched, String> {
    let client = build_client(cfg).map_err(|e| e.to_string())?;
    let mut resp = follow(
        url,
        |hop, ctx| {
            let rb = client.get(hop.clone());
            // The user's own Accept (on the given origin) replaces ours.
            if ctx
                .headers
                .iter()
                .any(|(n, _)| n.trim().eq_ignore_ascii_case("accept"))
            {
                rb
            } else {
                rb.header(header::ACCEPT, ACCEPT)
            }
        },
        // Everything on the given URL's origin; the user agent alone elsewhere.
        |hop| context.for_mirror(url, hop),
        |current, next| check_redirect(url, current, next),
    )
    .await
    .map_err(|e| match e {
        RedirectError::Refused(why) => why,
        e => format!("cannot fetch the metalink: {e}"),
    })?;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redirects_never_downgrade() {
        let u = |s: &str| url::Url::parse(s).unwrap();
        let req = u("https://a.example/f.meta4");
        assert!(check_redirect(&req, &req, &u("https://b.example/f.meta4")).is_ok());
        let e = check_redirect(&req, &req, &u("http://a.example/f.meta4")).unwrap_err();
        assert!(e.contains("from HTTPS to plain HTTP"), "{e}");
        // Down again after a hop elsewhere: refused too.
        let mid = u("https://c.example/x");
        assert!(check_redirect(&req, &mid, &u("http://c.example/y")).is_err());
        // Plain HTTP from the start: the user chose it.
        let plain = u("http://a.example/f.meta4");
        assert!(check_redirect(&plain, &plain, &u("http://b.example/f")).is_ok());
        assert!(check_redirect(&plain, &plain, &u("https://b.example/f")).is_ok());
        assert!(check_redirect(&req, &req, &u("file:///etc/passwd")).is_err());
    }
}
