//! Newline-delimited JSON framing with a hard per-line size cap.

use crate::MAX_LINE;
use serde::{Serialize, de::DeserializeOwned};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt};

#[derive(thiserror::Error, Debug)]
pub enum CodecError {
    /// A line exceeded [`MAX_LINE`]. The stream is left mid-line, so the
    /// connection is unusable afterwards: the caller must close it.
    #[error("message exceeds {MAX_LINE} bytes")]
    TooLong,
    #[error("invalid json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

/// Cancel-safe framed reader. Partial lines are kept inside the reader, so
/// dropping the `next` future (e.g. losing a `tokio::select!` race) never loses
/// bytes: the next call resumes the same line.
pub struct MsgReader<R> {
    inner: R,
    line: Vec<u8>,
}

impl<R: AsyncBufRead + Unpin> MsgReader<R> {
    pub fn new(r: R) -> Self {
        Self {
            inner: r,
            line: Vec::new(),
        }
    }

    /// Reads one message; cancel-safe. `Ok(None)` on clean EOF (no pending
    /// non-blank bytes). EOF in the middle of a line is `Io(UnexpectedEof)`.
    /// Never buffers more than `MAX_LINE` bytes of a line; `TooLong` leaves the
    /// stream mid-line, so the connection must then be closed. Blank lines are
    /// skipped and a trailing `\r` is stripped. A `Json` error consumes the
    /// offending line, so the stream stays usable.
    pub async fn next<T: DeserializeOwned>(&mut self) -> Result<Option<T>, CodecError> {
        loop {
            let buf = self.inner.fill_buf().await?;
            if buf.is_empty() {
                if self.line.iter().all(u8::is_ascii_whitespace) {
                    self.line.clear();
                    return Ok(None);
                }
                return Err(CodecError::Io(std::io::ErrorKind::UnexpectedEof.into()));
            }
            // No awaits below: state update and consume are atomic w.r.t. cancellation.
            let pos = buf.iter().position(|&b| b == b'\n');
            let take = pos.unwrap_or(buf.len());
            if self.line.len() + take > MAX_LINE {
                return Err(CodecError::TooLong);
            }
            self.line.extend_from_slice(&buf[..take]);
            self.inner.consume(take + usize::from(pos.is_some()));
            if pos.is_none() {
                continue;
            }
            let mut line = std::mem::take(&mut self.line);
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            if line.iter().all(u8::is_ascii_whitespace) {
                continue;
            }
            return Ok(Some(serde_json::from_slice(&line)?));
        }
    }
}

/// Convenience one-shot read. NOT cancel-safe: if the future is dropped
/// mid-line, the partial line is lost. Use [`MsgReader`] inside `select!`.
pub async fn read_msg<R: AsyncBufRead + Unpin, T: DeserializeOwned>(
    r: &mut R,
) -> Result<Option<T>, CodecError> {
    MsgReader::new(r).next().await
}

/// Writes `msg` as one JSON line and flushes. Refuses frames over `MAX_LINE`
/// (`TooLong`) without writing anything.
pub async fn write_msg<W: AsyncWrite + Unpin, T: Serialize>(
    w: &mut W,
    msg: &T,
) -> Result<(), CodecError> {
    let mut bytes = serde_json::to_vec(msg)?;
    if bytes.len() > MAX_LINE {
        return Err(CodecError::TooLong);
    }
    bytes.push(b'\n');
    w.write_all(&bytes).await?;
    w.flush().await?;
    Ok(())
}
