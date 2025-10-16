use std::cmp;

use std::path::PathBuf;

use bytes::Bytes;
use clap::Parser;

use photon_standalone::ipc_protocol as proto;
use photon_standalone::ipc_transport::IpcTransport;
#[cfg(all(unix, feature = "zero_copy"))]
use photon_standalone::ipc_transport::OpaqueHandle;
use photon_standalone::ipc_transport::StdIoTransport;
#[cfg(any(feature = "tauri_ipc", feature = "tauri_compat_ipc"))]
use std::sync::{Mutex, OnceLock};
#[cfg(all(feature = "tauri_compat_ipc", not(feature = "tauri_ipc")))]
use tauri_compat_ipc::{ChannelAdapter, CommandRegistry, InvokeRequest, InvokeResponse};
#[cfg(feature = "tauri_ipc")]
use tauri_ipc_adapter::{ChannelAdapter, CommandRegistry, InvokeRequest, InvokeResponse};
use tracing::{error, info, warn};

#[cfg(any(feature = "tauri_ipc", feature = "tauri_compat_ipc"))]
static TAURI_CHANNEL: OnceLock<ChannelAdapter> = OnceLock::new();
#[cfg(any(feature = "tauri_ipc", feature = "tauri_compat_ipc"))]
static TAURI_REGISTRY: OnceLock<Mutex<CommandRegistry>> = OnceLock::new();

#[cfg(feature = "dioxus_engine")]
mod dioxus_engine;
mod engine;
mod servo_engine;

#[cfg(feature = "dioxus_engine")]
use crate::dioxus_engine::DioxusEngine;
use crate::engine::{DemoEngine, Engine, EngineFrame, EngineInit};
use crate::servo_engine::ServoEngine;
#[cfg(all(unix, feature = "zero_copy"))]
use photon_standalone::transport_unix::UnixSocketTransport;

enum TransportMode {
    Stdio,
    #[allow(dead_code)]
    UnixSocket(String),
}

#[derive(Debug, Parser, Clone)]
#[command(
    name = "photon",
    about = "Out-of-process Photon server that speaks the Verso IPC protocol over framed stdio"
)]
struct Args {
    /// Use a Unix-domain socket transport instead of stdio (Unix-only builds with zero-copy).
    #[arg(long = "unix-socket")]
    unix_socket: Option<String>,

    /// Absolute path to the Servo binary to use. Overrides env-based or pointer-file discovery.
    #[arg(long = "servo-bin")]
    servo_bin: Option<PathBuf>,
}

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    init_tracing();

    let args = Args::parse();
    let mode = match args.unix_socket.as_ref() {
        Some(path) => TransportMode::UnixSocket(path.clone()),
        None => TransportMode::Stdio,
    };

    // Discover Servo binary path: --servo-bin > PHOTON_SERVO_PATH > PHOTON_SERVO_POINTER > local pointer scan
    let cli_servo_bin: Option<PathBuf> = args.servo_bin.clone();
    let resolved_servo = (|| {
        if let Some(p) = cli_servo_bin.clone() {
            if p.exists() {
                return Some(p);
            }
        }
        if let Ok(envp) = std::env::var("PHOTON_SERVO_PATH") {
            let p = PathBuf::from(envp);
            if p.exists() {
                return Some(p);
            }
        }
        if let Ok(ptr) = std::env::var("PHOTON_SERVO_POINTER") {
            let pointer_path = PathBuf::from(ptr);
            if pointer_path.exists() {
                if let Ok(s) = std::fs::read_to_string(&pointer_path) {
                    let p = PathBuf::from(s.trim());
                    if p.exists() {
                        return Some(p);
                    }
                }
            }
        }
        let base = std::env::current_dir()
            .ok()?
            .join("third_party/servo-binaries/local");
        if base.is_dir() {
            if let Ok(targets) = std::fs::read_dir(&base) {
                for t in targets.flatten() {
                    let tpath = t.path();
                    if tpath.is_dir() {
                        if let Ok(profiles) = std::fs::read_dir(&tpath) {
                            for pr in profiles.flatten() {
                                let prpath = pr.path();
                                if prpath.is_dir() {
                                    let cur_ptr = prpath.join("current").join("servo_path.txt");
                                    if cur_ptr.exists() {
                                        if let Ok(s) = std::fs::read_to_string(&cur_ptr) {
                                            let p = PathBuf::from(s.trim());
                                            if p.exists() {
                                                return Some(p);
                                            }
                                        }
                                    }
                                    let latest = prpath.join("latest.json");
                                    if latest.exists() {
                                        if let Ok(s) = std::fs::read_to_string(&latest) {
                                            if let Ok(v) =
                                                serde_json::from_str::<serde_json::Value>(&s)
                                            {
                                                if let Some(commit) =
                                                    v.get("current").and_then(|x| x.as_str())
                                                {
                                                    let ptr =
                                                        prpath.join(commit).join("servo_path.txt");
                                                    if ptr.exists() {
                                                        if let Ok(s2) =
                                                            std::fs::read_to_string(&ptr)
                                                        {
                                                            let p = PathBuf::from(s2.trim());
                                                            if p.exists() {
                                                                return Some(p);
                                                            }
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        None
    })();

    if let Some(bin) = resolved_servo {
        let val = bin.display().to_string();

        info!("Using Servo binary: {}", val);
    } else {
        info!(
            "No Servo binary discovered (you can pass --servo-bin PATH or set PHOTON_SERVO_PATH)"
        );
    }

    if let Err(e) = run_server(mode).await {
        error!("Photon server error: {e}");
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
            info!("Photon server starting (unix-socket transport: {})", path);
            Box::new(UnixSocketTransport::connect(path).await?)
        }
        #[cfg(not(all(unix, feature = "zero_copy")))]
        TransportMode::UnixSocket(path) => {
            warn!("unix-socket mode requested but not supported in this build. Path: {path}");
            Box::new(StdIoTransport::new(tokio::io::stdin(), tokio::io::stdout()))
        }
    };

    let server_name = "photon";
    let engine_choice = std::env::var("PHOTON_ENGINE").unwrap_or_else(|_| "servo".to_string());
    let mut engine: Box<dyn Engine> = if engine_choice.eq_ignore_ascii_case("demo") {
        Box::new(DemoEngine::new())
    } else if engine_choice.eq_ignore_ascii_case("dioxus") {
        #[cfg(feature = "dioxus_engine")]
        {
            Box::new(DioxusEngine::new())
        }
        #[cfg(not(feature = "dioxus_engine"))]
        {
            warn!(
                "PHOTON_ENGINE=dioxus requested but 'dioxus_engine' feature is not enabled; falling back to servo"
            );
            Box::new(ServoEngine::new())
        }
    } else {
        Box::new(ServoEngine::new())
    };
    let mut next_id: u64 = 1;

    let mode_desc = if transport.supports_handle_passing() {
        "transport with handle-passing"
    } else {
        "stdio transport"
    };
    #[cfg(any(feature = "tauri_ipc", feature = "tauri_compat_ipc"))]
    {
        let _ = TAURI_CHANNEL.get_or_init(|| ChannelAdapter::new());
        let _ = TAURI_REGISTRY.get_or_init(|| {
            let mut reg = CommandRegistry::new();
            // Default sample command: echoes the JSON payload back to the caller.
            reg.register("hello", Box::new(|payload| Ok(payload)));
            Mutex::new(reg)
        });
    }
    info!("Photon server started ({mode_desc})");

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
    info!("Photon server exiting");
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
                source: Some("photon".to_string()),
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
                source: Some("photon".to_string()),
                line: None,
                column: None,
            };
            send_event(transport, next_id, evt).await?;
        }

        proto::Request::Invoke {
            command,
            payload,
            request_id,
        } => {
            #[cfg(any(feature = "tauri_ipc", feature = "tauri_compat_ipc"))]
            {
                let id = request_id.unwrap_or(req_id);
                let json_payload: serde_json::Value = if payload.is_empty() {
                    serde_json::Value::Null
                } else {
                    serde_json::from_slice(&payload).unwrap_or(serde_json::Value::Null)
                };
                let req_msg = InvokeRequest {
                    command,
                    payload: json_payload,
                    id,
                };
                let registry = TAURI_REGISTRY
                    .get()
                    .expect("tauri registry not initialized")
                    .lock()
                    .expect("tauri registry poisoned");
                let adapter = TAURI_CHANNEL.get().expect("tauri channel not initialized");
                let resp_msg = adapter.invoke(&registry, req_msg);
                let (ok, data, error) = match (resp_msg.ok, resp_msg.err) {
                    (Some(v), _) => (true, serde_json::to_vec(&v).ok(), None),
                    (None, Some(e)) => (false, None, Some(e)),
                    _ => (true, None, None),
                };
                let resp = proto::Response::InvokeResult {
                    in_reply_to: id,
                    ok,
                    data,
                    error,
                };
                send_response(transport, next_id, resp).await?;
            }
            #[cfg(not(any(feature = "tauri_ipc", feature = "tauri_compat_ipc")))]
            {
                let _ = (&command, &payload, &request_id);
                let resp = proto::Response::InvokeResult {
                    in_reply_to: req_id,
                    ok: false,
                    data: None,
                    error: Some("invoke not supported (tauri_ipc feature disabled)".to_string()),
                };
                send_response(transport, next_id, resp).await?;
            }
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
                let _token = next_id.saturating_add(1);
                // Map EngineFrame -> (FrameDescriptor, optional attached handles)
                #[allow(unused_mut)]
                #[cfg(all(unix, feature = "zero_copy"))]
                let mut handles: Vec<OpaqueHandle> = Vec::new();
                #[cfg(not(all(unix, feature = "zero_copy")))]
                let handles: Vec<()> = Vec::new();
                let descriptor = match frame {
                    EngineFrame::SharedMemoryInline {
                        width,
                        height,
                        stride: _,
                        format: _,
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
                    source: Some("photon".to_string()),
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
                let _token = next_id.saturating_add(1);
                #[allow(unused_mut)]
                #[cfg(all(unix, feature = "zero_copy"))]
                let mut handles: Vec<OpaqueHandle> = Vec::new();
                #[cfg(not(all(unix, feature = "zero_copy")))]
                let handles: Vec<()> = Vec::new();
                let descriptor = match frame {
                    EngineFrame::SharedMemoryInline {
                        width,
                        height,
                        stride: _,
                        format: _,
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

#[cfg(any(feature = "tauri_ipc", feature = "tauri_compat_ipc"))]
fn status_to_str(s: proto::NavigationStatus) -> &'static str {
    match s {
        proto::NavigationStatus::Started => "started",
        proto::NavigationStatus::Committed => "committed",
        proto::NavigationStatus::Finished => "finished",
        proto::NavigationStatus::Failed => "failed",
    }
}

#[cfg(any(feature = "tauri_ipc", feature = "tauri_compat_ipc"))]
fn level_to_str(l: proto::ConsoleLevel) -> &'static str {
    match l {
        proto::ConsoleLevel::Debug => "debug",
        proto::ConsoleLevel::Info => "info",
        proto::ConsoleLevel::Warn => "warn",
        proto::ConsoleLevel::Error => "error",
    }
}

#[cfg(any(feature = "tauri_ipc", feature = "tauri_compat_ipc"))]
fn map_proto_event_to_adapter(evt: &proto::Event) -> Option<(String, serde_json::Value)> {
    match evt {
        proto::Event::Navigation {
            status,
            url,
            http_status,
        } => Some((
            "navigation".to_string(),
            serde_json::json!({
                "status": status_to_str(*status),
                "url": url,
                "http_status": http_status,
            }),
        )),
        proto::Event::ConsoleMessage {
            level,
            message,
            source,
            line,
            column,
        } => Some((
            "console".to_string(),
            serde_json::json!({
                "level": level_to_str(*level),
                "message": message,
                "source": source,
                "line": line,
                "column": column,
            }),
        )),
        proto::Event::DevToolsPortOpened { port, url } => Some((
            "devtools_port_opened".to_string(),
            serde_json::json!({
                "port": port,
                "url": url,
            }),
        )),
        _ => None,
    }
}

async fn send_event(
    transport: &mut dyn IpcTransport,
    next_id: &mut u64,
    evt: proto::Event,
) -> Result<(), Box<dyn std::error::Error>> {
    #[cfg(any(feature = "tauri_ipc", feature = "tauri_compat_ipc"))]
    let emit_msg = map_proto_event_to_adapter(&evt);

    let env = proto::Envelope {
        version: proto::PROTOCOL_VERSION,
        id: take_id(next_id),
        flags: 0,
        message: proto::Message::Event(evt),
    };
    let bytes = serialize(&env)?;
    transport.send_frame(bytes).await?;

    #[cfg(any(feature = "tauri_ipc", feature = "tauri_compat_ipc"))]
    if let Some((name, payload)) = emit_msg {
        if let Some(adapter) = TAURI_CHANNEL.get() {
            adapter.emit_event(&name, &payload);
        }
    }

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
