//! Following redirects so that each hop gets only the request context it may.
//!
//! Clients from [`build_client`](super::probe::build_client) do not follow redirects:
//! reqwest's own following re-sends every header set on the request to every hop (it
//! strips only `Cookie` and the `Authorization` headers on another host), so a
//! `-H 'X-Api-Key: …'` would reach whatever host the first one redirects to. Every
//! request that carries a download's context goes through [`send`] instead, which follows
//! redirects itself and scopes the context to each hop with
//! [`RequestContext::for_target`], always against the URL the context was given for. A
//! request that bypasses it gets the redirect response back rather than following it.

use super::probe::apply_context;
use crate::types::RequestContext;
use percent_encoding::percent_decode_str;
use reqwest::header;
use reqwest::{RequestBuilder, Response, StatusCode};

/// At most this many redirects are followed (as reqwest's default policy did).
pub const MAX_REDIRECTS: usize = 10;

/// Why a request (or one of its redirects) failed.
#[derive(Debug, thiserror::Error)]
pub enum RedirectError {
    #[error(transparent)]
    Send(#[from] reqwest::Error),
    #[error("more than {MAX_REDIRECTS} redirects")]
    TooMany,
    #[error("bad redirect: {0}")]
    BadLocation(String),
    /// The caller's check refused a hop (its message is shown as is).
    #[error("{0}")]
    Refused(String),
}

impl From<RedirectError> for crate::engine::EngineError {
    fn from(e: RedirectError) -> Self {
        crate::engine::EngineError::Network(e.to_string())
    }
}

/// Sends the request `build` makes for `url` (puppy's own part of it: method, `Range`,
/// `If-Range`, …) with `context`, the context given for `origin` (a download's source or
/// mirror URL), following redirects. Each hop, `url` included, gets
/// `context.for_target(origin, hop)`. The response is the last hop's (`resp.url()` is the
/// final URL); a redirect without a `Location` is returned as is.
///
/// Credentials in `origin` itself (`https://user:pass@host/…`) go to every hop on its
/// origin, as the `Authorization` header reqwest made of them did.
pub async fn send(
    url: &url::Url,
    origin: &url::Url,
    context: &RequestContext,
    build: impl FnMut(&url::Url, &RequestContext) -> RequestBuilder,
) -> Result<Response, RedirectError> {
    follow(
        url,
        build,
        |hop| hop_context(context, origin, hop),
        |_, _| Ok(()),
    )
    .await
}

/// The context of a request to `hop` on behalf of `origin`, whose context is `context`.
fn hop_context(context: &RequestContext, origin: &url::Url, hop: &url::Url) -> RequestContext {
    let mut ctx = context.for_target(origin, hop);
    let carries_own = !hop.username().is_empty() || hop.password().is_some();
    if ctx.basic_auth.is_none()
        && !carries_own
        && !origin.username().is_empty()
        && origin.origin() == hop.origin()
    {
        let decode = |s: &str| percent_decode_str(s).decode_utf8_lossy().into_owned();
        ctx.basic_auth = Some((decode(origin.username()), origin.password().map(decode)));
    }
    ctx
}

/// The redirect loop behind [`send`], with the scoping and the hop check up to the
/// caller: `scope` gives the context for a hop's URL, and `check(current, next)` may
/// refuse a redirect. Only HTTP(S) redirects are followed.
pub(crate) async fn follow(
    url: &url::Url,
    mut build: impl FnMut(&url::Url, &RequestContext) -> RequestBuilder,
    mut scope: impl FnMut(&url::Url) -> RequestContext,
    mut check: impl FnMut(&url::Url, &url::Url) -> Result<(), String>,
) -> Result<Response, RedirectError> {
    let mut current = url.clone();
    let mut redirects = 0;
    loop {
        let ctx = scope(&current);
        let resp = apply_context(build(&current, &ctx), &ctx).send().await?;
        if !is_redirect(resp.status()) {
            return Ok(resp);
        }
        let Some(location) = resp.headers().get(header::LOCATION) else {
            return Ok(resp);
        };
        let location = location
            .to_str()
            .map_err(|_| RedirectError::BadLocation("Location is not text".into()))?;
        let next = current
            .join(location.trim())
            .map_err(|e| RedirectError::BadLocation(e.to_string()))?;
        check(&current, &next).map_err(RedirectError::Refused)?;
        if !matches!(next.scheme(), "http" | "https") {
            return Err(RedirectError::BadLocation(format!(
                "to a {}: URL",
                next.scheme()
            )));
        }
        redirects += 1;
        if redirects > MAX_REDIRECTS {
            return Err(RedirectError::TooMany);
        }
        current = next;
    }
}

/// The statuses reqwest followed: 301, 302, 303, 307 and 308 (not 300 or 304). puppy
/// sends only GET and HEAD, which every one of them keeps.
fn is_redirect(status: StatusCode) -> bool {
    matches!(
        status,
        StatusCode::MOVED_PERMANENTLY
            | StatusCode::FOUND
            | StatusCode::SEE_OTHER
            | StatusCode::TEMPORARY_REDIRECT
            | StatusCode::PERMANENT_REDIRECT
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_credentials_stay_on_their_origin() {
        let u = |s: &str| url::Url::parse(s).unwrap();
        let origin = u("https://al%20ice:p%40ss@a.example/f");
        let none = RequestContext::default();
        let auth = |hop: &str| hop_context(&none, &origin, &u(hop)).basic_auth;
        let alice = Some(("al ice".to_string(), Some("p@ss".to_string())));
        assert_eq!(auth("https://a.example/g"), alice);
        assert_eq!(auth("https://b.example/g"), None);
        assert_eq!(auth("http://a.example/g"), None);
        // The hop's own (the request URL's, which reqwest sends) or the context's win.
        assert_eq!(auth("https://bob@a.example/g"), None);
        let given = RequestContext {
            basic_auth: Some(("carol".into(), None)),
            ..RequestContext::default()
        };
        let got = hop_context(&given, &origin, &u("https://a.example/g")).basic_auth;
        assert_eq!(got, given.basic_auth);
    }
}
