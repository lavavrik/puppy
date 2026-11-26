//! Shared data types exchanged between engines, the daemon and clients.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

pub type DownloadId = u64;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Source {
    /// A URL: `http`, `https`, `ftp`, `ftps` (implicit FTPS) or `ftpes` (explicit FTPS).
    Http {
        url: url::Url,
    },
    Magnet {
        uri: String,
    },
    TorrentFile {
        path: PathBuf,
    },
}

#[derive(Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct RequestContext {
    pub headers: Vec<(String, String)>,
    pub cookies: Option<String>,
    pub referer: Option<String>,
    pub user_agent: Option<String>,
    pub basic_auth: Option<(String, Option<String>)>,
    /// The ssh-agent of the client that added an SFTP download (its `SSH_AUTH_SOCK`):
    /// tried first for its logins. Not a secret (the agent signs only for its owner),
    /// so it goes to every location of the download.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ssh_agent: Option<String>,
}

impl RequestContext {
    /// The context for a request to `target` on behalf of a download of `original` (e.g.
    /// its redirect target). On another origin (scheme, host or port) the original host's
    /// cookies and credentials are dropped, including `Cookie`, `Authorization` and
    /// `Proxy-Authorization` given as headers; the referer, user agent and other headers
    /// are kept.
    pub fn for_target(&self, original: &url::Url, target: &url::Url) -> RequestContext {
        if original.origin() == target.origin() {
            return self.clone();
        }
        const SENSITIVE: [&str; 3] = ["cookie", "authorization", "proxy-authorization"];
        RequestContext {
            headers: self
                .headers
                .iter()
                .filter(|(n, _)| !SENSITIVE.iter().any(|s| n.trim().eq_ignore_ascii_case(s)))
                .cloned()
                .collect(),
            cookies: None,
            referer: self.referer.clone(),
            user_agent: self.user_agent.clone(),
            basic_auth: None,
            ssh_agent: self.ssh_agent.clone(),
        }
    }

    /// The context for requests to `mirror`, another location of the file at `primary`
    /// (whose context this is). Cookies and headers given for a download (by a browser
    /// or `-H`/`--cookie`/`--referer`) belong to the primary's site: a mirror on the same
    /// origin gets them all, a mirror on another origin only the user agent.
    pub fn for_mirror(&self, primary: &url::Url, mirror: &url::Url) -> RequestContext {
        if primary.origin() == mirror.origin() {
            return self.clone();
        }
        RequestContext {
            user_agent: self.user_agent.clone(),
            ssh_agent: self.ssh_agent.clone(),
            ..RequestContext::default()
        }
    }
}

/// `url` for display: a password in it is shown as `***`.
pub fn masked_url(url: &url::Url) -> String {
    if url.password().is_none() {
        return url.to_string();
    }
    let mut shown = url.clone();
    // Only fails for URLs that cannot have credentials, which this one has.
    let _ = shown.set_password(Some("***"));
    shown.to_string()
}

/// `url` without its password (the user name is kept), as written where secrets must not
/// go (the resume sidecar).
pub fn without_password(url: &url::Url) -> url::Url {
    let mut out = url.clone();
    if out.password().is_some() {
        // Only fails for URLs that cannot have credentials, which this one has.
        let _ = out.set_password(None);
    }
    out
}

/// Another location of a URL download's file (aria2's TAB-separated URIs, a
/// Metalink `<url>`). The download's source stays its primary URL.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Mirror {
    pub url: url::Url,
    /// Preference as in Metalink: lower is preferred (1 is the highest); mirrors without
    /// one come after every numbered one, in the order given.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority: Option<u32>,
    /// Where the mirror is (Metalink `location`, an ISO 3166-1 country code); shown only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub location: Option<String>,
}

impl Mirror {
    pub fn new(url: url::Url) -> Self {
        Mirror {
            url,
            priority: None,
            location: None,
        }
    }
}

/// Header values, cookies and passwords are never printed: a stray `{:?}` in a log line
/// must not leak them.
impl std::fmt::Debug for RequestContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        const HIDDEN: &str = "<redacted>";
        let headers: Vec<(&String, &str)> = self.headers.iter().map(|(n, _)| (n, HIDDEN)).collect();
        let basic_auth = self
            .basic_auth
            .as_ref()
            .map(|(user, pw)| (user, pw.as_ref().map(|_| HIDDEN)));
        f.debug_struct("RequestContext")
            .field("headers", &headers)
            .field("cookies", &self.cookies.as_ref().map(|_| HIDDEN))
            .field("referer", &self.referer)
            .field("user_agent", &self.user_agent)
            .field("basic_auth", &basic_auth)
            .field("ssh_agent", &self.ssh_agent)
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AddRequest {
    pub source: Source,
    pub output_dir: Option<PathBuf>,
    pub filename: Option<String>,
    pub context: RequestContext,
    /// Which files of a torrent to download.
    #[serde(default)]
    pub files: FileChoice,
    /// Seeding policy override for a torrent (`None`: the daemon's defaults).
    #[serde(default)]
    pub seed: Option<SeedPolicy>,
    /// The digest the finished file must have (URL downloads only): checked once it is complete
    /// (state `Verifying`); a mismatch fails the download and keeps the file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checksum: Option<crate::checksum::Checksum>,
    /// Other locations of the same file (URL downloads only), besides `source` (the
    /// primary), over HTTP(S), FTP(S) or SFTP whatever the primary's protocol. The engine probes
    /// them, uses the ones that agree with the primary on size and range support, spreads
    /// connections over them and fails over between them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mirrors: Vec<Mirror>,
    /// The file's size in bytes, when known in advance (a Metalink's `<size>`; URL
    /// downloads only). A location reporting another size is dropped when the download
    /// starts, and if every location that answers does, the download fails. One that does
    /// not report a size is used as before.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
}

/// Which files of a multi-file torrent to download.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum FileChoice {
    #[default]
    All,
    /// Wait in `ChoosingFiles` until the user selects.
    Ask,
    Select {
        selection: Selection,
    },
}

/// A set of torrent files: 0-based indices plus path globs.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Selection {
    #[serde(default)]
    pub indices: Vec<usize>,
    #[serde(default)]
    pub globs: Vec<String>,
}

/// When to stop seeding, per torrent. A `None` field inherits the daemon's global value;
/// `Some(0)` disables that trigger for this torrent.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SeedPolicy {
    #[serde(default)]
    pub ratio: Option<f32>,
    #[serde(default)]
    pub time_secs: Option<u64>,
}

/// One file of a torrent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TorrentFile {
    pub index: usize,
    pub path: String,
    pub size: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AttentionReason {
    RemoteChanged,
    NotResumable,
    ValidationFailed { detail: String },
}

/// The `ValidationFailed` detail of a seeding torrent whose data failed its check after
/// a restart: resuming it downloads the data again (unlike other reasons, which need a
/// restart).
pub const RECHECK_FAILED: &str = "data changed on disk; resume to download again";

impl AttentionReason {
    /// A seeding torrent's data failed its check (see [`RECHECK_FAILED`]): it can be
    /// resumed as is.
    pub fn is_resumable(&self) -> bool {
        matches!(self, AttentionReason::ValidationFailed { detail } if detail == RECHECK_FAILED)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DownloadState {
    Queued,
    Starting,
    Active,
    /// The file is complete and being hashed against the request's checksum;
    /// `done_bytes` counts the bytes hashed so far.
    Verifying,
    Paused,
    Completed,
    Failed {
        reason: String,
        /// The cause may go away by itself (network error, timeout, HTTP 5xx/429/408):
        /// retrying later can succeed. Otherwise retrying as is would fail the same way.
        #[serde(default)]
        transient: bool,
    },
    NeedsAttention {
        reason: AttentionReason,
    },
    /// Torrent metadata is known; waiting for the user to pick files.
    ChoosingFiles,
    /// Torrent complete and uploading.
    Seeding {
        uploaded: u64,
        ratio: f32,
        seeded_secs: u64,
        #[serde(default)]
        ratio_target: Option<f32>,
        #[serde(default)]
        time_target_secs: Option<u64>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DownloadStatus {
    pub state: DownloadState,
    pub name: String,
    /// Where the finished file is (or will be). Can differ from the path `add` returned:
    /// if a file appeared there meanwhile, the download is finalized under a free name.
    pub path: PathBuf,
    pub total_bytes: Option<u64>,
    pub done_bytes: u64,
    pub down_bps: u64,
    pub up_bps: u64,
    pub eta_secs: Option<u64>,
    /// Connected peers (torrents); `None` for engines without peers.
    #[serde(default)]
    pub peers: Option<u32>,
    /// Why the download is in its state, when the engine has something to say (a torrent
    /// choosing files although a choice was given: why that choice did not fit).
    #[serde(default)]
    pub note: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SegmentView {
    pub start: u64,
    /// Exclusive.
    pub end: u64,
    pub done: u64,
    pub bps: u64,
    pub active: bool,
}

/// One location of a URL download's file and how the download uses it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MirrorView {
    /// Display form (a password masked).
    pub url: String,
    /// The download's source; the others are mirrors.
    pub primary: bool,
    pub state: MirrorState,
    /// Connections open to it now.
    pub connections: u16,
    /// Current speed from it.
    pub bps: u64,
    /// Bytes this download received from it (this session).
    pub done_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum MirrorState {
    /// Agrees with the file and may get connections.
    InUse,
    /// Failed recently; tried again after `retry_in_secs`.
    Benched { reason: String, retry_in_secs: u64 },
    /// Not used any more by this run of the download (a resume tries it again).
    Dropped { reason: String },
    /// A state this build does not know (newer daemon).
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EngineDetail {
    /// A URL download (HTTP(S), FTP(S) or SFTP: the same segmented transfer).
    Http {
        segments: Vec<SegmentView>,
        connections: u16,
        resumable: bool,
        /// Every location of the file, the primary first, when the download has mirrors
        /// (empty otherwise).
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        mirrors: Vec<MirrorView>,
    },
    Torrent {
        piece_count: u32,
        /// Bitfield, MSB first.
        have: Vec<u8>,
        peers: u32,
        seeds: u32,
        leechers: u32,
        ratio: f32,
    },
    /// A kind this build does not know (sent by a newer daemon): no detail to show.
    #[serde(other)]
    Unknown,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn for_target_drops_credentials_across_origins() {
        let c = RequestContext {
            headers: vec![
                ("Authorization".into(), "Bearer a".into()),
                ("cookie".into(), "c=1".into()),
                ("PROXY-AUTHORIZATION".into(), "p".into()),
                ("X-Other".into(), "keep".into()),
            ],
            cookies: Some("sid=1".into()),
            referer: Some("https://example.com/p".into()),
            user_agent: Some("UA/1".into()),
            basic_auth: Some(("alice".into(), Some("pw".into()))),
            ssh_agent: Some("/run/agent.sock".into()),
        };
        let u = |s: &str| url::Url::parse(s).unwrap();
        let orig = u("https://a.example/f");
        assert_eq!(c.for_target(&orig, &u("https://a.example:443/g?x")), c);
        for other in [
            "http://a.example/f",
            "https://b.example/f",
            "https://a.example:8443/f",
        ] {
            let t = c.for_target(&orig, &u(other));
            assert_eq!(t.headers, vec![("X-Other".to_string(), "keep".to_string())]);
            assert_eq!(t.cookies, None);
            assert_eq!(t.basic_auth, None);
            assert_eq!(t.referer, c.referer);
            assert_eq!(t.user_agent, c.user_agent);
            assert_eq!(t.ssh_agent, c.ssh_agent);
            assert_eq!(c.for_mirror(&orig, &u(other)).ssh_agent, c.ssh_agent);
        }
    }

    #[test]
    fn debug_redacts_secrets() {
        let c = RequestContext {
            headers: vec![("Authorization".into(), "Bearer HDRSECRET".into())],
            cookies: Some("sid=COOKIESECRET".into()),
            referer: Some("https://example.com/p".into()),
            user_agent: Some("UA/1".into()),
            basic_auth: Some(("alice".into(), Some("PWSECRET".into()))),
            ssh_agent: None,
        };
        let s = format!("{c:?}");
        for secret in ["HDRSECRET", "COOKIESECRET", "PWSECRET"] {
            assert!(!s.contains(secret), "{s}");
        }
        for shown in ["Authorization", "alice", "https://example.com/p", "UA/1"] {
            assert!(s.contains(shown), "{s}");
        }
    }

    #[test]
    fn new_types_roundtrip_and_defaults() {
        let req: AddRequest = serde_json::from_str(
            r#"{"source":{"kind":"magnet","uri":"m"},"output_dir":null,"filename":null,"context":{"headers":[],"cookies":null,"referer":null,"user_agent":null,"basic_auth":null}}"#,
        )
        .unwrap();
        assert_eq!(req.files, FileChoice::All);
        assert_eq!(req.seed, None);
        let req = AddRequest {
            files: FileChoice::Select {
                selection: Selection {
                    indices: vec![0, 2],
                    globs: vec!["*.mkv".into()],
                },
            },
            seed: Some(SeedPolicy {
                ratio: Some(1.5),
                time_secs: Some(60),
            }),
            ..req
        };
        let back: AddRequest = serde_json::from_str(&serde_json::to_string(&req).unwrap()).unwrap();
        assert_eq!(back, req);
        assert_eq!(
            serde_json::to_string(&FileChoice::Ask).unwrap(),
            r#"{"kind":"ask"}"#
        );
        let sel: Selection = serde_json::from_str("{}").unwrap();
        assert_eq!(sel, Selection::default());
        let sp: SeedPolicy = serde_json::from_str("{}").unwrap();
        assert_eq!(sp, SeedPolicy::default());
        let f = TorrentFile {
            index: 1,
            path: "a/b".into(),
            size: 9,
        };
        assert_eq!(
            serde_json::from_str::<TorrentFile>(&serde_json::to_string(&f).unwrap()).unwrap(),
            f
        );
        for s in [
            DownloadState::ChoosingFiles,
            DownloadState::Seeding {
                uploaded: 5,
                ratio: 0.5,
                seeded_secs: 3,
                ratio_target: Some(1.0),
                time_target_secs: None,
            },
        ] {
            let back: DownloadState =
                serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
            assert_eq!(back, s);
        }
        assert_eq!(
            serde_json::to_string(&DownloadState::ChoosingFiles).unwrap(),
            r#"{"kind":"choosing_files"}"#
        );
    }

    #[test]
    fn download_state_serde_roundtrip() {
        let s = DownloadState::NeedsAttention {
            reason: AttentionReason::RemoteChanged,
        };
        let json = serde_json::to_string(&s).unwrap();
        assert_eq!(
            json,
            r#"{"kind":"needs_attention","reason":{"kind":"remote_changed"}}"#
        );
        let back: DownloadState = serde_json::from_str(&json).unwrap();
        assert!(matches!(
            back,
            DownloadState::NeedsAttention {
                reason: AttentionReason::RemoteChanged
            }
        ));
    }

    /// A detail kind from a newer daemon decodes as `Unknown` instead of failing.
    #[test]
    fn unknown_engine_detail_kind_decodes() {
        let d: EngineDetail =
            serde_json::from_str(r#"{"kind":"ftp","sessions":3,"extra":[1,2]}"#).unwrap();
        assert_eq!(d, EngineDetail::Unknown);
        let back: EngineDetail = serde_json::from_str(&serde_json::to_string(&d).unwrap()).unwrap();
        assert_eq!(back, EngineDetail::Unknown);
        let http = EngineDetail::Http {
            segments: Vec::new(),
            connections: 2,
            resumable: true,
            mirrors: Vec::new(),
        };
        let back: EngineDetail =
            serde_json::from_str(&serde_json::to_string(&http).unwrap()).unwrap();
        assert_eq!(back, http);
    }
}
