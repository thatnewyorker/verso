#![cfg(feature = "versoview-runtime")]
/*!
Transport abstraction for IPC with length-prefixed framing.

Goals:
- Provide a minimal, transport-agnostic trait for sending/receiving framed messages.
- Default to a simple u32 little-endian length-prefixed frame format.
- Supply a `StdIoTransport` skeleton that can be constructed over any AsyncRead/AsyncWrite pair,
  including the current process stdio for prototyping.

Notes:
- This module is feature-gated behind `versoview-runtime`.
- It depends on `tokio`'s async IO traits and uses a straightforward manual framing implementation
  to avoid additional dependencies.
*/

use bytes::Bytes;
use std::{fmt, future::Future, io, pin::Pin};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Default maximum frame size (64 MiB) to guard against OOM from malformed peers.
pub const DEFAULT_MAX_FRAME_SIZE: usize = 64 * 1024 * 1024;

/// A boxed, sendable future used by the transport trait.
pub type TransportFut<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Errors produced by transport I/O and framing.
#[derive(Debug)]
pub enum TransportError {
    Io(io::Error),
    /// The announced frame length exceeds the configured maximum.
    FrameTooLarge(usize),
    /// End-of-stream encountered unexpectedly during a read.
    UnexpectedEof,
    /// Transport does not support the requested capability (e.g., handle passing).
    Unsupported(&'static str),
}

impl fmt::Display for TransportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TransportError::Io(e) => write!(f, "io error: {e}"),
            TransportError::FrameTooLarge(n) => write!(f, "frame too large: {n} bytes"),
            TransportError::UnexpectedEof => write!(f, "unexpected end of stream"),
            TransportError::Unsupported(what) => write!(f, "unsupported: {what}"),
        }
    }
}

impl std::error::Error for TransportError {}

impl From<io::Error> for TransportError {
    fn from(e: io::Error) -> Self {
        TransportError::Io(e)
    }
}

/// An abstraction over a framed message transport.
///
/// Framing:
/// - Each message is sent as: [4-byte little-endian length][payload bytes].
/// - Implementations must ensure partial reads/writes are handled correctly.
///
/// Handle/FD passing:
/// - Default returns `false`; specific transports may override when implemented.
/// - When the `zero_copy` feature is enabled, an optional handle-passing API is exposed.
///   Implementations that support handle passing should override these methods accordingly.

#[cfg(feature = "zero_copy")]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct OpaqueHandle(pub u64);
pub trait IpcTransport: Send {
    /// Send a single frame (payload) over the transport.
    fn send_frame<'a>(&'a mut self, payload: Bytes)
    -> TransportFut<'a, Result<(), TransportError>>;

    /// Receive the next complete frame from the transport.
    fn next_frame<'a>(&'a mut self) -> TransportFut<'a, Result<Bytes, TransportError>>;

    /// Gracefully close the transport if applicable.
    fn close<'a>(&'a mut self) -> TransportFut<'a, Result<(), TransportError>>;

    /// Optional: send a frame with associated OS/resource handles (feature-gated).
    ///
    /// Default implementation falls back to `send_frame` and ignores handles.
    #[cfg(feature = "zero_copy")]
    fn send_frame_with_handles<'a>(
        &'a mut self,
        payload: Bytes,
        _handles: Vec<OpaqueHandle>,
    ) -> TransportFut<'a, Result<(), TransportError>> {
        self.send_frame(payload)
    }

    /// Optional: receive a frame with any associated OS/resource handles (feature-gated).
    ///
    /// Default implementation falls back to `next_frame` and returns no handles.
    #[cfg(feature = "zero_copy")]
    fn next_frame_with_handles<'a>(
        &'a mut self,
    ) -> TransportFut<'a, Result<(Bytes, Vec<OpaqueHandle>), TransportError>> {
        Box::pin(async move {
            let payload = self.next_frame().await?;
            Ok((payload, Vec::new()))
        })
    }

    /// Returns true if this transport supports ancillary handle passing.
    fn supports_handle_passing(&self) -> bool {
        false
    }
}

/// A length-prefixed framed transport over any AsyncRead/AsyncWrite pair.
///
/// This is suitable for:
/// - Stdio pipes (child stdin/stdout).
/// - Named pipes / domain sockets (when wrapped as AsyncRead/AsyncWrite).
///
/// It does not implement handle/FD passing; higher-level transports should be
/// introduced later for Unix domain sockets or platform-specific mechanisms.
pub struct StdIoTransport<R, W> {
    reader: R,
    writer: W,
    max_frame_size: usize,
}

impl<R, W> StdIoTransport<R, W>
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    /// Construct a new stdio-based transport from an AsyncRead/AsyncWrite pair.
    pub fn new(reader: R, writer: W) -> Self {
        Self {
            reader,
            writer,
            max_frame_size: DEFAULT_MAX_FRAME_SIZE,
        }
    }

    /// Set the maximum acceptable frame size. Returns self for chaining.
    pub fn with_max_frame_size(mut self, bytes: usize) -> Self {
        self.max_frame_size = bytes;
        self
    }

    /// Convenience constructor using the current process stdin/stdout.
    ///
    /// Useful for quick prototyping or when another process connects to this one's stdio.
    pub fn from_stdio() -> Self
    where
        R: From<tokio::io::Stdin>,
        W: From<tokio::io::Stdout>,
    {
        let r = tokio::io::stdin();
        let w = tokio::io::stdout();
        Self::new(r.into(), w.into())
    }

    async fn write_frame_inner(&mut self, payload: Bytes) -> Result<(), TransportError> {
        let len = payload.len();
        if len > self.max_frame_size {
            return Err(TransportError::FrameTooLarge(len));
        }
        let mut header = [0u8; 4];
        // u32 length, little-endian
        let len_u32 = u32::try_from(len).map_err(|_| TransportError::FrameTooLarge(len))?;
        header[0] = (len_u32 & 0xFF) as u8;
        header[1] = ((len_u32 >> 8) & 0xFF) as u8;
        header[2] = ((len_u32 >> 16) & 0xFF) as u8;
        header[3] = ((len_u32 >> 24) & 0xFF) as u8;

        self.writer.write_all(&header).await?;
        if len > 0 {
            self.writer.write_all(&payload).await?;
        }
        self.writer.flush().await?;
        Ok(())
    }

    async fn read_frame_inner(&mut self) -> Result<Bytes, TransportError> {
        // Read the 4-byte length prefix.
        let mut header = [0u8; 4];
        self.reader.read_exact(&mut header).await.map_err(|e| {
            if e.kind() == io::ErrorKind::UnexpectedEof {
                TransportError::UnexpectedEof
            } else {
                TransportError::Io(e)
            }
        })?;

        let len = u32::from_le_bytes(header) as usize;
        if len > self.max_frame_size {
            return Err(TransportError::FrameTooLarge(len));
        }

        let mut buf = vec![0u8; len];
        if len > 0 {
            self.reader.read_exact(&mut buf).await.map_err(|e| {
                if e.kind() == io::ErrorKind::UnexpectedEof {
                    TransportError::UnexpectedEof
                } else {
                    TransportError::Io(e)
                }
            })?;
        }
        Ok(Bytes::from(buf))
    }
}

impl<R, W> IpcTransport for StdIoTransport<R, W>
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    fn send_frame<'a>(
        &'a mut self,
        payload: Bytes,
    ) -> TransportFut<'a, Result<(), TransportError>> {
        Box::pin(async move { self.write_frame_inner(payload).await })
    }

    fn next_frame<'a>(&'a mut self) -> TransportFut<'a, Result<Bytes, TransportError>> {
        Box::pin(async move { self.read_frame_inner().await })
    }

    fn close<'a>(&'a mut self) -> TransportFut<'a, Result<(), TransportError>> {
        Box::pin(async move {
            // Try to gracefully shutdown writer; ignore "not connected" errors.
            if let Err(e) = self.writer.shutdown().await {
                // If the peer already closed, treat as success to simplify callers.
                if e.kind() != io::ErrorKind::NotConnected && e.kind() != io::ErrorKind::BrokenPipe
                {
                    return Err(TransportError::Io(e));
                }
            }
            Ok(())
        })
    }

    fn supports_handle_passing(&self) -> bool {
        false
    }
}

/// Helper to send a slice as a frame using any transport.
///
/// This is a convenience wrapper for quickly sending a payload without constructing Bytes.
pub fn send_slice<'a, T: IpcTransport + ?Sized>(
    transport: &'a mut T,
    payload: &'a [u8],
) -> TransportFut<'a, Result<(), TransportError>> {
    transport.send_frame(Bytes::copy_from_slice(payload))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::duplex;

    #[tokio::test]
    async fn roundtrip_small_frame() {
        let (a_rx, a_tx) = duplex(1024);
        let (b_rx, b_tx) = duplex(1024);

        // Connect A's writer to B's reader and vice versa to simulate a pipe pair.
        let mut a = StdIoTransport::new(a_rx, b_tx);
        let mut b = StdIoTransport::new(b_rx, a_tx);

        let payload = Bytes::from_static(b"hello world");
        a.send_frame(payload.clone()).await.unwrap();

        let recv = b.next_frame().await.unwrap();
        assert_eq!(&recv[..], b"hello world");

        a.close().await.unwrap();
        b.close().await.unwrap();
    }

    #[tokio::test]
    async fn reject_oversize_frame() {
        let (a_rx, a_tx) = duplex(1024);
        let (b_rx, b_tx) = duplex(1024);

        let mut a = StdIoTransport::new(a_rx, b_tx).with_max_frame_size(8);
        let mut b = StdIoTransport::new(b_rx, a_tx).with_max_frame_size(8);

        let payload = Bytes::from_static(b"0123456789"); // 10 bytes
        let err = a.send_frame(payload).await.unwrap_err();
        matches!(err, TransportError::FrameTooLarge(_));

        // Ensure receiver isn't confused if no frame was sent.
        // Send a small frame and confirm roundtrip works.
        let ok = Bytes::from_static(b"ok");
        a.send_frame(ok.clone()).await.unwrap();
        let recv = b.next_frame().await.unwrap();
        assert_eq!(&recv[..], b"ok");
    }
}
