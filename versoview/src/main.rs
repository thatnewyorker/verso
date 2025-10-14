use std::cmp;

use bytes::Bytes;

#[cfg(unix)]
use std::os::unix::io::AsRawFd;
use tracing::{error, info, warn};
use verso_standalone::ipc_protocol as proto;
use verso_standalone::ipc_transport::IpcTransport;
#[cfg(all(unix, feature = "zero_copy"))]
use verso_standalone::ipc_transport::OpaqueHandle;
use verso_standalone::ipc_transport::StdIoTransport;
#[cfg(all(unix, feature = "zero_copy"))]
use verso_standalone::transport_unix::UnixSocketTransport;

enum TransportMode {
    Stdio,
    #[allow(dead_code)]
    UnixSocket(String),
}

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    init_tracing();

    let mut args = std::env::args().skip(1);
    let mode = match args.next().as_deref() {
        Some("--unix-socket") => {
            if let Some(path) = args.next() {
                TransportMode::UnixSocket(path)
            } else {
                warn!("--unix-socket flag requires a path argument; falling back to stdio.");
                TransportMode::Stdio
            }
        }
        _ => TransportMode::Stdio,
    };

    if let Err(e) = run_server(mode).await {
        error!("versoview server error: {e}");
        // Best effort flush before exit.
        // Note: stdout is managed by the transport; nothing else to flush here.
        std::process::exit(1);
    }
}

fn init_tracing() {
    use tracing_subscriber::EnvFilter;
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .try_init();
}

fn serialize(env: &proto::Envelope) -> Result<Bytes, Box<dyn std::error::Error>> {
    let cfg = bincode::config::standard();
    let v = bincode::serde::encode_to_vec(env, cfg)?;
    Ok(Bytes::from(v))
}

fn supports_version(range: proto::VersionRange, v: u16) -> bool {
    v >= range.min && v <= range.max
}

async fn run_server(mode: TransportMode) -> Result<(), Box<dyn std::error::Error>> {
    // Build transport based on CLI
    #[allow(unused_mut)]
    let mut transport: Box<dyn IpcTransport> = match mode {
        TransportMode::Stdio => {
            Box::new(StdIoTransport::new(tokio::io::stdin(), tokio::io::stdout()))
        }
        #[cfg(all(unix, feature = "zero_copy"))]
        TransportMode::UnixSocket(path) => {
            info!(
                "versoview server starting (unix-socket transport: {})",
                path
            );
            Box::new(UnixSocketTransport::connect(path).await?)
        }
        #[cfg(not(all(unix, feature = "zero_copy")))]
        TransportMode::UnixSocket(path) => {
            warn!("unix-socket mode requested but not supported in this build. Path: {path}");
            Box::new(StdIoTransport::new(tokio::io::stdin(), tokio::io::stdout()))
        }
    };

    let server_name = "versoview";
    let mut next_id: u64 = 1;

    let mode_desc = if transport.supports_handle_passing() {
        "transport with handle-passing"
    } else {
        "stdio transport"
    };
    info!("versoview server started ({mode_desc})");

    loop {
        match transport.next_frame().await {
            Ok(bytes) => {
                // Decode envelope
                let env: proto::Envelope =
                    match bincode::serde::decode_from_slice(&bytes, bincode::config::standard()) {
                        Ok((m, _)) => m,
                        Err(e) => {
                            warn!("bad message: {e}");
                            continue;
                        }
                    };

                match env.message {
                    proto::Message::Request(req) => {
                        if let Err(e) = handle_request(
                            &mut *transport,
                            &mut next_id,
                            server_name,
                            env.version,
                            env.id,
                            req,
                        )
                        .await
                        {
                            warn!("failed to handle request: {e}");
                        }
                    }
                    proto::Message::Response(_resp) => {
                        warn!("unexpected Response from client; ignoring");
                    }
                    proto::Message::Event(_evt) => {
                        warn!("unexpected Event from client; ignoring");
                    }
                }
            }
            Err(e) => {
                warn!("transport read error or EOF: {e}");
                break;
            }
        }
    }

    // Attempt to gracefully close the writer.
    let _ = transport.close().await;
    info!("versoview server exiting");
    Ok(())
}

async fn handle_request(
    transport: &mut dyn IpcTransport,
    next_id: &mut u64,
    server_name: &str,
    peer_version: u16,
    req_id: u64,
    req: proto::Request,
) -> Result<(), Box<dyn std::error::Error>> {
    match req {
        proto::Request::Init {
            client_name,
            supported,
            config,
        } => {
            info!(
                "Init from '{client_name}', supported={}-{}, peer_version={peer_version}",
                supported.min, supported.max
            );
            // Choose a protocol version: min of peer message version and our max, within supported range.
            let chosen = cmp::min(peer_version, proto::PROTOCOL_MAX_VERSION);
            if !supports_version(supported, chosen) {
                let err = proto::Response::Error {
                    in_reply_to: req_id,
                    code: proto::ErrorCode::Unsupported,
                    message: format!(
                        "no compatible protocol version in range {}-{}",
                        supported.min, supported.max
                    ),
                };
                send_response(transport, next_id, err).await?;
                return Ok(());
            }

            // Log a couple of config hints (optional).
            if let Some(dir) = config.resources_dir.as_ref() {
                info!("resources_dir set to {}", dir.0);
            }
            if let Some(port) = config.devtools_port {
                info!("devtools_port requested: {port}");
            }

            let caps = proto::Capabilities {
                fd_passing: transport.supports_handle_passing(),
                zero_copy_frames: transport.supports_handle_passing(),
                compressed_frames: true,
                supported_compressed: vec![proto::CompressedFormat::Png],
                extra: vec![],
            };

            let init_ack = proto::Response::InitAck {
                in_reply_to: req_id,
                chosen_version: chosen,
                server_name: server_name.to_string(),
                capabilities: caps,
            };
            send_response(transport, next_id, init_ack).await?;
        }

        proto::Request::BindSurface { surface } => {
            info!("BindSurface: {:?}", surface);
            send_response(
                transport,
                next_id,
                proto::Response::Ack {
                    in_reply_to: req_id,
                },
            )
            .await?;
        }

        proto::Request::Load { url } => {
            info!("Load: {url}");
            send_response(
                transport,
                next_id,
                proto::Response::Ack {
                    in_reply_to: req_id,
                },
            )
            .await?;
            // Emit a simple console event as a proof-of-life.
            let evt = proto::Event::ConsoleMessage {
                level: proto::ConsoleLevel::Info,
                message: format!("Load requested: {url}"),
                source: Some("versoview".to_string()),
                line: None,
                column: None,
            };
            send_event(transport, next_id, evt).await?;
            // Optionally, a simple Navigation event
            let nav = proto::Event::Navigation {
                status: proto::NavigationStatus::Committed,
                url: Some(url),
                http_status: Some(200),
            };
            send_event(transport, next_id, nav).await?;
        }

        proto::Request::EvalScript { source, request_id } => {
            info!(
                "EvalScript (len={} chars), req_id={:?}",
                source.chars().count(),
                request_id
            );
            send_response(
                transport,
                next_id,
                proto::Response::Ack {
                    in_reply_to: req_id,
                },
            )
            .await?;
            // For debugging, emit a console event noting the request.
            let evt = proto::Event::ConsoleMessage {
                level: proto::ConsoleLevel::Debug,
                message: "EvalScript received".to_string(),
                source: Some("versoview".to_string()),
                line: None,
                column: None,
            };
            send_event(transport, next_id, evt).await?;
        }

        proto::Request::Resize { width, height } => {
            info!("Resize: {}x{}", width, height);
            send_response(
                transport,
                next_id,
                proto::Response::Ack {
                    in_reply_to: req_id,
                },
            )
            .await?;
        }

        proto::Request::RequestDraw => {
            info!("RequestDraw");
            // Acknowledge first.
            send_response(
                transport,
                next_id,
                proto::Response::Ack {
                    in_reply_to: req_id,
                },
            )
            .await?;
            // If handle passing is available, emit a demo FrameReady with a shared-memory handle.
            #[cfg(all(unix, feature = "zero_copy"))]
            if transport.supports_handle_passing() {
                use nix::sys::memfd::{MemfdCreateFlag, memfd_create};
                use std::ffi::CString;
                use std::io::Write;
                use std::os::unix::io::FromRawFd;

                // Create an anonymous shared memory file via memfd.
                let name = CString::new("versoview_frame").unwrap();
                let fd = memfd_create(&name, MemfdCreateFlag::empty())?;

                // SAFETY: we own the freshly created memfd `fd`.
                let mut file =
                    unsafe { <std::fs::File as std::os::unix::io::FromRawFd>::from_raw_fd(fd) };

                // Deterministic RGBA pattern: 64x64 gradient.
                let width: u32 = 64;
                let height: u32 = 64;
                let stride: u32 = width * 4;
                let size: usize = (stride as usize) * (height as usize);
                let mut bytes = vec![0u8; size];
                for y in 0..height as usize {
                    for x in 0..width as usize {
                        let idx = y * (stride as usize) + x * 4;
                        bytes[idx + 0] = x as u8; // R
                        bytes[idx + 1] = y as u8; // G
                        bytes[idx + 2] = 0x80; // B
                        bytes[idx + 3] = 0xFF; // A
                    }
                }

                // Ensure file is sized appropriately and write the payload.
                file.set_len(size as u64)?;
                file.write_all(&bytes)?;

                // Choose a token decoupled from the raw fd (monotonic relative to message id).
                let token = next_id.saturating_add(1);

                let evt = proto::Event::FrameReady {
                    frame_id: *next_id,
                    descriptor: proto::FrameDescriptor::SharedMemoryImage {
                        width,
                        height,
                        stride,
                        format: proto::PixelFormat::Rgba8888,
                        shm: proto::HandleToken(token),
                        size: size as u64,
                    },
                };
                // Attach the memfd and transfer ownership to the client; drop our local file afterwards.
                let fd_send = file.as_raw_fd();
                send_event_with_handles(
                    transport,
                    next_id,
                    evt,
                    vec![OpaqueHandle(fd_send as u64)],
                )
                .await?;
            } else {
                // Fallback console event.
                let evt = proto::Event::ConsoleMessage {
                    level: proto::ConsoleLevel::Info,
                    message: "Draw requested (no zero-copy transport)".to_string(),
                    source: Some("versoview".to_string()),
                    line: None,
                    column: None,
                };
                send_event(transport, next_id, evt).await?;
            }
        }

        proto::Request::DrawNow => {
            info!("DrawNow");
            send_response(
                transport,
                next_id,
                proto::Response::Ack {
                    in_reply_to: req_id,
                },
            )
            .await?;
        }

        proto::Request::InputEvent { events } => {
            info!("InputEvent: batch size={}", events.len());
            send_response(
                transport,
                next_id,
                proto::Response::Ack {
                    in_reply_to: req_id,
                },
            )
            .await?;
        }

        proto::Request::Shutdown => {
            info!("Shutdown requested");
            send_response(
                transport,
                next_id,
                proto::Response::Ack {
                    in_reply_to: req_id,
                },
            )
            .await?;
            // After ack, terminate the server loop by closing transport (outer loop handles break on EOF).
            // Immediate return ends request handler; caller loop will close on EOF or control-c.
            // We can't close from here directly; best-effort by returning and letting the main loop exit.
            // The client will typically close its side after receiving Ack.
        }
    }

    Ok(())
}

async fn send_response(
    transport: &mut dyn IpcTransport,
    next_id: &mut u64,
    resp: proto::Response,
) -> Result<(), Box<dyn std::error::Error>> {
    let env = proto::Envelope {
        version: proto::PROTOCOL_VERSION,
        id: take_id(next_id),
        flags: 0,
        message: proto::Message::Response(resp),
    };
    let bytes = serialize(&env)?;
    transport.send_frame(bytes).await?;
    Ok(())
}

async fn send_event(
    transport: &mut dyn IpcTransport,
    next_id: &mut u64,
    evt: proto::Event,
) -> Result<(), Box<dyn std::error::Error>> {
    let env = proto::Envelope {
        version: proto::PROTOCOL_VERSION,
        id: take_id(next_id),
        flags: 0,
        message: proto::Message::Event(evt),
    };
    let bytes = serialize(&env)?;
    transport.send_frame(bytes).await?;
    Ok(())
}

#[cfg(all(unix, feature = "zero_copy"))]
async fn send_event_with_handles(
    transport: &mut dyn IpcTransport,
    next_id: &mut u64,
    evt: proto::Event,
    handles: Vec<OpaqueHandle>,
) -> Result<(), Box<dyn std::error::Error>> {
    let env = proto::Envelope {
        version: proto::PROTOCOL_VERSION,
        id: take_id(next_id),
        flags: 0,
        message: proto::Message::Event(evt),
    };
    let bytes = serialize(&env)?;
    transport.send_frame_with_handles(bytes, handles).await?;
    Ok(())
}

fn take_id(next_id: &mut u64) -> u64 {
    let id = *next_id;
    *next_id = next_id.saturating_add(1);
    id
}
