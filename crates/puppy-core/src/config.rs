//! Engine configuration.

use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct HttpConfig {
    pub connections: u16,
    pub min_segment_bytes: u64,
    pub max_retries: u32,
    pub backoff_base_ms: u64,
    pub backoff_max_ms: u64,
    pub flush_interval_ms: u64,
    pub spot_check_bytes: u64,
    /// TCP/TLS connect timeout.
    pub connect_timeout_ms: u64,
    /// Idle timeout per read (headers or body); a stalled connection errors out.
    pub read_timeout_ms: u64,
    /// Upper bound for validating a remote file before resuming it.
    pub validate_timeout_ms: u64,
    pub proxy: Option<url::Url>,
    pub default_dir: PathBuf,
}

impl Default for HttpConfig {
    fn default() -> Self {
        Self {
            connections: 8,
            min_segment_bytes: 1024 * 1024,
            max_retries: 8,
            backoff_base_ms: 500,
            backoff_max_ms: 30_000,
            flush_interval_ms: 1000,
            spot_check_bytes: 64 * 1024,
            connect_timeout_ms: 15_000,
            read_timeout_ms: 30_000,
            validate_timeout_ms: 60_000,
            proxy: None,
            default_dir: dirs::download_dir()
                .or_else(|| dirs::home_dir().map(|h| h.join("Downloads")))
                .unwrap_or_else(|| PathBuf::from("Downloads")),
        }
    }
}
