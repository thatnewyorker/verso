#![cfg(all(unix, feature = "zero_copy"))]
//! Zero-copy memfd integration test over Unix socket.
//!
//! This test spins up a UnixListener, launches the `versoview` server in
//! `--unix-socket <path>` mode (where the server actively connects to the
//! listener), performs the protocol handshake, requests a draw, and asserts:
//! - the server emits an Event::FrameReady with a SharedMemoryImage descriptor,
//! - the descriptor carries a non-zero HandleToken and correct geometry,
//! - at least one file descriptor is attached via SCM_RIGHTS.

use memmap2::MmapOptions;
use std::env;
use std::fs;
use std::io::{Read, Write};
use std::os::unix::io::{AsRawFd, FromRawFd, RawFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;

use bincode;
use nix::sys::socket::{ControlMessageOwned, MsgFlags, cmsg_space, recvmsg};
use nix::sys::uio::IoSliceMut;
use verso_standalone::ipc_protocol as proto;

fn serialize(env: &proto::Envelope) -> Vec<u8> {
    bincode::serde::encode_to_vec(env, bincode::config::standard()).expect("serialize envelope")
}

fn deserialize(buf: &[u8]) -> proto::Envelope {
    let (env, _): (proto::Envelope, _) =
        bincode::serde::decode_from_slice(buf, bincode::config::standard())
            .expect("deserialize envelope");
    env
}

fn write_frame(stream: &mut UnixStream, payload: &[u8]) {
    let len = u32::try_from(payload.len()).expect("frame too large");
    let header = len.to_le_bytes();
    stream.write_all(&header).expect("write header");
    if !payload.is_empty() {
        stream.write_all(payload).expect("write payload");
    }
    stream.flush().expect("flush");
}

fn read_exact(reader: &mut UnixStream, buf: &mut [u8]) -> std::io::Result<()> {
    let mut read = 0;
    while read < buf.len() {
        let n = reader.read(&mut buf[read..])?;
        if n == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "eof",
            ));
        }
        read += n;
    }
    Ok(())
}

fn read_frame_blocking(stream: &mut UnixStream) -> std::io::Result<Vec<u8>> {
    let mut header = [0u8; 4];
    read_exact(stream, &mut header)?;
    let len = u32::from_le_bytes(header) as usize;
    let mut buf = vec![0u8; len];
    if len > 0 {
        read_exact(stream, &mut buf)?;
    }
    Ok(buf)
}

/// Receive a single message using recvmsg and collect any attached FDs.
/// Returns the raw frame bytes (header+payload already stripped to payload) and a list of FDs.
/// Note: For this test, we assume the server sends header+payload in a single sendmsg;
/// small frames make this very likely. Production code should implement a proper framing loop.
fn recv_one_frame_with_fds(fd: RawFd, max_frame: usize) -> std::io::Result<(Vec<u8>, Vec<RawFd>)> {
    // Allocate a large buffer: 4 bytes header + max_frame payload.
    let mut buf = vec![0u8; 4 + max_frame];
    let mut iov = [IoSliceMut::new(&mut buf)];
    // Reserve control message space for up to 8 FDs.
    let mut cspace = cmsg_space::<[RawFd; 8]>();
    let msg = recvmsg::<()>(fd, &mut iov, Some(&mut cspace), MsgFlags::empty())
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, format!("recvmsg: {e}")))?;
    let n = msg.bytes;
    if n < 4 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            "short read (no header)",
        ));
    }
    let header = &buf[..4];
    let frame_len = u32::from_le_bytes([header[0], header[1], header[2], header[3]]) as usize;
    if frame_len > max_frame {
        return Err(std::io::Error::new(
            std::io::ErrorKind::Other,
            format!("frame too large: {frame_len} > {max_frame}"),
        ));
    }
    if n < 4 + frame_len {
        return Err(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            format!("partial frame: got {n}, need {}", 4 + frame_len),
        ));
    }
    // Extract payload and collect FDs.
    let payload = if frame_len == 0 {
        Vec::new()
    } else {
        buf[4..4 + frame_len].to_vec()
    };
    let mut fds: Vec<RawFd> = Vec::new();
    for c in msg.cmsgs() {
        if let ControlMessageOwned::ScmRights(rights) = c {
            for fd in rights {
                fds.push(fd);
            }
        }
    }
    Ok((payload, fds))
}

fn find_versoview_binary() -> String {
    if let Ok(bin) = env::var("CARGO_BIN_EXE_versoview") {
        return bin;
    }
    // Fallback: search near the test binary
    let exe = env::current_exe().expect("current_exe");
    let exe_dir = exe.parent().expect("exe parent");
    let candidates = [
        exe_dir.join("versoview"),
        exe_dir.join("versoview.exe"),
        exe_dir.join("../versoview"),
        exe_dir.join("../versoview.exe"),
        exe_dir.join("../../versoview"),
        exe_dir.join("../../versoview.exe"),
        exe_dir.join("../../../versoview"),
        exe_dir.join("../../../versoview.exe"),
    ];
    for c in candidates {
        if c.exists() {
            return c.canonicalize().unwrap().to_string_lossy().to_string();
        }
    }
    panic!(
        "versoview binary not found: set CARGO_BIN_EXE_versoview or ensure it's next to the test binary"
    );
}

fn next_id(id: &mut u64) -> u64 {
    let cur = *id;
    *id = id.saturating_add(1);
    cur
}

#[test]
fn zero_copy_memfd_unix_socket_frame_ready_with_fd() {
    // Prepare unique socket path
    let sock_path = {
        let mut p = env::temp_dir();
        p.push(format!(
            "versoview_zero_copy_test_{}.sock",
            std::process::id()
        ));
        // Ensure clean start
        let _ = fs::remove_file(&p);
        p
    };

    // Create listener for the server to connect to.
    let listener = UnixListener::bind(&sock_path).expect("bind unix listener");

    // Spawn the server in unix-socket mode (it will connect to our listener).
    let bin = find_versoview_binary();
    let mut child = Command::new(bin)
        .arg("--unix-socket")
        .arg(&sock_path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("spawn versoview");

    // Accept the incoming connection from the server.
    listener
        .set_nonblocking(false)
        .expect("set blocking listener");
    let (mut conn, _addr) = listener.accept().expect("accept connection");
    // After accept, we won't accept any more connections; drop listener and remove socket file.
    drop(listener);
    let _ = fs::remove_file(&sock_path);

    let mut msg_id = 1u64;

    // Step 1: Init -> InitAck
    let init = proto::Envelope {
        version: proto::PROTOCOL_VERSION,
        id: next_id(&mut msg_id),
        flags: 0,
        message: proto::Message::Request(proto::Request::Init {
            client_name: "zero_copy_memfd_test".to_string(),
            supported: proto::VersionRange {
                min: proto::PROTOCOL_MIN_VERSION,
                max: proto::PROTOCOL_MAX_VERSION,
            },
            config: proto::ControllerConfig {
                resources_dir: None,
                devtools_port: Some(0),
                user_agent: Some("verso-integration-test".to_string()),
                init_scripts: vec![],
                ipc_tuning: None,
            },
        }),
    };
    write_frame(&mut conn, &serialize(&init));

    let resp = read_frame_blocking(&mut conn).expect("read InitAck");
    match deserialize(&resp).message {
        proto::Message::Response(proto::Response::InitAck {
            in_reply_to,
            capabilities,
            ..
        }) => {
            assert_eq!(in_reply_to, init.id, "InitAck correlation id");
            assert!(
                capabilities.fd_passing,
                "fd_passing should be true for unix-socket transport"
            );
            assert!(
                capabilities.zero_copy_frames,
                "zero_copy_frames should be true"
            );
        }
        other => panic!("unexpected response to Init: {:?}", other),
    }

    // Step 2: BindSurface -> Ack
    let bind = proto::Envelope {
        version: proto::PROTOCOL_VERSION,
        id: next_id(&mut msg_id),
        flags: 0,
        message: proto::Message::Request(proto::Request::BindSurface {
            surface: proto::SurfaceDescriptor::Fallback {
                width: 64,
                height: 64,
                scale_factor: 1.0,
            },
        }),
    };
    write_frame(&mut conn, &serialize(&bind));
    let resp = read_frame_blocking(&mut conn).expect("read BindSurface Ack");
    match deserialize(&resp).message {
        proto::Message::Response(proto::Response::Ack { in_reply_to }) => {
            assert_eq!(in_reply_to, bind.id, "BindSurface Ack correlation id");
        }
        other => panic!("unexpected response to BindSurface: {:?}", other),
    }

    // Step 3: RequestDraw -> Ack
    let req_draw = proto::Envelope {
        version: proto::PROTOCOL_VERSION,
        id: next_id(&mut msg_id),
        flags: 0,
        message: proto::Message::Request(proto::Request::RequestDraw),
    };
    write_frame(&mut conn, &serialize(&req_draw));
    let resp = read_frame_blocking(&mut conn).expect("read RequestDraw Ack");
    match deserialize(&resp).message {
        proto::Message::Response(proto::Response::Ack { in_reply_to }) => {
            assert_eq!(in_reply_to, req_draw.id, "RequestDraw Ack correlation id");
        }
        other => panic!("unexpected response to RequestDraw: {:?}", other),
    }

    // Step 4: Expect a FrameReady event with a SharedMemoryImage descriptor and an attached FD.
    let (evt_payload, fds) =
        recv_one_frame_with_fds(conn.as_raw_fd(), 1024 * 1024).expect("recv event with fds");
    let env = deserialize(&evt_payload);
    match env.message {
        proto::Message::Event(proto::Event::FrameReady {
            frame_id: _,
            descriptor,
        }) => match descriptor {
            proto::FrameDescriptor::SharedMemoryImage {
                width,
                height,
                stride,
                format,
                shm,
                size,
            } => {
                assert_eq!(width, 64);
                assert_eq!(height, 64);
                assert_eq!(stride, 64 * 4);
                assert_eq!(format, proto::PixelFormat::Rgba8888);
                assert!(shm.0 > 0, "HandleToken must be non-zero");
                assert_eq!(size, (stride as u64) * (height as u64));
                assert!(
                    !fds.is_empty(),
                    "expected at least one FD attached via SCM_RIGHTS"
                );
                // Mmap the received memfd and verify expected RGBA gradient pattern.
                let fd0 = fds[0];
                // SAFETY: fd0 is a valid file descriptor received via SCM_RIGHTS; we take ownership here.
                let file = unsafe { std::fs::File::from_raw_fd(fd0) };
                let map = unsafe { MmapOptions::new().len(size as usize).map(&file) }
                    .expect("mmap memfd");
                let stride_usize = stride as usize;
                for y in 0..(height as usize) {
                    for x in 0..(width as usize) {
                        let idx = y * stride_usize + x * 4;
                        assert_eq!(map[idx], x as u8, "R at ({x},{y})");
                        assert_eq!(map[idx + 1], y as u8, "G at ({x},{y})");
                        assert_eq!(map[idx + 2], 0x80, "B at ({x},{y})");
                        assert_eq!(map[idx + 3], 0xFF, "A at ({x},{y})");
                    }
                }
                // Close any extra FDs to avoid leaks; fd0 will be closed when `file` is dropped.
                for &extra_fd in &fds[1..] {
                    let _ = nix::unistd::close(extra_fd);
                }
            }
            other => panic!(
                "unexpected FrameDescriptor (expected SharedMemoryImage): {:?}",
                other
            ),
        },
        other => panic!(
            "unexpected message (expected FrameReady event): {:?}",
            other
        ),
    }

    // FDs already closed after mmap verification.

    // Step 5: Shutdown -> Ack
    let shutdown = proto::Envelope {
        version: proto::PROTOCOL_VERSION,
        id: next_id(&mut msg_id),
        flags: 0,
        message: proto::Message::Request(proto::Request::Shutdown),
    };
    write_frame(&mut conn, &serialize(&shutdown));
    let resp = read_frame_blocking(&mut conn).expect("read Shutdown Ack");
    match deserialize(&resp).message {
        proto::Message::Response(proto::Response::Ack { in_reply_to }) => {
            assert_eq!(in_reply_to, shutdown.id, "Shutdown Ack correlation id");
        }
        other => panic!("unexpected response to Shutdown: {:?}", other),
    }

    // Cleanup child
    let _ = child.kill();
    let _ = child.wait();
}

#[cfg(not(all(unix, feature = "zero_copy")))]
#[test]
fn zero_copy_memfd_unix_socket_frame_ready_with_fd_skipped() {
    eprintln!("skipped: requires unix + zero_copy feature");
}
