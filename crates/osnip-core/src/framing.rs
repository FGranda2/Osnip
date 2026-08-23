//! Length-prefixed JSON framing over an async byte stream.
//!
//! Wire format per frame:
//! ```text
//! [ len: u32 big-endian ][ json: len bytes UTF-8 ]
//! ```
//!
//! No trailers, no checksums — the underlying transport is `AF_UNIX`,
//! which is already reliable and bytewise-ordered. Frames over
//! [`MAX_FRAME_BYTES`] are rejected before allocation to bound memory
//! pressure from a malformed or hostile peer.

use serde::{de::DeserializeOwned, Serialize};
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Hard ceiling on a single frame's payload size, in bytes (1 MiB).
///
/// IPC payloads here are tiny (request enums, list summaries). A 1 MiB
/// cap is generous for the protocol while keeping a misbehaving peer
/// from forcing a multi-gigabyte allocation.
pub const MAX_FRAME_BYTES: u32 = 1024 * 1024;

/// Errors raised by [`read_frame`] / [`write_frame`].
#[derive(Debug, Error)]
pub enum FrameError {
    /// Underlying I/O failure (peer closed, broken pipe, etc.).
    #[error("framing i/o: {0}")]
    Io(#[from] std::io::Error),

    /// Peer announced a frame larger than [`MAX_FRAME_BYTES`].
    #[error("frame too large: {len} bytes (max {max})")]
    TooLarge {
        /// The advertised size.
        len: u32,
        /// The cap that was exceeded.
        max: u32,
    },

    /// JSON decode failed.
    #[error("frame decode: {0}")]
    Decode(#[from] serde_json::Error),
}

/// Read exactly one length-prefixed JSON frame and decode it as `T`.
///
/// Returns `FrameError::Io` with `ErrorKind::UnexpectedEof` if the peer
/// closes mid-frame (including before the length prefix is complete).
pub async fn read_frame<R, T>(reader: &mut R) -> Result<T, FrameError>
where
    R: AsyncRead + Unpin,
    T: DeserializeOwned,
{
    let len = reader.read_u32().await?;
    if len > MAX_FRAME_BYTES {
        return Err(FrameError::TooLarge {
            len,
            max: MAX_FRAME_BYTES,
        });
    }
    let mut buf = vec![0u8; len as usize];
    reader.read_exact(&mut buf).await?;
    let value = serde_json::from_slice(&buf)?;
    Ok(value)
}

/// Encode `value` as JSON and write it as one length-prefixed frame.
///
/// The buffer is flushed before returning so a single round-trip of
/// `write_frame` then `read_frame` on the same socket cannot deadlock
/// on buffered output.
pub async fn write_frame<W, T>(writer: &mut W, value: &T) -> Result<(), FrameError>
where
    W: AsyncWrite + Unpin,
    T: Serialize,
{
    let bytes = serde_json::to_vec(value)?;
    let len: u32 = bytes.len().try_into().map_err(|_| FrameError::TooLarge {
        len: u32::MAX,
        max: MAX_FRAME_BYTES,
    })?;
    if len > MAX_FRAME_BYTES {
        return Err(FrameError::TooLarge {
            len,
            max: MAX_FRAME_BYTES,
        });
    }
    writer.write_u32(len).await?;
    writer.write_all(&bytes).await?;
    writer.flush().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{IpcError, IpcRequest, IpcResponse, PinActionKind, PinId, PinSummary};
    use tokio::io::duplex;

    async fn roundtrip_req(req: IpcRequest) {
        let (mut a, mut b) = duplex(8192);
        write_frame(&mut a, &req).await.expect("write");
        let got: IpcRequest = read_frame(&mut b).await.expect("read");
        assert_eq!(got, req);
    }

    async fn roundtrip_resp(resp: IpcResponse) {
        let (mut a, mut b) = duplex(8192);
        write_frame(&mut a, &resp).await.expect("write");
        let got: IpcResponse = read_frame(&mut b).await.expect("read");
        assert_eq!(got, resp);
    }

    #[tokio::test]
    async fn all_request_variants_roundtrip() {
        roundtrip_req(IpcRequest::Capture).await;
        roundtrip_req(IpcRequest::Clipboard).await;
        roundtrip_req(IpcRequest::List).await;
        roundtrip_req(IpcRequest::Close { id: PinId::new(3) }).await;
        roundtrip_req(IpcRequest::CloseAll).await;
        roundtrip_req(IpcRequest::Subscribe).await;
        roundtrip_req(IpcRequest::PinAction {
            id: PinId::new(3),
            action: PinActionKind::Copy,
        })
        .await;
    }

    #[tokio::test]
    async fn all_response_variants_roundtrip() {
        roundtrip_resp(IpcResponse::Ok).await;
        roundtrip_resp(IpcResponse::Pinned { id: PinId::new(7) }).await;
        roundtrip_resp(IpcResponse::Pins {
            pins: vec![PinSummary {
                id: PinId::new(1),
                width: 800,
                height: 600,
                created_at_unix_ms: 1,
                thumbnail: None,
                revision: 0,
            }],
        })
        .await;
        roundtrip_resp(IpcResponse::Error(IpcError::NotImplemented {
            feature: "capture".into(),
        }))
        .await;
    }

    #[tokio::test]
    async fn oversized_frame_is_rejected() {
        let (mut a, mut b) = duplex(16);
        // Manually write a length prefix that exceeds the cap.
        a.write_u32(MAX_FRAME_BYTES + 1).await.expect("write len");
        let err = read_frame::<_, IpcRequest>(&mut b)
            .await
            .expect_err("should reject");
        assert!(matches!(err, FrameError::TooLarge { .. }));
    }

    #[tokio::test]
    async fn truncated_frame_returns_io_eof() {
        let (mut a, mut b) = duplex(16);
        // Length prefix says 10 bytes, but we close after writing nothing.
        a.write_u32(10).await.expect("write len");
        drop(a);
        let err = read_frame::<_, IpcRequest>(&mut b)
            .await
            .expect_err("should fail");
        assert!(matches!(err, FrameError::Io(e) if e.kind() == std::io::ErrorKind::UnexpectedEof));
    }
}
