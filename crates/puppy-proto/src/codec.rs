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

/// Reads one message. `Ok(None)` on clean EOF (no pending bytes). Never buffers
/// more than `MAX_LINE` bytes of a line; returns `TooLong` as soon as the cap is
/// exceeded (the connection must then be closed). Blank lines are skipped. A
/// `Json` error consumes the offending line, so the stream stays usable.
pub async fn read_msg<R: AsyncBufRead + Unpin, T: DeserializeOwned>(
    r: &mut R,
) -> Result<Option<T>, CodecError> {
    loop {
        let mut line: Vec<u8> = Vec::new();
        let mut saw_newline = false;
        loop {
            let buf = r.fill_buf().await?;
            if buf.is_empty() {
                break;
            }
            match buf.iter().position(|&b| b == b'\n') {
                Some(i) => {
                    if line.len() + i > MAX_LINE {
                        return Err(CodecError::TooLong);
                    }
                    line.extend_from_slice(&buf[..i]);
                    r.consume(i + 1);
                    saw_newline = true;
                    break;
                }
                None => {
                    let n = buf.len();
                    if line.len() + n > MAX_LINE {
                        return Err(CodecError::TooLong);
                    }
                    line.extend_from_slice(buf);
                    r.consume(n);
                }
            }
        }
        if line.last() == Some(&b'\r') {
            line.pop();
        }
        if line.iter().all(u8::is_ascii_whitespace) {
            if saw_newline {
                continue;
            }
            return Ok(None);
        }
        return Ok(Some(serde_json::from_slice(&line)?));
    }
}

/// Writes `msg` as one JSON line and flushes.
pub async fn write_msg<W: AsyncWrite + Unpin, T: Serialize>(
    w: &mut W,
    msg: &T,
) -> Result<(), CodecError> {
    let mut bytes = serde_json::to_vec(msg)?;
    bytes.push(b'\n');
    w.write_all(&bytes).await?;
    w.flush().await?;
    Ok(())
}
