//! Probing a remote file: size, range support, validators and filename.

use crate::config::HttpConfig;
use crate::engine::EngineError;
use crate::fsutil::sanitize_filename;
use crate::types::RequestContext;
use percent_encoding::percent_decode_str;
use reqwest::header::{self, HeaderMap, HeaderName, HeaderValue};
use reqwest::{Client, RequestBuilder, Response, StatusCode};
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct RemoteInfo {
    pub final_url: url::Url,
    pub total: Option<u64>,
    pub accepts_ranges: bool,
    pub etag: Option<String>,
    pub etag_strong: bool,
    pub last_modified: Option<String>,
    pub filename: String,
}

pub fn build_client(cfg: &HttpConfig) -> Result<Client, EngineError> {
    let mut defaults = HeaderMap::new();
    defaults.insert(
        header::ACCEPT_ENCODING,
        HeaderValue::from_static("identity"),
    );
    let mut builder = Client::builder()
        .default_headers(defaults)
        // HTTP/2 would multiplex every segment request over one TCP connection, defeating
        // multi-connection downloads (per-connection throttling is what they work around).
        .http1_only()
        .redirect(reqwest::redirect::Policy::limited(10))
        .connect_timeout(Duration::from_millis(cfg.connect_timeout_ms))
        .read_timeout(Duration::from_millis(cfg.read_timeout_ms))
        .user_agent(concat!("puppy/", env!("CARGO_PKG_VERSION")));
    if let Some(proxy) = &cfg.proxy {
        let proxy = reqwest::Proxy::all(proxy.as_str())
            .map_err(|e| EngineError::Invalid(format!("proxy: {e}")))?;
        builder = builder.proxy(proxy);
    }
    builder
        .build()
        .map_err(|e| EngineError::Network(e.to_string()))
}

/// Applies per-download request context. Invalid header names/values are skipped.
pub fn apply_context(mut rb: RequestBuilder, ctx: &RequestContext) -> RequestBuilder {
    for (name, value) in &ctx.headers {
        if let (Ok(n), Ok(v)) = (
            HeaderName::from_bytes(name.as_bytes()),
            HeaderValue::from_str(value),
        ) {
            rb = rb.header(n, v);
        }
    }
    if let Some(c) = &ctx.cookies {
        rb = rb.header(header::COOKIE, c);
    }
    if let Some(r) = &ctx.referer {
        rb = rb.header(header::REFERER, r);
    }
    if let Some(ua) = &ctx.user_agent {
        rb = rb.header(header::USER_AGENT, ua);
    }
    if let Some((user, pass)) = &ctx.basic_auth {
        rb = rb.basic_auth(user, pass.as_deref());
    }
    rb
}

fn net_err(e: reqwest::Error) -> EngineError {
    EngineError::Network(e.to_string())
}

fn header_str(h: &HeaderMap, name: header::HeaderName) -> Option<String> {
    h.get(name)
        .map(|v| String::from_utf8_lossy(v.as_bytes()).trim().to_string())
        .filter(|s| !s.is_empty())
}

fn content_length(h: &HeaderMap) -> Option<u64> {
    header_str(h, header::CONTENT_LENGTH)?.parse().ok()
}

/// Total from `Content-Range: bytes 0-0/<total>`.
fn content_range_total(h: &HeaderMap) -> Option<u64> {
    let v = header_str(h, header::CONTENT_RANGE)?;
    v.rsplit('/').next()?.trim().parse().ok()
}

#[derive(Default)]
struct Partial {
    total: Option<u64>,
    accepts_ranges: bool,
    etag: Option<String>,
    last_modified: Option<String>,
    disposition: Option<String>,
}

fn collect(resp: &Response) -> Partial {
    let h = resp.headers();
    let accepts_ranges = header_str(h, header::ACCEPT_RANGES)
        .is_some_and(|v| v.to_ascii_lowercase().contains("bytes"));
    Partial {
        total: content_length(h),
        accepts_ranges,
        etag: header_str(h, header::ETAG),
        last_modified: header_str(h, header::LAST_MODIFIED),
        disposition: header_str(h, header::CONTENT_DISPOSITION),
    }
}

pub async fn probe(
    client: &Client,
    url: &url::Url,
    ctx: &RequestContext,
) -> Result<RemoteInfo, EngineError> {
    let head = apply_context(client.head(url.clone()), ctx).send().await;
    let mut final_url = url.clone();
    let mut info = Partial::default();
    let mut head_ok = false;
    if let Ok(resp) = head
        && resp.status().is_success()
    {
        final_url = resp.url().clone();
        info = collect(&resp);
        head_ok = true;
    }

    // Some servers/CDNs answer HEAD with `Content-Length: 0` for real files, so a zero
    // length is confirmed with a ranged GET as well.
    let head_zero = head_ok && info.total == Some(0);
    if !head_ok || info.total.is_none() || head_zero {
        match range_probe(client, url, ctx).await {
            Ok((url2, p)) => {
                final_url = url2;
                // After a HEAD claiming 0, the GET is authoritative (even if it is unsized).
                info.total = if head_zero {
                    p.total
                } else {
                    p.total.or(info.total)
                };
                info.accepts_ranges = p.accepts_ranges || (head_ok && info.accepts_ranges);
                info.etag = p.etag.or(info.etag);
                info.last_modified = p.last_modified.or(info.last_modified);
                info.disposition = p.disposition.or(info.disposition);
            }
            Err(e) if !head_ok => return Err(e),
            // 416 for `bytes=0-0` confirms an empty file.
            Err(EngineError::Http { status: 416 }) => {}
            // HEAD's 0 is unconfirmed (error, timeout, 5xx, 429): the size is unknown.
            Err(_) if head_zero => info.total = None,
            Err(_) => {}
        }
    }

    let strong = info.etag.as_deref().is_some_and(|e| !e.starts_with("W/"));
    let filename = filename_from(info.disposition.as_deref(), &final_url);
    Ok(RemoteInfo {
        final_url,
        total: info.total,
        accepts_ranges: info.accepts_ranges,
        etag_strong: strong,
        etag: info.etag,
        last_modified: info.last_modified,
        filename,
    })
}

async fn range_probe(
    client: &Client,
    url: &url::Url,
    ctx: &RequestContext,
) -> Result<(url::Url, Partial), EngineError> {
    let resp = apply_context(
        client.get(url.clone()).header(header::RANGE, "bytes=0-0"),
        ctx,
    )
    .send()
    .await
    .map_err(net_err)?;
    let status = resp.status();
    if !status.is_success() {
        return Err(EngineError::Http {
            status: status.as_u16(),
        });
    }
    let mut p = collect(&resp);
    if status == StatusCode::PARTIAL_CONTENT {
        p.accepts_ranges = true;
        p.total = content_range_total(resp.headers());
    } else {
        p.accepts_ranges = false;
    }
    Ok((resp.url().clone(), p))
}

fn filename_from(disposition: Option<&str>, url: &url::Url) -> String {
    if let Some(name) = disposition.and_then(parse_disposition_filename) {
        return sanitize_filename(&name);
    }
    let seg = url
        .path_segments()
        .and_then(|mut s| s.rfind(|s| !s.is_empty()))
        .map(|s| percent_decode_str(s).decode_utf8_lossy().into_owned());
    sanitize_filename(seg.as_deref().unwrap_or("download"))
}

/// Splits `;`-separated parameters, honouring quoted strings.
fn disposition_params(v: &str) -> Vec<(String, String)> {
    let mut parts = Vec::new();
    let (mut cur, mut quoted, mut escaped) = (String::new(), false, false);
    for c in v.chars() {
        if escaped {
            cur.push(c);
            escaped = false;
        } else if quoted && c == '\\' {
            escaped = true;
        } else if c == '"' {
            quoted = !quoted;
            cur.push(c);
        } else if c == ';' && !quoted {
            parts.push(std::mem::take(&mut cur));
        } else {
            cur.push(c);
        }
    }
    parts.push(cur);
    parts
        .into_iter()
        .skip(1)
        .filter_map(|p| {
            let (k, val) = p.split_once('=')?;
            let val = val.trim();
            let val = val
                .strip_prefix('"')
                .and_then(|x| x.strip_suffix('"'))
                .unwrap_or(val);
            Some((k.trim().to_ascii_lowercase(), val.to_string()))
        })
        .collect()
}

fn parse_disposition_filename(v: &str) -> Option<String> {
    let params = disposition_params(v);
    if let Some((_, ext)) = params.iter().find(|(k, _)| k == "filename*")
        && let Some(name) = decode_rfc5987(ext)
    {
        return Some(name);
    }
    params
        .into_iter()
        .find(|(k, v)| k == "filename" && !v.is_empty())
        .map(|(_, v)| v)
}

/// `charset'lang'percent-encoded`; supports UTF-8 and ISO-8859-1.
fn decode_rfc5987(v: &str) -> Option<String> {
    let mut it = v.splitn(3, '\'');
    let charset = it.next()?.to_ascii_lowercase();
    let _lang = it.next()?;
    let data = it.next()?;
    let bytes: Vec<u8> = percent_decode_str(data).collect();
    match charset.as_str() {
        "utf-8" | "" => String::from_utf8(bytes).ok(),
        "iso-8859-1" => Some(bytes.iter().map(|&b| b as char).collect()),
        _ => None,
    }
}
