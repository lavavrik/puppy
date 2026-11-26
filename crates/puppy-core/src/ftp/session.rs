//! One FTP session: a control connection, secured as its URL and the configuration ask,
//! logged in, in binary mode and passive (`EPSV`, or `PASV` when that fails).

use super::{Endpoint, Wire, ftp_err, reply_err, tls};
use crate::config::{FtpTls, HttpConfig};
use crate::engine::EngineError;
use crate::http::probe::timed;
use std::net::SocketAddr;
use std::time::Duration;
use suppaftp::tokio::{AsyncRustlsFtpStream, TransferStream};
use suppaftp::types::{FileType, Mode};
use suppaftp::{FtpError, Status};
use tokio::net::TcpStream;

pub(crate) type Stream = TransferStream<suppaftp::tokio::AsyncRustlsStream>;

pub(crate) struct Session {
    ftp: AsyncRustlsFtpStream,
    pasv: bool,
}

/// Reply codes refusing `AUTH TLS` (not supported, not available): plain FTP may go on
/// when TLS was only tried.
fn refuses_tls(e: &FtpError) -> bool {
    matches!(e, FtpError::UnexpectedResponse(r) if matches!(reply_code(&r.body), 500..=504 | 530 | 534 | 431))
}

fn reply_code(body: &[u8]) -> u16 {
    body.get(..3)
        .and_then(|c| std::str::from_utf8(c).ok())
        .and_then(|c| c.parse().ok())
        .unwrap_or(0)
}

async fn resolve(host: &str, port: u16, timeout: Duration) -> Result<Vec<SocketAddr>, EngineError> {
    Ok(timed(timeout, tokio::net::lookup_host((host, port)))
        .await?
        .map_err(|e| EngineError::Network(format!("cannot resolve {host}: {e}")))?
        .collect())
}

/// Resolves and connects to `host:port`, trying each address in turn.
async fn connect_tcp(host: &str, port: u16, timeout: Duration) -> Result<TcpStream, EngineError> {
    let addrs = resolve(host, port, timeout).await?;
    let mut last = None;
    for addr in addrs {
        match tokio::time::timeout(timeout, TcpStream::connect(addr)).await {
            Ok(Ok(s)) => return Ok(s),
            Ok(Err(e)) => last = Some(e.to_string()),
            Err(_) => last = Some("connection timed out".into()),
        }
    }
    Err(EngineError::Network(format!(
        "cannot connect to {host}:{port}: {}",
        last.unwrap_or_else(|| "no address".into())
    )))
}

impl Session {
    /// Opens a session. `tls_refused`: the server refused `AUTH TLS` before (with TLS
    /// only tried, it is not asked again). Returns whether it refused it this time.
    pub async fn open(
        ep: &Endpoint,
        cfg: &HttpConfig,
        pasv: bool,
        tls_refused: bool,
    ) -> Result<(Session, bool), EngineError> {
        let connect = Duration::from_millis(cfg.connect_timeout_ms);
        let read = Duration::from_millis(cfg.read_timeout_ms);
        let want_tls = match ep.wire {
            Wire::Implicit => None,
            Wire::Explicit => Some(true),
            Wire::Configured => match cfg.ftp.tls {
                FtpTls::Off => None,
                FtpTls::Try if tls_refused => None,
                FtpTls::Try => Some(false),
                FtpTls::Require => Some(true),
            },
        };
        let mut refused = false;
        let mut ftp = if ep.wire == Wire::Implicit {
            let mut ftp = Self::implicit(ep, cfg, connect + read).await?;
            // Data connections are protected too (as after `AUTH TLS`).
            for cmd in ["PBSZ 0", "PROT P"] {
                timed(read, ftp.custom_command(cmd, &[Status::CommandOk]))
                    .await?
                    .map_err(ftp_err)?;
            }
            ftp
        } else {
            let ftp = Self::greet(ep, connect, read).await?;
            match want_tls {
                None => ftp,
                Some(required) => {
                    let connector = tls::connector(cfg)?;
                    match timed(read, ftp.into_secure(connector, &ep.host)).await? {
                        Ok(ftp) => ftp,
                        Err(e) if refuses_tls(&e) && !required => {
                            tracing::info!(
                                host = %ep.host,
                                "the FTP server does not offer TLS; continuing without"
                            );
                            refused = true;
                            // The refused connection is gone with `into_secure`: open
                            // another one.
                            Self::greet(ep, connect, read).await?
                        }
                        Err(e) if refuses_tls(&e) => {
                            let EngineError::Ftp { code, message } = ftp_err(e) else {
                                unreachable!("a refusal is a reply");
                            };
                            return Err(EngineError::Tls(format!(
                                "the server refused AUTH TLS ({code} {message}), and TLS is \
                                 required"
                            )));
                        }
                        Err(e) => return Err(ftp_err(e)),
                    }
                }
            }
        };
        // A `PASV` reply's address is ignored (NATed servers send private ones; a
        // hostile one could point elsewhere): data connections go to the server's.
        ftp.set_passive_nat_workaround(true);
        ftp.set_mode(if pasv {
            Mode::Passive
        } else {
            Mode::ExtendedPassive
        });
        let ftp = ftp.passive_stream_builder(move |addr| {
            Box::pin(async move {
                match tokio::time::timeout(connect, TcpStream::connect(addr)).await {
                    Ok(r) => r.map_err(FtpError::ConnectionError),
                    Err(_) => Err(FtpError::ConnectionError(std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        "data connection timed out",
                    ))),
                }
            })
        });
        let mut ftp = ftp;
        timed(read, ftp.login(ep.user.as_str(), ep.pass.as_str()))
            .await?
            .map_err(ftp_err)?;
        timed(read, ftp.transfer_type(FileType::Binary))
            .await?
            .map_err(ftp_err)?;
        Ok((Session { ftp, pasv }, refused))
    }

    /// An implicit-TLS control connection with its greeting read, trying each address of
    /// the host in turn.
    async fn implicit(
        ep: &Endpoint,
        cfg: &HttpConfig,
        timeout: Duration,
    ) -> Result<AsyncRustlsFtpStream, EngineError> {
        let mut last = EngineError::Network(format!("cannot resolve {}", ep.host));
        for addr in resolve(&ep.host, ep.port, timeout).await? {
            let connector = tls::connector(cfg)?;
            let got = timed(
                timeout,
                AsyncRustlsFtpStream::connect_secure_implicit(addr, connector, &ep.host),
            )
            .await;
            match got {
                Ok(Ok(ftp)) => return Ok(ftp),
                Ok(Err(e @ FtpError::ConnectionError(_))) => last = ftp_err(e),
                Ok(Err(e)) => return Err(ftp_err(e)),
                Err(e) => last = e,
            }
        }
        Err(last)
    }

    /// A plain control connection with its greeting read.
    async fn greet(
        ep: &Endpoint,
        connect: Duration,
        read: Duration,
    ) -> Result<AsyncRustlsFtpStream, EngineError> {
        let tcp = connect_tcp(&ep.host, ep.port, connect).await?;
        timed(read, AsyncRustlsFtpStream::connect_with_stream(tcp))
            .await?
            .map_err(ftp_err)
    }

    /// Logs out (draining the reply of an abandoned transfer first), within `limit`.
    /// Failures do not matter: the connection is closed either way.
    pub async fn quit(&mut self, limit: Duration) {
        let _ = tokio::time::timeout(limit, self.ftp.quit()).await;
    }

    /// The server's features (`FEAT`), `None` if it cannot list them.
    pub async fn features(
        &mut self,
        cfg: &HttpConfig,
    ) -> Result<Option<suppaftp::types::Features>, EngineError> {
        let read = Duration::from_millis(cfg.read_timeout_ms);
        match timed(read, self.ftp.feat()).await? {
            Ok(f) => Ok(Some(
                f.into_iter()
                    .map(|(k, v)| (k.to_ascii_uppercase(), v))
                    .collect(),
            )),
            Err(FtpError::UnexpectedResponse(_) | FtpError::BadResponse) => Ok(None),
            Err(e) => Err(ftp_err(e)),
        }
    }

    /// `SIZE`: `None` if the server does not implement it. A 550 ("no such file", or not
    /// a plain file) is an error.
    pub async fn size(&mut self, path: &str, cfg: &HttpConfig) -> Result<Option<u64>, EngineError> {
        let read = Duration::from_millis(cfg.read_timeout_ms);
        match self.query(&format!("SIZE {path}"), read).await? {
            Ok(text) => text.parse::<u64>().map(Some).map_err(|_| {
                EngineError::Network(format!("the FTP server sent a bad size ({text:?})"))
            }),
            // Not implemented.
            Err(_) => Ok(None),
        }
    }

    /// `MDTM` (as sent: `YYYYMMDDHHMMSS[.sss]`): `None` if the server does not tell.
    pub async fn mdtm(
        &mut self,
        path: &str,
        cfg: &HttpConfig,
    ) -> Result<Option<String>, EngineError> {
        let read = Duration::from_millis(cfg.read_timeout_ms);
        match self.query(&format!("MDTM {path}"), read).await {
            Ok(Ok(text)) if !text.is_empty() => Ok(Some(text)),
            // Not implemented, or refused for this file: no modification time known.
            Ok(_) | Err(EngineError::Ftp { code: 500.., .. }) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// A command answered by `213 <value>`: `Ok(Ok(value))`, `Ok(Err(code))` for a 50x
    /// "not implemented" reply, `Err` for any other refusal or a failure.
    async fn query(
        &mut self,
        cmd: &str,
        read: Duration,
    ) -> Result<Result<String, u16>, EngineError> {
        match timed(read, self.ftp.custom_command(cmd, &[Status::File])).await? {
            Ok(r) => {
                let text = String::from_utf8_lossy(&r.body);
                let first = text.lines().next().unwrap_or("");
                Ok(Ok(first.get(4..).unwrap_or("").trim().to_string()))
            }
            Err(FtpError::UnexpectedResponse(r)) => match reply_code(&r.body) {
                code @ 500..=504 => Ok(Err(code)),
                _ => Err(reply_err(&r.body)),
            },
            Err(e) => Err(ftp_err(e)),
        }
    }

    /// Starts `RETR path` at `offset`. If the data connection cannot be set up the way
    /// this session tries (`EPSV` refused, say), the other passive mode is tried once:
    /// `Ok((stream, Some(pasv)))` tells which one worked then. Refusals of both are the
    /// second's error; with an offset, they may well be the server refusing `REST`.
    /// `pasv`: the passive mode the site has settled on (another session may have
    /// learned it since this one opened).
    pub async fn retr(
        &mut self,
        path: &str,
        offset: u64,
        pasv: bool,
        cfg: &HttpConfig,
    ) -> Result<(Stream, Option<bool>), RetrError> {
        let read = Duration::from_millis(cfg.read_timeout_ms);
        if pasv != self.pasv {
            self.pasv = pasv;
            self.ftp.set_mode(if pasv {
                Mode::Passive
            } else {
                Mode::ExtendedPassive
            });
        }
        let offset = usize::try_from(offset)
            .map_err(|_| RetrError::Other(EngineError::Invalid("offset too large".into())))?;
        let mut switched = None;
        for attempt in 0..2 {
            let _ = self.ftp.resume_transfer(offset).await;
            let got = timed(read, self.ftp.retr_as_stream(path))
                .await
                .map_err(RetrError::Other)?;
            match got {
                Ok(stream) => return Ok((stream, switched)),
                Err(FtpError::UnexpectedResponse(r))
                    if attempt == 0 && matches!(reply_code(&r.body), 500..=504 | 522) =>
                {
                    self.pasv = !self.pasv;
                    self.ftp.set_mode(if self.pasv {
                        Mode::Passive
                    } else {
                        Mode::ExtendedPassive
                    });
                    switched = Some(self.pasv);
                }
                Err(FtpError::UnexpectedResponse(r))
                    if offset > 0 && matches!(reply_code(&r.body), 500..=504) =>
                {
                    return Err(RetrError::RestRefused);
                }
                Err(e) => return Err(RetrError::Other(ftp_err(e))),
            }
        }
        unreachable!("the second attempt returns")
    }
}

pub(crate) enum RetrError {
    /// The server refused the restart offset (or both passive modes).
    RestRefused,
    Other(EngineError),
}
