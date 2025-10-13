#![cfg(all(unix, feature = "zero_copy"))]
/*!
Unix domain socket transport for Verso IPC (feature-gated).

This module introduces a transport that implements the `IpcTransport` trait over
`tokio::net::UnixStream`. It uses the same length-prefixed framing as other
transports in this crate:

- [4-byte little-endian length][payload bytes]

Notes:
- This initial implementation does not attach or receive OS/native handles. It
  focuses on the framed byte transport over Unix domain sockets. The optional
  handle-passing API exposed by `IpcTransport` (behind the `zero_copy` feature)
  falls back to the default no-handle behavior.
- Future iterations can extend this transport to support SCM_RIGHTS for
  ancillary file descriptor passing.
*/

use std::io;
use std::path::Path;

use bytes::Bytes;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

use crate::ipc_transport::{DEFAULT_MAX_FRAME_SIZE, IpcTransport, TransportError, TransportFut};

/// A length-prefixed framed transport over a Unix domain socket stream.
///
/// This transport is suitable when the client and server can connect via a Unix
/// domain socket for local IPC. It currently implements framed I/O only; handle
/// passing (e.g., SCM_RIGHTS) can be added in a follow-up iteration.
///
/// Framing:
/// - Each frame: [u32 little-endian length][payload bytes]
pub struct UnixSocketTransport {
    stream: UnixStream,
    max_frame_size: usize,
}

impl UnixSocketTransport {
    /// Establish a new connection to a Unix domain socket at the given path.
    ///
    /// Returns a framed transport that reads/writes length-prefixed payloads.
    pub async fn connect(path: impl AsRef<Path>) -> io::Result<Self> {
        let stream = UnixStream::connect(path).await?;
        Ok(Self::from_stream(stream))
    }

    /// Wrap an existing UnixStream with a framed transport.
    pub fn from_stream(stream: UnixStream) -> Self {
        Self {
            stream,
            max_frame_size: DEFAULT_MAX_FRAME_SIZE,
        }
    }

    /// Set the maximum acceptable frame size (for guarding against OOM).
    pub fn with_max_frame_size(mut self, bytes: usize) -> Self {
        self.max_frame_size = bytes;
        self
    }

    async fn write_frame_inner(&mut self, payload: Bytes) -> Result<(), TransportError> {
        let len = payload.len();
        if len > self.max_frame_size {
            return Err(TransportError::FrameTooLarge(len));
        }

        let len_u32 = u32::try_from(len).map_err(|_| TransportError::FrameTooLarge(len))?;
        let header = len_u32.to_le_bytes();

        self.stream.write_all(&header).await?;
        if len > 0 {
            self.stream.write_all(&payload).await?;
        }
        self.stream.flush().await?;
        Ok(())
    }

    async fn read_frame_inner(&mut self) -> Result<Bytes, TransportError> {
        let mut header = [0u8; 4];
        self.stream.read_exact(&mut header).await.map_err(|e| {
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
            self.stream.read_exact(&mut buf).await.map_err(|e| {
                if e.kind() == io::ErrorKind::UnexpectedEof {
                    TransportError::UnexpectedEof
                } else {
                    TransportError::Io(e)
                }
            })?;
        }
        Ok(Bytes::from(buf))
    }

    async fn close_inner(&mut self) -> Result<(), TransportError> {
        // Attempt a graceful shutdown; ignore common "not connected" errors.
        if let Err(e) = self.stream.shutdown().await {
            if e.kind() != io::ErrorKind::NotConnected && e.kind() != io::ErrorKind::BrokenPipe {
                return Err(TransportError::Io(e));
            }
        }
        Ok(())
    }
}

impl IpcTransport for UnixSocketTransport {
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
        Box::pin(async move { self.close_inner().await })
    }

    fn supports_handle_passing(&self) -> bool {
        // Future: return true when SCM_RIGHTS support is implemented.
        false
    }
}
