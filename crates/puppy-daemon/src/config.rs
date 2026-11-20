//! TOML configuration for the daemon.

use std::path::{Path, PathBuf};
use std::time::Duration;

use puppy_core::config::HttpConfig;
use serde::{Deserialize, Serialize};

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("cannot read config {path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("invalid config {path}{}: {message}", fmt_pos(*.line, *.column))]
    Parse {
        path: PathBuf,
        /// 1-based line, when known.
        line: Option<usize>,
        /// 1-based column, when known.
        column: Option<usize>,
        message: String,
    },
}

fn fmt_pos(line: Option<usize>, column: Option<usize>) -> String {
    match (line, column) {
        (Some(l), Some(c)) => format!(" at line {l}, column {c}"),
        _ => String::new(),
    }
}

/// The `[torrent]` table.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(default)]
pub struct TorrentConfig {
    /// Use the DHT to find peers.
    pub dht: bool,
    /// Map the listen port with UPnP.
    pub upnp: bool,
    /// Find peers on the local network (local service discovery).
    pub lsd: bool,
    /// Port to listen on for peers (an OS-assigned one if busy).
    pub listen_port: u16,
    /// Seed until this share ratio is reached (`0` disables the trigger).
    pub seed_ratio: f32,
    /// Seed for at most this long (`"2h"`, `"30m"`, `0` disables the trigger). With both
    /// triggers off a torrent stops at completion.
    #[serde(with = "duration_text")]
    pub seed_time: Duration,
}

/// A duration as a human string (`"2h"`, `"30m"`); a bare integer is seconds (`0`).
mod duration_text {
    use serde::{Deserialize, Deserializer, Serializer, de};
    use std::time::Duration;

    pub fn serialize<S: Serializer>(d: &Duration, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(&humantime::format_duration(*d))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(de: D) -> Result<Duration, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Text {
            Secs(u64),
            Str(String),
        }
        match Text::deserialize(de)? {
            Text::Secs(n) => Ok(Duration::from_secs(n)),
            Text::Str(s) => match s.trim() {
                "0" => Ok(Duration::ZERO),
                t => humantime::parse_duration(t).map_err(|e| {
                    de::Error::custom(format!(
                        "seed_time: invalid duration {t:?} ({e}); use e.g. \"2h\", \"30m\" or 0"
                    ))
                }),
            },
        }
    }
}

impl Default for TorrentConfig {
    fn default() -> Self {
        Self {
            dht: true,
            upnp: false,
            lsd: true,
            listen_port: 42069,
            seed_ratio: 1.0,
            seed_time: Duration::from_secs(2 * 3600),
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(default)]
pub struct Config {
    pub download_dir: PathBuf,
    pub connections: u16,
    pub max_active: u16,
    pub global_down_limit: Option<u64>,
    pub global_up_limit: Option<u64>,
    pub theme: String,
    pub proxy: Option<url::Url>,
    pub torrent: TorrentConfig,
    /// First delay before a transiently failed download is retried automatically;
    /// doubles per failure up to [`MAX_RETRY_DELAY`]; never less than 1 s. Not read from
    /// the file.
    #[serde(skip)]
    pub retry_base: Duration,
    /// How long shutdown waits for downloads to pause (flushing their sidecars) before
    /// it persists the state regardless. Not read from the file.
    #[serde(skip)]
    pub shutdown_pause_timeout: Duration,
}

/// The longest delay between automatic retries of a transiently failed download.
pub const MAX_RETRY_DELAY: Duration = Duration::from_secs(15 * 60);

impl Default for Config {
    fn default() -> Self {
        Self {
            download_dir: dirs::download_dir()
                .or_else(|| dirs::home_dir().map(|h| h.join("Downloads")))
                .unwrap_or_else(|| PathBuf::from("Downloads")),
            connections: 8,
            max_active: 3,
            global_down_limit: None,
            global_up_limit: None,
            theme: "auto".into(),
            proxy: None,
            torrent: TorrentConfig::default(),
            retry_base: Duration::from_secs(30),
            shutdown_pause_timeout: Duration::from_secs(10),
        }
    }
}

impl Config {
    /// Load from `path`; a missing file yields the defaults.
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(source) => {
                return Err(ConfigError::Io {
                    path: path.to_path_buf(),
                    source,
                });
            }
        };
        toml::from_str(&text).map_err(|e| {
            let (line, column) = match e.span() {
                Some(span) => {
                    let before = &text[..span.start.min(text.len())];
                    let line = before.matches('\n').count() + 1;
                    let col = before.rsplit('\n').next().map_or(0, |l| l.chars().count()) + 1;
                    (Some(line), Some(col))
                }
                None => (None, None),
            };
            ConfigError::Parse {
                path: path.to_path_buf(),
                line,
                column,
                message: e.message().to_string(),
            }
        })
    }

    pub fn http_config(&self) -> HttpConfig {
        HttpConfig {
            connections: self.connections,
            proxy: self.proxy.clone(),
            default_dir: self.download_dir.clone(),
            ..HttpConfig::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `Config::default` reads the environment (the Downloads folder), which other
    /// tests change.
    fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        crate::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    #[test]
    fn config_missing_is_default() {
        let _env = env_lock();
        let d = tempfile::tempdir().unwrap();
        let c = Config::load(&d.path().join("nope.toml")).unwrap();
        assert_eq!(c, Config::default());
    }

    #[test]
    fn config_partial_fills_defaults() {
        let _env = env_lock();
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("c.toml");
        std::fs::write(&p, "max_active = 5\n").unwrap();
        let c = Config::load(&p).unwrap();
        assert_eq!(c.max_active, 5);
        assert_eq!(c.connections, 8);
        assert_eq!(c.theme, "auto");
    }

    #[test]
    fn config_torrent_table_defaults_and_overrides() {
        let _env = env_lock();
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("c.toml");
        std::fs::write(&p, "max_active = 5\n").unwrap();
        let t = Config::load(&p).unwrap().torrent;
        assert!(t.dht && !t.upnp && t.lsd);
        assert_eq!(t.listen_port, 42069);
        assert_eq!(t.seed_ratio, 1.0);
        assert_eq!(t.seed_time, Duration::from_secs(7200));
        std::fs::write(&p, "[torrent]\ndht = false\nlisten_port = 51413\n").unwrap();
        let t = Config::load(&p).unwrap().torrent;
        assert!(!t.dht && !t.upnp);
        assert_eq!(t.listen_port, 51413);
        assert_eq!(t.seed_ratio, 1.0);
        std::fs::write(&p, "[torrent]\nupnp = true\nseed_ratio = 1.5\n").unwrap();
        let t = Config::load(&p).unwrap().torrent;
        assert!(t.dht && t.upnp);
        assert_eq!(t.seed_ratio, 1.5);
        std::fs::write(&p, "[torrent]\nlsd = false\n").unwrap();
        assert!(!Config::load(&p).unwrap().torrent.lsd);
    }

    #[test]
    fn config_seed_time_forms() {
        let _env = env_lock();
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("c.toml");
        for (text, want) in [
            ("seed_time = \"30m\"", 1800),
            ("seed_time = 0", 0),
            ("seed_time = \"0\"", 0),
            ("seed_time = \"1h 30m\"", 5400),
        ] {
            std::fs::write(&p, format!("[torrent]\n{text}\n")).unwrap();
            let t = Config::load(&p).unwrap().torrent;
            assert_eq!(t.seed_time, Duration::from_secs(want), "{text}");
        }
        std::fs::write(&p, "[torrent]\nseed_time = \"soon\"\n").unwrap();
        let err = Config::load(&p).unwrap_err().to_string();
        assert!(err.contains("seed_time"), "{err}");
        // Serializes back to something it can read.
        let t = TorrentConfig::default();
        let s = toml::to_string(&t).unwrap();
        assert_eq!(toml::from_str::<TorrentConfig>(&s).unwrap(), t);
    }

    #[test]
    fn config_invalid_reports_error() {
        let _env = env_lock();
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("c.toml");
        std::fs::write(&p, "theme = \"dark\"\nmax_active = [oops\n").unwrap();
        let e = Config::load(&p).unwrap_err();
        match &e {
            ConfigError::Parse { line, .. } => assert_eq!(*line, Some(2)),
            other => panic!("unexpected {other:?}"),
        }
        assert!(e.to_string().contains("line 2"), "{e}");
    }

    #[test]
    fn http_config_maps_fields() {
        let _env = env_lock();
        let c = Config {
            connections: 4,
            proxy: Some("http://127.0.0.1:3128".parse().unwrap()),
            download_dir: PathBuf::from("/x/dl"),
            ..Config::default()
        };
        let h = c.http_config();
        assert_eq!(h.connections, 4);
        assert_eq!(h.proxy, c.proxy);
        assert_eq!(h.default_dir, PathBuf::from("/x/dl"));
        assert_eq!(h.max_retries, HttpConfig::default().max_retries);
    }
}
