//! HTTP download engine building blocks.

pub mod download;
pub mod engine;
pub mod probe;
pub mod resume;
pub mod segments;
pub mod single;
pub mod validate;
pub mod worker;

/// The URL schemes a URL download (its source and mirrors) may have: `http`, `https`,
/// `ftp`, `ftps`, `ftpes` and `sftp`.
pub fn is_url_scheme(scheme: &str) -> bool {
    matches!(scheme, "http" | "https")
        || crate::ftp::is_ftp_scheme(scheme)
        || crate::sftp::is_sftp_scheme(scheme)
}
