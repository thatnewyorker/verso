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
use std::os::unix::io::AsRawFd;
use std::path::Path;

use bytes::Bytes;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::task;

use crate::ipc_transport::{DEFAULT_MAX_FRAME_SIZE, IpcTransport, TransportError, TransportFut};

#[cfg(feature = "zero_copy")]
use nix::sys::socket::{ControlMessage, ControlMessageOwned, MsgFlags, recvmsg, sendmsg};
#[cfg(feature = "zero_copy")]
use std::io::{IoSlice, IoSliceMut};
#[cfg(feature = "zero_copy")]
use std::os::unix::io::RawFd;

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
        // When built with zero_copy and SCM_RIGHTS support, this transport can pass handles.
        true
    }

    #[cfg(feature = "zero_copy")]
    fn send_frame_with_handles<'a>(
        &'a mut self,
        payload: Bytes,
        handles: Vec<crate::ipc_transport::OpaqueHandle>,
    ) -> TransportFut<'a, Result<(), TransportError>> {
        Box::pin(async move {
            // Build a single buffer: [len LE][payload]
            let len = payload.len();
            if len > self.max_frame_size {
                return Err(TransportError::FrameTooLarge(len));
            }
            let len_u32 = u32::try_from(len).map_err(|_| TransportError::FrameTooLarge(len))?;
            let mut buf = Vec::with_capacity(4 + len);
            buf.extend_from_slice(&len_u32.to_le_bytes());
            if len > 0 {
                buf.extend_from_slice(&payload);
            }

            // Convert OpaqueHandle -> RawFd
            let mut fds: Vec<RawFd> = handles.into_iter().map(|h| h.0 as RawFd).collect();
            // Send via sendmsg with SCM_RIGHTS in a blocking section.
            let fd = self.stream.as_raw_fd();
            let send_res = task::spawn_blocking(move || {
                let iov = [IoSlice::new(&buf)];
                let cmsgs: Vec<ControlMessage<'_>> = if fds.is_empty() {
                    Vec::new()
                } else {
                    vec![ControlMessage::ScmRights(&fds)]
                };
                sendmsg::<()>(fd, &iov, &cmsgs, MsgFlags::empty(), None)
            })
            .await
            .map_err(|e| {
                TransportError::Io(io::Error::new(
                    io::ErrorKind::Other,
                    format!("join error: {e}"),
                ))
            })?;

            match send_res {
                Ok(_n) => Ok(()),
                Err(e) => {
                    let err = io::Error::new(io::ErrorKind::Other, e.to_string());
                    Err(TransportError::Io(err))
                }
            }
        })
    }

    #[cfg(feature = "zero_copy")]
    fn next_frame_with_handles<'a>(
        &'a mut self,
    ) -> TransportFut<'a, Result<(Bytes, Vec<crate::ipc_transport::OpaqueHandle>), TransportError>>
    {
        Box::pin(async move {
            // Receive a single message (header+payload) and any attached FDs.
            // Use a large buffer; enforce frame size after parsing header.
            let fd = self.stream.as_raw_fd();
            // Receive a single message (header+payload) and any attached FDs using a blocking helper.
            // Build Send-friendly return data from the blocking section to satisfy spawn_blocking bounds.
            let max = self.max_frame_size;
            let recv_result = task::spawn_blocking(move || {
                // Allocate control space for up to 8 FDs and a buffer for header+payload (max + 4).
                let mut cmsg_buf = vec![0u8; nix::sys::socket::cmsg_space::<[RawFd; 8]>()];
                let mut buf = vec![0u8; max + 4];
                let mut iov = [IoSliceMut::new(&mut buf)];
                match recvmsg::<()>(fd, &mut iov, Some(&mut cmsg_buf), MsgFlags::empty()) {
                    Ok(m) => {
                        let n = m.bytes;
                        let mut fds: Vec<RawFd> = Vec::new();
                        for cmsg in m.cmsgs() {
                            if let ControlMessageOwned::ScmRights(fd_list) = cmsg {
                                for fd in fd_list {
                                    fds.push(fd);
                                }
                            }
                        }
                        Ok((n, buf, fds))
                    }
                    Err(e) => Err(e),
                }
            })
            .await
            .map_err(|e| {
                TransportError::Io(io::Error::new(
                    io::ErrorKind::Other,
                    format!("join error: {e}"),
                ))
            })?;

            let (n, buf, fd_list) = match recv_result {
                Ok(v) => v,
                Err(e) => {
                    let err = io::Error::new(io::ErrorKind::Other, e.to_string());
                    return Err(TransportError::Io(err));
                }
            };

            if n < 4 {
                return Err(TransportError::UnexpectedEof);
            }
            let header = &buf[..4];
            let frame_len =
                u32::from_le_bytes([header[0], header[1], header[2], header[3]]) as usize;
            if frame_len > self.max_frame_size {
                return Err(TransportError::FrameTooLarge(frame_len));
            }
            if n < 4 + frame_len {
                // Partial frame in a single read is unexpected for our frame format.
                return Err(TransportError::UnexpectedEof);
            }
            let payload = if frame_len == 0 {
                Bytes::new()
            } else {
                Bytes::copy_from_slice(&buf[4..4 + frame_len])
            };

            // Collect attached FDs into OpaqueHandle list.
            let mut handles = Vec::new();
            for fd in fd_list {
                handles.push(crate::ipc_transport::OpaqueHandle(fd as u64));
            }

            Ok((payload, handles))
        })
    }
}
