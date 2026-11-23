//! Native messaging framing: a 4-byte length in native byte order, then that many
//! bytes of UTF-8 JSON.

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// The largest message the host may send to the browser (Chrome's limit).
pub const MAX_OUT: usize = 1 << 20;
/// The largest message the host accepts from the browser (requests are small).
pub const MAX_IN: usize = 1 << 20;

#[derive(thiserror::Error, Debug)]
pub enum FrameError {
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("message of {len} bytes is over {max}")]
    TooLarge { len: usize, max: usize },
    #[error("message cut short")]
    Truncated,
}

/// Reads one message. `None`: the input ended cleanly, before a length.
pub async fn read_frame<R: AsyncRead + Unpin>(r: &mut R) -> Result<Option<Vec<u8>>, FrameError> {
    let mut header = [0u8; 4];
    let mut got = 0;
    while got < header.len() {
        match r.read(&mut header[got..]).await? {
            0 if got == 0 => return Ok(None),
            0 => return Err(FrameError::Truncated),
            n => got += n,
        }
    }
    let len = u32::from_ne_bytes(header) as usize;
    if len > MAX_IN {
        return Err(FrameError::TooLarge { len, max: MAX_IN });
    }
    let mut body = vec![0u8; len];
    r.read_exact(&mut body).await.map_err(|e| {
        if e.kind() == std::io::ErrorKind::UnexpectedEof {
            FrameError::Truncated
        } else {
            FrameError::Io(e)
        }
    })?;
    Ok(Some(body))
}

/// Writes one message and flushes it. Nothing is written for one over [`MAX_OUT`].
pub async fn write_frame<W: AsyncWrite + Unpin>(w: &mut W, json: &[u8]) -> Result<(), FrameError> {
    if json.len() > MAX_OUT {
        return Err(FrameError::TooLarge {
            len: json.len(),
            max: MAX_OUT,
        });
    }
    // MAX_OUT fits in a u32.
    let len = json.len() as u32;
    w.write_all(&len.to_ne_bytes()).await?;
    w.write_all(json).await?;
    w.flush().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn frame_roundtrip_native_endian() {
        let body = br#"{"a":1}"#;
        let mut buf = Vec::new();
        write_frame(&mut buf, body).await.unwrap();
        assert_eq!(buf, [&7u32.to_ne_bytes()[..], &body[..]].concat());
        let mut r = &buf[..];
        assert_eq!(read_frame(&mut r).await.unwrap(), Some(body.to_vec()));
        assert_eq!(read_frame(&mut r).await.unwrap(), None);
    }

    #[tokio::test]
    async fn oversized_reply_is_refused() {
        let mut buf = Vec::new();
        let r = write_frame(&mut buf, &vec![b' '; MAX_OUT + 1]).await;
        assert!(
            matches!(r, Err(FrameError::TooLarge { len, max }) if len == MAX_OUT + 1 && max == 1 << 20),
            "{r:?}"
        );
        assert!(buf.is_empty());
        write_frame(&mut buf, &vec![b' '; MAX_OUT]).await.unwrap();
        assert_eq!(buf.len(), 4 + MAX_OUT);
    }

    #[tokio::test]
    async fn oversized_or_cut_input_is_an_error() {
        let big = ((MAX_IN + 1) as u32).to_ne_bytes();
        let r = read_frame(&mut &big[..]).await;
        assert!(
            matches!(r, Err(FrameError::TooLarge { len, max }) if len == MAX_IN + 1 && max == MAX_IN),
            "{r:?}"
        );

        let cut = [&10u32.to_ne_bytes()[..], b"abc"].concat();
        let r = read_frame(&mut &cut[..]).await;
        assert!(matches!(r, Err(FrameError::Truncated)), "{r:?}");

        let half = &7u32.to_ne_bytes()[..2];
        let r = read_frame(&mut &half[..]).await;
        assert!(matches!(r, Err(FrameError::Truncated)), "{r:?}");
    }
}
