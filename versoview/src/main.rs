use std::cmp;

use bytes::Bytes;

use tracing::{error, info, warn};
use verso_standalone::ipc_protocol as proto;
use verso_standalone::ipc_transport::IpcTransport;
#[cfg(all(unix, feature = "zero_copy"))]
use verso_standalone::ipc_transport::OpaqueHandle;
use verso_standalone::ipc_transport::StdIoTransport;

mod engine;
mod servo_engine;
use crate::engine::{DemoEngine, Engine, EngineFrame, EngineInit};
use crate::servo_engine::ServoEngine;
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
    let engine_choice = std::env::var("VERSOVIEW_ENGINE").unwrap_or_else(|_| "servo".to_string());
    let mut engine: Box<dyn Engine> = if engine_choice.eq_ignore_ascii_case("demo") {
        Box::new(DemoEngine::new())
    } else {
        Box::new(ServoEngine::new())
    };
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
                            engine.as_mut(),
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
    engine: &mut dyn Engine,
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

            // Initialize the engine and intersect capabilities with the transport support.
            let prefer_zero_copy = transport.supports_handle_passing();
            let eng_caps = engine
                .init(EngineInit {
                    resources_dir: config
                        .resources_dir
                        .as_ref()
                        .map(|p| std::path::PathBuf::from(&p.0)),
                    devtools_port: config.devtools_port,
                    user_agent: config.user_agent.clone(),
                    init_scripts: config.init_scripts.clone(),
                    prefer_zero_copy,
                })
                .unwrap_or_default();

            let caps = proto::Capabilities {
                fd_passing: prefer_zero_copy && eng_caps.fd_passing,
                zero_copy_frames: prefer_zero_copy && eng_caps.zero_copy_frames,
                compressed_frames: eng_caps.compressed_frames,
                supported_compressed: eng_caps.supported_compressed.clone(),
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
            let _ = engine.bind_surface(&surface);
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
            let _ = engine.load(&url);
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
            let _ = engine.eval_script(&source);
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
            let _ = engine.resize(width, height);
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
            // Ask the engine for a frame and map it to a protocol descriptor.
            if let Ok(Some(frame)) = engine.request_draw() {
                // Choose a token for any out-of-band handle attachments.
                let token = next_id.saturating_add(1);
                // Map EngineFrame -> (FrameDescriptor, optional attached handles)
                #[allow(unused_mut)]
                #[cfg(all(unix, feature = "zero_copy"))]
                let mut handles: Vec<OpaqueHandle> = Vec::new();
                #[cfg(not(all(unix, feature = "zero_copy")))]
                let mut handles: Vec<()> = Vec::new();
                let descriptor = match frame {
                    EngineFrame::SharedMemoryInline {
                        width,
                        height,
                        stride,
                        format,
                        bytes,
                    } => {
                        // Use compressed fallback path with inline payload for portability.
                        proto::FrameDescriptor::CompressedImage {
                            width,
                            height,
                            format: proto::CompressedFormat::Png,
                            payload: proto::PayloadLocator::Inline(bytes),
                        }
                    }
                    #[cfg(all(unix, feature = "zero_copy"))]
                    EngineFrame::SharedMemoryHandle {
                        width,
                        height,
                        stride,
                        format,
                        fd,
                        size,
                    } => {
                        use std::os::unix::io::IntoRawFd;
                        let raw = fd.into_raw_fd() as u64;
                        handles.push(OpaqueHandle(raw));
                        proto::FrameDescriptor::SharedMemoryImage {
                            width,
                            height,
                            stride,
                            format,
                            shm: proto::HandleToken(token),
                            size,
                        }
                    }
                    #[cfg(all(unix, feature = "zero_copy"))]
                    EngineFrame::LinuxDmabuf {
                        width,
                        height,
                        fourcc,
                        modifier,
                        planes,
                        fence,
                    } => {
                        use std::os::unix::io::IntoRawFd;
                        for p in &planes {
                            handles.push(OpaqueHandle(p.fd.as_raw_fd() as u64));
                        }
                        if let Some(f) = fence.as_ref() {
                            handles.push(OpaqueHandle(f.as_raw_fd() as u64));
                        }
                        let planes_proto = planes
                            .iter()
                            .map(|p| proto::DmabufPlane {
                                fd: proto::HandleToken(token),
                                offset: p.offset,
                                stride: p.stride,
                                plane_index: p.plane_index,
                            })
                            .collect();
                        proto::FrameDescriptor::LinuxDmabuf {
                            width,
                            height,
                            fourcc,
                            modifier,
                            planes: planes_proto,
                            fence: Some(proto::HandleToken(token)),
                        }
                    }
                    #[cfg(target_os = "macos")]
                    EngineFrame::MacOsIoSurface {
                        width,
                        height,
                        pixel_format,
                        io_surface_id,
                    } => proto::FrameDescriptor::MacOsIoSurface {
                        width,
                        height,
                        pixel_format,
                        io_surface: proto::HandleToken(token),
                    },
                    #[cfg(target_os = "windows")]
                    EngineFrame::WindowsDxgiSharedHandle {
                        width,
                        height,
                        dxgi_format,
                        handle_value,
                    } => {
                        handles.push(OpaqueHandle(handle_value));
                        proto::FrameDescriptor::WindowsDxgiSharedHandle {
                            width,
                            height,
                            dxgi_format,
                            handle: proto::HandleToken(token),
                        }
                    }
                    EngineFrame::CompressedImage {
                        width,
                        height,
                        format,
                        data,
                    } => proto::FrameDescriptor::CompressedImage {
                        width,
                        height,
                        format,
                        payload: proto::PayloadLocator::Inline(data),
                    },
                };
                let evt = proto::Event::FrameReady {
                    frame_id: *next_id,
                    descriptor,
                };
                // Send with or without handles depending on platform/feature.
                #[cfg(all(unix, feature = "zero_copy"))]
                {
                    if transport.supports_handle_passing() && !handles.is_empty() {
                        send_event_with_handles(transport, next_id, evt, handles).await?;
                    } else {
                        send_event(transport, next_id, evt).await?;
                    }
                }
                #[cfg(not(all(unix, feature = "zero_copy")))]
                {
                    let _ = handles;
                    send_event(transport, next_id, evt).await?;
                }
            } else {
                // Fallback console event.
                let evt = proto::Event::ConsoleMessage {
                    level: proto::ConsoleLevel::Info,
                    message: "Draw requested (no frame produced)".to_string(),
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
            if let Ok(Some(frame)) = engine.draw_now() {
                let token = next_id.saturating_add(1);
                #[allow(unused_mut)]
                #[cfg(all(unix, feature = "zero_copy"))]
                let mut handles: Vec<OpaqueHandle> = Vec::new();
                #[cfg(not(all(unix, feature = "zero_copy")))]
                let mut handles: Vec<()> = Vec::new();
                let descriptor = match frame {
                    EngineFrame::SharedMemoryInline {
                        width,
                        height,
                        stride,
                        format,
                        bytes,
                    } => proto::FrameDescriptor::CompressedImage {
                        width,
                        height,
                        format: proto::CompressedFormat::Png,
                        payload: proto::PayloadLocator::Inline(bytes),
                    },
                    #[cfg(all(unix, feature = "zero_copy"))]
                    EngineFrame::SharedMemoryHandle {
                        width,
                        height,
                        stride,
                        format,
                        fd,
                        size,
                    } => {
                        use std::os::unix::io::IntoRawFd;
                        let raw = fd.into_raw_fd() as u64;
                        handles.push(OpaqueHandle(raw));
                        proto::FrameDescriptor::SharedMemoryImage {
                            width,
                            height,
                            stride,
                            format,
                            shm: proto::HandleToken(token),
                            size,
                        }
                    }
                    #[cfg(all(unix, feature = "zero_copy"))]
                    EngineFrame::LinuxDmabuf {
                        width,
                        height,
                        fourcc,
                        modifier,
                        planes,
                        fence,
                    } => {
                        use std::os::unix::io::AsRawFd;
                        for p in &planes {
                            handles.push(OpaqueHandle(p.fd.as_raw_fd() as u64));
                        }
                        if let Some(f) = fence.as_ref() {
                            handles.push(OpaqueHandle(f.as_raw_fd() as u64));
                        }
                        let planes_proto = planes
                            .iter()
                            .map(|p| proto::DmabufPlane {
                                fd: proto::HandleToken(token),
                                offset: p.offset,
                                stride: p.stride,
                                plane_index: p.plane_index,
                            })
                            .collect();
                        proto::FrameDescriptor::LinuxDmabuf {
                            width,
                            height,
                            fourcc,
                            modifier,
                            planes: planes_proto,
                            fence: Some(proto::HandleToken(token)),
                        }
                    }
                    #[cfg(target_os = "macos")]
                    EngineFrame::MacOsIoSurface {
                        width,
                        height,
                        pixel_format,
                        io_surface_id: _,
                    } => proto::FrameDescriptor::MacOsIoSurface {
                        width,
                        height,
                        pixel_format,
                        io_surface: proto::HandleToken(token),
                    },
                    #[cfg(target_os = "windows")]
                    EngineFrame::WindowsDxgiSharedHandle {
                        width,
                        height,
                        dxgi_format,
                        handle_value,
                    } => {
                        handles.push(OpaqueHandle(handle_value));
                        proto::FrameDescriptor::WindowsDxgiSharedHandle {
                            width,
                            height,
                            dxgi_format,
                            handle: proto::HandleToken(token),
                        }
                    }
                    EngineFrame::CompressedImage {
                        width,
                        height,
                        format,
                        data,
                    } => proto::FrameDescriptor::CompressedImage {
                        width,
                        height,
                        format,
                        payload: proto::PayloadLocator::Inline(data),
                    },
                };
                let evt = proto::Event::FrameReady {
                    frame_id: *next_id,
                    descriptor,
                };
                #[cfg(all(unix, feature = "zero_copy"))]
                {
                    if transport.supports_handle_passing() && !handles.is_empty() {
                        send_event_with_handles(transport, next_id, evt, handles).await?;
                    } else {
                        send_event(transport, next_id, evt).await?;
                    }
                }
                #[cfg(not(all(unix, feature = "zero_copy")))]
                {
                    let _ = handles;
                    send_event(transport, next_id, evt).await?;
                }
            }
        }

        proto::Request::InputEvent { events } => {
            info!("InputEvent: batch size={}", events.len());
            let _ = engine.input_events(&events);
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
