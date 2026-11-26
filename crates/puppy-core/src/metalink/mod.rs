//! Metalink: documents listing where to get files and how to check them. Metalink 4
//! (RFC 5854, `.meta4`, `application/metalink4+xml`) and the older Metalink 3
//! (`.metalink`, `application/metalink+xml`, as Fedora's MirrorManager still serves).
//!
//! A document describes one or more files. Each [`MetaFile`] becomes its own URL
//! download ([`MetaFile::request`]):
//! - its `<url>`s are the primary and the mirrors, best `priority` first (Metalink 3:
//!   highest `preference`), with their `location`; only schemes the URL transports
//!   serve are kept ([`crate::http::is_url_scheme`]: today http(s), ftp(s), ftpes and
//!   sftp), at most [`MAX_LOCATIONS`];
//! - the strongest supported `<hash>` is its checksum;
//! - `<size>` is the size every location must report ([`AddRequest::size`]);
//! - `<name>` is a path below the output folder, refused if it could leave it
//!   ([`SafePath`]).
//!
//! Ignored: `<metaurl>`s (torrents; Metalink 3 `type="bittorrent"` URLs), `<pieces>`
//! (chunk hashes: the whole-file hash is checked instead), `<signature>`s and the
//! descriptive elements.
//!
//! The XML parser refuses DTDs and unknown entities ([`xml`]); documents and the
//! number of files and URLs are capped ([`MAX_DOCUMENT_BYTES`], [`MAX_FILES`],
//! [`MAX_URLS`]).

mod fetch;
mod name;
mod xml;

pub use fetch::{FETCH_TIMEOUT, Fetched, fetch};
pub use name::SafePath;

use crate::checksum::{Checksum, HashAlgo};
use crate::config::HttpConfig;
use crate::http::is_url_scheme;
use crate::types::{AddRequest, FileChoice, Mirror, RequestContext, Source};
use std::collections::HashSet;
use std::io::Read;
use std::path::{Path, PathBuf};
use xml::Element;

/// Larger documents are refused, read from a file or fetched. Real ones are a few KiB
/// (a file with a hundred mirrors) to a few hundred (many files with piece hashes).
pub const MAX_DOCUMENT_BYTES: usize = 4 * 1024 * 1024;
/// A document describing more files is refused.
pub const MAX_FILES: usize = 1000;
/// A file listing more `<url>`s and `<metaurl>`s is refused.
pub const MAX_URLS: usize = 1000;
/// At most this many locations (primary and mirrors) of a file are used, the best by
/// priority: the engine probes every location when a download starts.
pub const MAX_LOCATIONS: usize = 16;

/// The Metalink 4 namespace (RFC 5854).
pub const NS_V4: &str = "urn:ietf:params:xml:ns:metalink";
/// The Metalink 3 namespace.
pub const NS_V3: &str = "http://www.metalinker.org/";

/// The media types a server labels Metalink documents with (4, then 3).
pub const MEDIA_TYPES: [&str; 2] = ["application/metalink4+xml", "application/metalink+xml"];

/// The `Accept` header of a metalink fetch. MirrorBrain and MirrorCache servers answer a
/// file's own URL with its metalink when asked so; MirrorCache serves the file instead
/// (even at its `.meta4` URL) as soon as `*/*` is accepted too, so it is not. Servers
/// that do not negotiate send what they have anyway.
pub const ACCEPT: &str = "application/metalink4+xml, application/metalink+xml";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Version {
    /// Metalink 3.0 (metalinker.org).
    V3,
    /// Metalink 4 (RFC 5854).
    V4,
}

/// A parsed, checked document.
#[derive(Debug, Clone, PartialEq)]
pub struct Metalink {
    pub version: Version,
    /// In document order; at least one.
    pub files: Vec<MetaFile>,
}

/// One `<file>`, ready to download.
#[derive(Debug, Clone, PartialEq)]
pub struct MetaFile {
    /// Where it goes, below the output folder.
    pub path: SafePath,
    pub size: Option<u64>,
    /// The strongest supported hash, if any.
    pub checksum: Option<Checksum>,
    /// Usable locations, best first (at least one, at most [`MAX_LOCATIONS`]).
    pub urls: Vec<Mirror>,
}

/// Why a document was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum MetalinkError {
    #[error("the metalink is larger than {} MiB", MAX_DOCUMENT_BYTES >> 20)]
    TooBig,
    #[error("not valid XML: {0}")]
    Xml(String),
    #[error(
        "the metalink has a DOCTYPE, which is refused: Metalink uses none, and DTD entities \
         are a way to attack XML parsers"
    )]
    Doctype,
    #[error("the metalink is too complex: {0}")]
    TooComplex(String),
    #[error("not a metalink: {0}")]
    NotMetalink(String),
    #[error("the metalink lists no files")]
    NoFiles,
    #[error("the metalink lists more than {MAX_FILES} files")]
    TooManyFiles,
    /// One `<file>` is unusable; `file` is its name as written (or its position).
    #[error("{file}: {why}")]
    File { file: String, why: String },
}

/// Whether `path` (a file path or a URL's path) ends in `.meta4` or `.metalink`.
pub fn has_metalink_extension(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    lower.ends_with(".meta4") || lower.ends_with(".metalink")
}

/// Whether a `Content-Type` value is a Metalink media type (parameters ignored).
pub fn is_metalink_media_type(content_type: &str) -> bool {
    let essence = content_type.split(';').next().unwrap_or("").trim();
    MEDIA_TYPES.iter().any(|m| essence.eq_ignore_ascii_case(m))
}

/// Whether `bytes` look like the start of a Metalink document: a `<metalink` element
/// within the first KiBs (after the XML declaration and comments).
pub fn looks_like_metalink(bytes: &[u8]) -> bool {
    let head = &bytes[..bytes.len().min(4096)];
    let head = String::from_utf8_lossy(head);
    let mut rest = head.trim_start_matches('\u{feff}').trim_start();
    // Skip the declaration, comments and processing instructions; the first element
    // decides (with or without a prefix: `<m:metalink`).
    while let Some(r) = rest.strip_prefix('<') {
        if let Some(after) = r.strip_prefix("!--") {
            match after.find("-->") {
                Some(i) => rest = after[i + 3..].trim_start(),
                None => return false,
            }
        } else if r.starts_with('?') {
            match r.find("?>") {
                Some(i) => rest = r[i + 2..].trim_start(),
                None => return false,
            }
        } else {
            let name: String = r
                .chars()
                .take_while(|c| !c.is_whitespace() && *c != '>' && *c != '/')
                .collect();
            return name.rsplit(':').next() == Some("metalink");
        }
    }
    false
}

/// Parses and checks a document (UTF-8, a BOM allowed).
pub fn parse(bytes: &[u8]) -> Result<Metalink, MetalinkError> {
    if bytes.len() > MAX_DOCUMENT_BYTES {
        return Err(MetalinkError::TooBig);
    }
    let bytes = bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(bytes);
    if bytes.starts_with(b"\xFF\xFE") || bytes.starts_with(b"\xFE\xFF") {
        return Err(MetalinkError::Xml(
            "UTF-16 documents are not supported".into(),
        ));
    }
    let text =
        std::str::from_utf8(bytes).map_err(|e| MetalinkError::Xml(format!("not UTF-8: {e}")))?;
    let root = xml::parse(text)?;
    if root.name != "metalink" {
        return Err(MetalinkError::NotMetalink(format!(
            "the document is a <{}>",
            root.name
        )));
    }
    let ns = root.ns.as_deref();
    let version = match ns {
        Some(NS_V4) => Version::V4,
        Some(NS_V3) => Version::V3,
        Some(other) => {
            return Err(MetalinkError::NotMetalink(format!(
                "unknown namespace {other:?}"
            )));
        }
        // No namespace: as the structure says (Metalink 3 has a <files> wrapper).
        None if root.child(None, "files").is_some() => Version::V3,
        None => Version::V4,
    };
    let elements: Vec<&Element> = match version {
        Version::V4 => root.children(ns, "file").collect(),
        Version::V3 => root
            .children(ns, "files")
            .flat_map(|f| f.children(ns, "file"))
            .collect(),
    };
    if elements.is_empty() {
        return Err(MetalinkError::NoFiles);
    }
    if elements.len() > MAX_FILES {
        return Err(MetalinkError::TooManyFiles);
    }
    let mut files = Vec::with_capacity(elements.len());
    let mut seen = HashSet::new();
    for (i, el) in elements.into_iter().enumerate() {
        let label = match el.attr("name") {
            Some(n) if !n.trim().is_empty() => n.escape_debug().to_string(),
            _ => format!("file {}", i + 1),
        };
        let fail = |why: String| MetalinkError::File {
            file: label.clone(),
            why,
        };
        let file = interpret(el, ns, version).map_err(fail)?;
        if !seen.insert(file.path.key()) {
            return Err(fail(format!(
                "another file of the metalink has the same name ({})",
                file.path
            )));
        }
        files.push(file);
    }
    Ok(Metalink { version, files })
}

/// One `<file>` element.
fn interpret(el: &Element, ns: Option<&str>, version: Version) -> Result<MetaFile, String> {
    let path = SafePath::parse(el.attr("name").unwrap_or(""))?;
    let size = match el.child(ns, "size") {
        Some(s) => Some(
            s.value()
                .parse::<u64>()
                .map_err(|_| format!("bad <size> {:?}", s.value()))?,
        ),
        None => None,
    };
    let hashes: Vec<&Element> = match version {
        Version::V4 => el.children(ns, "hash").collect(),
        Version::V3 => el
            .children(ns, "verification")
            .flat_map(|v| v.children(ns, "hash"))
            .collect(),
    };
    let mut checksum: Option<Checksum> = None;
    for h in hashes {
        // An unknown type (`sha3-256`, `ed2k`, …) is skipped: another may do.
        let Some(algo) = h.attr("type").and_then(HashAlgo::parse) else {
            continue;
        };
        let c = Checksum::new(algo, h.value()).map_err(|e| format!("bad <hash>: {e}"))?;
        if checksum
            .as_ref()
            .is_none_or(|best| algo.strength() > best.algo.strength())
        {
            checksum = Some(c);
        }
    }

    let (urls, metaurls) = match version {
        Version::V4 => (
            el.children(ns, "url").collect::<Vec<_>>(),
            el.children(ns, "metaurl").count(),
        ),
        Version::V3 => (
            el.children(ns, "resources")
                .flat_map(|r| r.children(ns, "url"))
                .collect(),
            0,
        ),
    };
    if urls.len() + metaurls > MAX_URLS {
        return Err(format!("more than {MAX_URLS} URLs"));
    }
    let mut found: Vec<(Option<u32>, Mirror)> = Vec::new();
    let mut torrents = metaurls;
    let mut schemes = Vec::new();
    for u in urls {
        if version == Version::V3
            && u.attr("type")
                .is_some_and(|t| t.eq_ignore_ascii_case("bittorrent"))
        {
            torrents += 1;
            continue;
        }
        let Ok(url) = url::Url::parse(u.value()) else {
            continue;
        };
        if !is_url_scheme(url.scheme()) || url.host_str().is_none_or(str::is_empty) {
            schemes.push(url.scheme().to_string());
            continue;
        }
        if found.iter().any(|(_, m)| m.url == url) {
            continue;
        }
        let priority = match version {
            // 1 (best) to 999999.
            Version::V4 => u
                .attr("priority")
                .and_then(|p| p.trim().parse::<u32>().ok())
                .filter(|p| *p >= 1),
            // 0 to 100, higher preferred: 100 becomes 1, 0 becomes 101.
            Version::V3 => u
                .attr("preference")
                .and_then(|p| p.trim().parse::<u32>().ok())
                .map(|p| 101 - p.min(100)),
        };
        let location = u
            .attr("location")
            .map(str::trim)
            .filter(|l| !l.is_empty() && l.len() <= 8 && l.chars().all(|c| c.is_ascii_alphabetic()))
            .map(str::to_ascii_lowercase);
        found.push((
            priority,
            Mirror {
                url,
                priority,
                location,
            },
        ));
    }
    if found.is_empty() {
        let mut why = "no usable URL".to_string();
        if torrents > 0 {
            why.push_str(" (puppy does not use a metalink's torrents)");
        } else if !schemes.is_empty() {
            schemes.sort();
            schemes.dedup();
            why.push_str(&format!(" (unsupported: {})", schemes.join(", ")));
        }
        return Err(why);
    }
    // Stable: equal priorities keep document order; unnumbered ones come last.
    found.sort_by_key(|(p, _)| p.unwrap_or(u32::MAX));
    found.truncate(MAX_LOCATIONS);
    Ok(MetaFile {
        path,
        size,
        checksum,
        urls: found.into_iter().map(|(_, m)| m).collect(),
    })
}

impl MetaFile {
    /// This file's download: from its best URL with the others as mirrors, into
    /// `output_dir` (the caller adds the file's subdirectories: [`SafePath::dir_in`]),
    /// under its name, with its checksum and size.
    ///
    /// `context` (the user's headers and cookies) is for the metalink's own site: if the
    /// document came from `document` (its final URL), a primary on another origin gets
    /// it without cookies and credentials, as a redirect would. A local document's
    /// context is used as given.
    pub fn request(
        &self,
        output_dir: Option<PathBuf>,
        context: &RequestContext,
        document: Option<&url::Url>,
    ) -> AddRequest {
        let primary = self.urls[0].url.clone();
        let context = match document {
            Some(doc) => context.for_target(doc, &primary),
            None => context.clone(),
        };
        AddRequest {
            source: Source::Http { url: primary },
            output_dir,
            filename: Some(self.path.file_name().to_string()),
            context,
            files: FileChoice::All,
            seed: None,
            checksum: self.checksum.clone(),
            mirrors: self.urls[1..].to_vec(),
            size: self.size,
        }
    }
}

/// Where a metalink comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Location {
    Path(PathBuf),
    Url(url::Url),
}

impl std::fmt::Display for Location {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Location::Path(p) => write!(f, "{}", p.display()),
            Location::Url(u) => f.write_str(&crate::types::masked_url(u)),
        }
    }
}

/// Whether the source `s` (unquoted, `~` expanded) is a metalink: an HTTP(S) URL whose
/// path ends in `.meta4` or `.metalink`, or an existing file with that extension or
/// whose content starts like one (a `.torrent` file never is). With `force` every
/// HTTP(S) URL and existing file is taken as one, and anything else is an error.
pub fn detect(s: &str, force: bool) -> Result<Option<Location>, String> {
    if let Ok(url) = url::Url::parse(s)
        && matches!(url.scheme(), "http" | "https")
    {
        let named = has_metalink_extension(url.path());
        return Ok((force || named).then_some(Location::Url(url)));
    }
    let path = Path::new(s);
    if path.is_file() {
        if force || has_metalink_extension(s) {
            return Ok(Some(Location::Path(path.to_path_buf())));
        }
        if s.to_ascii_lowercase().ends_with(".torrent") {
            return Ok(None);
        }
        let mut head = Vec::with_capacity(4096);
        if let Ok(f) = std::fs::File::open(path) {
            let _ = f.take(4096).read_to_end(&mut head);
        }
        if looks_like_metalink(&head) {
            return Ok(Some(Location::Path(path.to_path_buf())));
        }
        return Ok(None);
    }
    if force {
        let what = if s.contains("://") || s.starts_with("magnet:") {
            "not an HTTP(S) URL"
        } else {
            "no such file"
        };
        return Err(format!(
            "{what}: a metalink is read from a file or an HTTP(S) URL"
        ));
    }
    Ok(None)
}

/// A metalink read or fetched: the document and, when fetched, its final URL.
#[derive(Debug, Clone)]
pub struct Loaded {
    pub metalink: Metalink,
    pub document: Option<url::Url>,
}

/// Reads (a path) or fetches (a URL, with `context` and `cfg`'s proxy) the metalink at
/// `location` and parses it. The error is a sentence for the user.
pub async fn load(
    location: &Location,
    cfg: &HttpConfig,
    context: &RequestContext,
) -> Result<Loaded, String> {
    match location {
        Location::Path(p) => {
            let p = p.clone();
            let bytes = tokio::task::spawn_blocking(move || read_capped(&p))
                .await
                .map_err(|e| e.to_string())?
                .map_err(|e| e.to_string())?;
            let metalink = parse(&bytes).map_err(|e| e.to_string())?;
            Ok(Loaded {
                metalink,
                document: None,
            })
        }
        Location::Url(url) => {
            let got = fetch(cfg, url, context).await?;
            let metalink = match parse(&got.bytes) {
                Ok(m) => m,
                // Say what came instead, when the server said what it is.
                Err(MetalinkError::Xml(_) | MetalinkError::NotMetalink(_))
                    if got
                        .content_type
                        .as_deref()
                        .is_some_and(|t| !is_metalink_media_type(t)) =>
                {
                    return Err(format!(
                        "not a metalink: the server sent {}",
                        got.content_type.unwrap_or_default()
                    ));
                }
                Err(e) => return Err(e.to_string()),
            };
            Ok(Loaded {
                metalink,
                document: Some(got.final_url),
            })
        }
    }
}

/// A local document, refused past [`MAX_DOCUMENT_BYTES`] without reading it all.
fn read_capped(path: &Path) -> std::io::Result<Vec<u8>> {
    let f = std::fs::File::open(path)?;
    let mut bytes = Vec::new();
    f.take(MAX_DOCUMENT_BYTES as u64 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > MAX_DOCUMENT_BYTES {
        return Err(std::io::Error::other(MetalinkError::TooBig.to_string()));
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests;
