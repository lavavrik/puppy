//! The locations of a download's file, its primary URL first and then its mirrors, and
//! how the download uses each: which may get connections, which failed recently
//! (benched with backoff) and which are dropped, and why.
//!
//! Every location is validated on its own: it has its own final URL (after redirects),
//! ETag and Last-Modified, sent as `If-Range` on its own requests. Bytes are only ever
//! requested from a location that agreed with the file (same size, range support) and
//! that has not been seen to change since.

use crate::config::HttpConfig;
use crate::engine::EngineError;
use crate::http::probe::RemoteInfo;
use crate::http::resume::MirrorResume;
use crate::http::worker::{backoff, is_url_rejected};
use crate::meter::SpeedMeter;
use crate::types::{
    AddRequest, MirrorState, MirrorView, RequestContext, Source, masked_url, without_password,
};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

/// What identifies the file at one location.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Validators {
    /// Where its requests go (its URL after redirects).
    pub final_url: url::Url,
    pub etag: Option<String>,
    pub etag_strong: bool,
    pub last_modified: Option<String>,
}

impl Validators {
    /// Nothing known yet: requests go to `url` itself.
    pub fn unknown(url: &url::Url) -> Self {
        Validators {
            final_url: url.clone(),
            etag: None,
            etag_strong: false,
            last_modified: None,
        }
    }

    pub fn of(r: &RemoteInfo) -> Self {
        Validators {
            final_url: r.final_url.clone(),
            etag: r.etag.clone(),
            etag_strong: r.etag_strong,
            last_modified: r.last_modified.clone(),
        }
    }

    /// No ETag and no Last-Modified recorded.
    pub fn is_empty(&self) -> bool {
        self.etag.is_none() && self.last_modified.is_none()
    }

    /// The `If-Range` value: a strong ETag, else Last-Modified when there is no ETag at
    /// all (weak ETags cannot validate ranges).
    pub fn if_range(&self) -> Option<String> {
        match &self.etag {
            Some(e) if self.etag_strong => Some(e.clone()),
            Some(_) => None,
            None => self.last_modified.clone(),
        }
    }
}

/// Why a location is not used any more; decides the download's outcome once none is
/// left.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum DropKind {
    /// It serves another file now (or a different size).
    Changed,
    /// It refused the request (401/403/404/410).
    Rejected(u16),
    /// It does not honour range requests.
    RangeUnsupported,
    /// It kept failing on errors that may go away (network, 5xx, 429, 408).
    Transient,
    /// Any other refusal.
    Fatal,
}

#[derive(Debug, Clone, PartialEq)]
pub(super) enum Health {
    Usable,
    Benched { until: Instant, reason: String },
    Dropped { kind: DropKind, reason: String },
}

pub(super) struct MirrorRt {
    /// As given.
    pub url: url::Url,
    pub primary: bool,
    /// The request context for this location (see [`RequestContext::for_mirror`]).
    pub context: RequestContext,
    pub v: Validators,
    pub health: Health,
    /// Failed attempts since its last progress.
    pub failures: u32,
    /// Workers assigned to it.
    pub active: u16,
    /// Bytes received from it (incremented by its workers).
    pub got: Arc<AtomicU64>,
    pub bps: u64,
    /// Progress (bytes done) when its URL was last re-resolved: a fresh URL that is
    /// refused again before any progress is not re-resolved again.
    pub reresolved_at: Option<u64>,
    /// Its URL is being re-resolved: no new connections meanwhile.
    pub busy: bool,
    /// The location's FTP side (sessions, what was learned about the server), for an
    /// FTP(S) URL.
    pub ftp: Option<Arc<crate::ftp::Site>>,
    meter: SpeedMeter,
    last_got: u64,
}

impl MirrorRt {
    fn new(url: url::Url, primary: bool, context: RequestContext) -> Self {
        MirrorRt {
            ftp: crate::ftp::is_ftp(&url).then(|| crate::ftp::Site::new(&url)),
            v: Validators::unknown(&url),
            url,
            primary,
            context,
            health: Health::Usable,
            failures: 0,
            active: 0,
            got: Arc::new(AtomicU64::new(0)),
            bps: 0,
            reresolved_at: None,
            busy: false,
            meter: SpeedMeter::new(),
            last_got: 0,
        }
    }

    /// The context for a request to `target` (its final URL, possibly another origin).
    pub fn context_for(&self, target: &url::Url) -> RequestContext {
        self.context.for_target(&self.url, target)
    }

    pub fn is_dropped(&self) -> bool {
        matches!(self.health, Health::Dropped { .. })
    }

    pub fn drop_with(&mut self, kind: DropKind, reason: impl Into<String>) {
        let reason = reason.into();
        tracing::info!(url = %masked_url(&self.url), "mirror dropped: {reason}");
        self.health = Health::Dropped { kind, reason };
    }

    /// Already has as many workers as its server allows connections (FTP servers that
    /// turned a session away).
    pub fn at_capacity(&self) -> bool {
        self.ftp
            .as_ref()
            .is_some_and(|s| s.at_capacity(self.active))
    }

    /// Usable now (a bench that has run out is lifted).
    pub fn available(&mut self, now: Instant) -> bool {
        match &self.health {
            Health::Usable => true,
            Health::Benched { until, .. } if *until <= now => {
                self.health = Health::Usable;
                true
            }
            _ => false,
        }
    }

    /// Usable now, without lifting anything.
    pub fn available_now(&self, now: Instant) -> bool {
        match &self.health {
            Health::Usable => true,
            Health::Benched { until, .. } => *until <= now,
            Health::Dropped { .. } => false,
        }
    }

    /// A failed attempt (retryable): benched with the usual backoff, or dropped once it
    /// failed `max_retries` times in a row without progress. Failures of other
    /// connections while it is already benched are the same outage and not counted.
    pub fn failed(&mut self, why: String, progress: bool, cfg: &HttpConfig, now: Instant) {
        if progress {
            self.failures = 0;
        }
        match &self.health {
            Health::Dropped { .. } => return,
            Health::Benched { until, .. } if *until > now && !progress => return,
            _ => {}
        }
        if self.failures >= cfg.max_retries {
            self.drop_with(DropKind::Transient, format!("retries exhausted: {why}"));
            return;
        }
        let delay = backoff(cfg, self.failures);
        self.failures += 1;
        self.health = Health::Benched {
            until: now + delay,
            reason: why,
        };
    }

    /// A worker on it finished its range.
    pub fn succeeded(&mut self) {
        self.failures = 0;
    }

    /// Samples the bytes received since the last call into its speed.
    pub fn sample(&mut self, now: Instant) {
        let got = self.got.load(Ordering::Relaxed);
        self.meter.record(got.saturating_sub(self.last_got), now);
        self.last_got = got;
        self.bps = self.meter.bps(now);
    }

    /// Speed per connection, the tie-break between equally loaded mirrors.
    fn per_connection(&self) -> u64 {
        self.bps / u64::from(self.active.max(1))
    }

    pub fn view(&self, now: Instant) -> MirrorView {
        let state = match &self.health {
            Health::Usable => MirrorState::InUse,
            Health::Benched { until, reason } => MirrorState::Benched {
                reason: reason.clone(),
                retry_in_secs: until.saturating_duration_since(now).as_secs_f64().ceil() as u64,
            },
            Health::Dropped { reason, .. } => MirrorState::Dropped {
                reason: reason.clone(),
            },
        };
        MirrorView {
            url: masked_url(&self.url),
            primary: self.primary,
            state,
            connections: self.active,
            bps: if self.active > 0 { self.bps } else { 0 },
            done_bytes: self.got.load(Ordering::Relaxed),
        }
    }

    /// The record kept in the resume sidecar (no passwords).
    pub fn record(&self) -> MirrorResume {
        MirrorResume {
            url: without_password(&self.url),
            final_url: without_password(&self.v.final_url),
            etag: self.v.etag.clone(),
            etag_strong: self.v.etag_strong,
            last_modified: self.v.last_modified.clone(),
        }
    }
}

/// The request's locations: its source (primary) first, then its HTTP(S) and FTP(S)
/// mirrors by priority (lower first, unnumbered last, otherwise as given), duplicates
/// removed.
pub(super) fn locations(req: &AddRequest) -> Vec<MirrorRt> {
    let Source::Http { url: primary } = &req.source else {
        return Vec::new();
    };
    let mut out = vec![MirrorRt::new(primary.clone(), true, req.context.clone())];
    let mut mirrors: Vec<_> = req.mirrors.iter().collect();
    mirrors.sort_by_key(|m| m.priority.unwrap_or(u32::MAX));
    for m in mirrors {
        if !is_url_scheme(m.url.scheme()) || out.iter().any(|o| o.url == m.url) {
            continue;
        }
        let context = req.context.for_mirror(primary, &m.url);
        out.push(MirrorRt::new(m.url.clone(), false, context));
    }
    out
}

/// The schemes a URL download's locations may have.
pub(crate) fn is_url_scheme(scheme: &str) -> bool {
    matches!(scheme, "http" | "https") || crate::ftp::is_ftp_scheme(scheme)
}

/// Applies a sidecar's records (by URL; the sidecar's have no password) to `mirrors`.
pub(super) fn restore_records(mirrors: &mut [MirrorRt], records: &[MirrorResume]) {
    for m in mirrors.iter_mut().filter(|m| !m.primary) {
        // An older puppy recorded the password too.
        if let Some(r) = records
            .iter()
            .find(|r| r.url == without_password(&m.url) || r.url == m.url)
        {
            m.v = Validators {
                final_url: with_password_of(&r.final_url, &m.url),
                etag: r.etag.clone(),
                etag_strong: r.etag_strong,
                last_modified: r.last_modified.clone(),
            };
        }
    }
}

/// `recorded` (from a sidecar, without password) as `given` if it is the same URL: with
/// `given`'s password then.
pub(super) fn with_password_of(recorded: &url::Url, given: &url::Url) -> url::Url {
    if *recorded == without_password(given) {
        given.clone()
    } else {
        recorded.clone()
    }
}

/// The mirror a new connection goes to: among the usable ones that `allowed` lets
/// through (and that are not being re-resolved, nor FTP servers at their session limit),
/// the one with the fewest connections, ties broken by the faster one per connection,
/// then by order. `None` if there is none.
pub(super) fn pick(
    mirrors: &mut [MirrorRt],
    now: Instant,
    allowed: impl Fn(&MirrorRt) -> bool,
) -> Option<usize> {
    let mut best: Option<usize> = None;
    for i in 0..mirrors.len() {
        if !mirrors[i].available(now)
            || mirrors[i].busy
            || mirrors[i].at_capacity()
            || !allowed(&mirrors[i])
        {
            continue;
        }
        best = match best {
            None => Some(i),
            Some(b) => {
                let (m, o) = (&mirrors[i], &mirrors[b]);
                let better = m.active < o.active
                    || (m.active == o.active && m.per_connection() > o.per_connection());
                Some(if better { i } else { b })
            }
        };
    }
    best
}

/// When the earliest bench runs out, if any mirror is benched.
pub(super) fn next_unbench(mirrors: &[MirrorRt]) -> Option<Instant> {
    mirrors
        .iter()
        .filter_map(|m| match &m.health {
            Health::Benched { until, .. } => Some(*until),
            _ => None,
        })
        .min()
}

/// Lifts every bench and drop: a resume tries every location again (validation decides
/// which are used).
pub(super) fn reset(mirrors: &mut [MirrorRt]) {
    for m in mirrors {
        m.failures = 0;
        m.reresolved_at = None;
        m.busy = false;
        m.health = Health::Usable;
    }
}

/// How a location that failed with `e` is dropped.
pub(super) fn kind_of(e: &EngineError) -> DropKind {
    match e {
        EngineError::Http { status } if is_url_rejected(*status) => DropKind::Rejected(*status),
        e if e.is_transient() => DropKind::Transient,
        _ => DropKind::Fatal,
    }
}

/// Why a location with `got` bytes does not match a file of `want` bytes.
pub(super) fn size_differs(got: Option<u64>, want: Option<u64>) -> String {
    let show = |n: Option<u64>| n.map_or("unknown".to_string(), |n| format!("{n} bytes"));
    format!(
        "its size ({}) differs from the file's ({})",
        show(got),
        show(want)
    )
}

/// "no mirror works: a.example: HTTP 404; b.example: retries exhausted: …".
pub(super) fn summary(mirrors: &[MirrorRt]) -> String {
    let parts: Vec<String> = mirrors
        .iter()
        .map(|m| {
            let why = match &m.health {
                Health::Dropped { reason, .. } | Health::Benched { reason, .. } => reason.as_str(),
                Health::Usable => "unused",
            };
            format!("{}: {why}", host_of(&m.url))
        })
        .collect();
    format!("no mirror works: {}", parts.join("; "))
}

fn host_of(url: &url::Url) -> String {
    match (url.host_str(), url.port()) {
        (Some(h), Some(p)) => format!("{h}:{p}"),
        (Some(h), None) => h.to_string(),
        _ => masked_url(url),
    }
}
