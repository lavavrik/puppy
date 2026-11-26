//! TLS for FTPS: the same trust as HTTPS (the platform's verifier, i.e. the system's root
//! certificates, or the roots puppy bundles where the system has none: see
//! [`crate::tls`]), with aws-lc-rs as the crypto provider.

use crate::config::HttpConfig;
use crate::engine::EngineError;
use rustls::ClientConfig;
use rustls::client::Resumption;
use rustls::pki_types::CertificateDer;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use suppaftp::tokio::{AsyncRustlsConnector, AsyncRustlsStream, AsyncTlsConnector};

/// A session's connector that records whether a handshake was started: suppaftp's
/// `into_secure` only starts one once the server accepted `AUTH TLS`, so a failure after
/// that (`PBSZ`/`PROT` refused, a bad certificate) is never mistaken for a server
/// without TLS.
#[derive(Debug)]
pub(super) struct Connector {
    inner: AsyncRustlsConnector,
    started: Arc<AtomicBool>,
}

impl Connector {
    /// Set once the first handshake starts.
    pub fn started(&self) -> Arc<AtomicBool> {
        self.started.clone()
    }
}

#[async_trait::async_trait]
impl AsyncTlsConnector for Connector {
    type Stream = AsyncRustlsStream;

    async fn connect(
        &self,
        domain: &str,
        stream: tokio::net::TcpStream,
    ) -> suppaftp::FtpResult<AsyncRustlsStream> {
        self.started.store(true, Ordering::SeqCst);
        self.inner.connect(domain, stream).await
    }
}

static DEFAULT: OnceLock<Result<Arc<ClientConfig>, String>> = OnceLock::new();

fn build(extra_roots: &[Vec<u8>]) -> Result<Arc<ClientConfig>, String> {
    let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    let roots: Vec<CertificateDer<'static>> = extra_roots
        .iter()
        .map(|der| CertificateDer::from(der.clone()))
        .collect();
    let verifier = if roots.is_empty() {
        rustls_platform_verifier::Verifier::new(provider.clone())
    } else {
        rustls_platform_verifier::Verifier::new_with_extra_roots(roots, provider.clone())
    }
    .map_err(|e| format!("cannot set up certificate verification: {e}"))?;
    let cfg = ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| e.to_string())?
        .dangerous() // Not dangerous: the platform verifier verifies.
        .with_custom_certificate_verifier(Arc::new(verifier))
        .with_no_client_auth();
    Ok(Arc::new(cfg))
}

/// A connector for one session (its control connection and the data connections it
/// opens). Each session gets its own resumption cache: servers that insist on data
/// connections resuming the control connection's TLS session (vsftpd's
/// `require_ssl_reuse`, ProFTPD's default) then find exactly that session offered.
pub(super) fn connector(cfg: &HttpConfig) -> Result<Connector, EngineError> {
    let base = if cfg.ftp.extra_roots.is_empty() {
        DEFAULT.get_or_init(|| build(&[])).clone()
    } else {
        build(&cfg.ftp.extra_roots)
    }
    .map_err(EngineError::Tls)?;
    let mut session = (*base).clone();
    session.resumption = Resumption::in_memory_sessions(8);
    Ok(Connector {
        inner: AsyncRustlsConnector::from(tokio_rustls_connector(session)),
        started: Arc::new(AtomicBool::new(false)),
    })
}

fn tokio_rustls_connector(cfg: ClientConfig) -> suppaftp::tokio_rustls::TlsConnector {
    suppaftp::tokio_rustls::TlsConnector::from(Arc::new(cfg))
}
