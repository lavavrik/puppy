//! Root certificates for systems that have none.
//!
//! On Linux and the BSDs, HTTPS trusts the CA certificates installed with the system
//! (the `ca-certificates` package). Minimal systems and containers can lack them, and
//! then no HTTPS client can even be built: neither puppy's nor the torrent library's.
//! [`use_bundled_roots_if_missing`] covers that with Mozilla's root list, compiled into
//! puppy. macOS and Windows use the OS's own certificate store and never need it.

use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// The bundled roots' file name in the directory given to [`use_bundled_roots_if_missing`].
pub const BUNDLED_ROOTS_FILE: &str = "ca-roots.pem";

static IN_USE: OnceLock<PathBuf> = OnceLock::new();

/// Whether the system offers no CA certificates at all (honouring `SSL_CERT_FILE` and
/// `SSL_CERT_DIR`). Always false on macOS and Windows.
pub fn system_roots_missing() -> bool {
    #[cfg(all(unix, not(target_vendor = "apple")))]
    {
        rustls_native_certs::load_native_certs().certs.is_empty()
    }
    #[cfg(not(all(unix, not(target_vendor = "apple"))))]
    {
        false
    }
}

/// Mozilla's root certificates (from `webpki-root-certs`) in PEM form.
pub fn bundled_roots_pem() -> String {
    use base64::Engine as _;
    let mut out = String::new();
    for cert in webpki_root_certs::TLS_SERVER_ROOT_CERTS {
        let b64 = base64::engine::general_purpose::STANDARD.encode(cert.as_ref());
        out.push_str("-----BEGIN CERTIFICATE-----\n");
        // Base64 is ASCII, so 64-byte chunks are whole characters.
        for line in b64.as_bytes().chunks(64) {
            out.push_str(std::str::from_utf8(line).expect("base64 is ASCII"));
            out.push('\n');
        }
        out.push_str("-----END CERTIFICATE-----\n");
    }
    out
}

/// If the system has no CA certificates, writes [`bundled_roots_pem`] to
/// `<dir>/ca-roots.pem` and points `SSL_CERT_FILE` at it, so every HTTPS client the
/// process builds afterwards trusts those roots. Returns the file when it did that.
///
/// # Safety
///
/// It sets an environment variable: call it before the process starts any other thread
/// (see [`std::env::set_var`]).
pub unsafe fn use_bundled_roots_if_missing(dir: &Path) -> io::Result<Option<PathBuf>> {
    if !system_roots_missing() {
        return Ok(None);
    }
    std::fs::create_dir_all(dir)?;
    let path = dir.join(BUNDLED_ROOTS_FILE);
    let tmp = dir.join(format!("{BUNDLED_ROOTS_FILE}.tmp"));
    let mut f = std::fs::File::create(&tmp)?;
    f.write_all(bundled_roots_pem().as_bytes())?;
    f.sync_all()?;
    drop(f);
    std::fs::rename(&tmp, &path)?;
    // SAFETY: the caller guarantees no other thread runs.
    unsafe { std::env::set_var("SSL_CERT_FILE", &path) };
    let _ = IN_USE.set(path.clone());
    Ok(Some(path))
}

/// The bundled roots file, once [`use_bundled_roots_if_missing`] switched to it.
pub fn bundled_roots_in_use() -> Option<&'static Path> {
    IN_USE.get().map(PathBuf::as_path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine as _;

    #[test]
    fn bundled_pem_holds_every_root_unchanged() {
        let pem = bundled_roots_pem();
        let blocks: Vec<&str> = pem
            .split("-----BEGIN CERTIFICATE-----\n")
            .skip(1)
            .map(|b| b.split("-----END CERTIFICATE-----").next().unwrap())
            .collect();
        assert_eq!(blocks.len(), webpki_root_certs::TLS_SERVER_ROOT_CERTS.len());
        assert!(blocks.len() > 100);
        for (block, cert) in blocks.iter().zip(webpki_root_certs::TLS_SERVER_ROOT_CERTS) {
            assert!(block.lines().all(|l| l.len() <= 64));
            let der = base64::engine::general_purpose::STANDARD
                .decode(block.replace('\n', ""))
                .unwrap();
            assert_eq!(der, cert.as_ref());
        }
    }
}
