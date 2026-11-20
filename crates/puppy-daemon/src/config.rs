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
    /// First delay before a transiently failed download is retried automatically;
    /// doubles per failure up to [`MAX_RETRY_DELAY`]. Not read from the file.
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

    #[test]
    fn config_missing_is_default() {
        let d = tempfile::tempdir().unwrap();
        let c = Config::load(&d.path().join("nope.toml")).unwrap();
        assert_eq!(c, Config::default());
    }

    #[test]
    fn config_partial_fills_defaults() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("c.toml");
        std::fs::write(&p, "max_active = 5\n").unwrap();
        let c = Config::load(&p).unwrap();
        assert_eq!(c.max_active, 5);
        assert_eq!(c.connections, 8);
        assert_eq!(c.theme, "auto");
    }

    #[test]
    fn config_invalid_reports_error() {
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
