use std::cmp;

use bytes::Bytes;

use tracing::{error, info, warn};
use verso_standalone::ipc_protocol as proto;
use verso_standalone::ipc_transport::IpcTransport;
use verso_standalone::ipc_transport::StdIoTransport;

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    init_tracing();

    if let Err(e) = run_server().await {
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

async fn run_server() -> Result<(), Box<dyn std::error::Error>> {
    // Framed stdio transport: read requests from stdin, write responses/events to stdout.
    let mut transport = StdIoTransport::new(tokio::io::stdin(), tokio::io::stdout());

    let server_name = "versoview";
    let mut next_id: u64 = 1;

    info!("versoview server started (stdio transport)");

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
                            &mut transport,
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
    transport: &mut StdIoTransport<tokio::io::Stdin, tokio::io::Stdout>,
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
                fd_passing: false,
                zero_copy_frames: false,
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
            // Acknowledge and emit a dummy event to simulate a frame path.
            send_response(
                transport,
                next_id,
                proto::Response::Ack {
                    in_reply_to: req_id,
                },
            )
            .await?;
            let evt = proto::Event::ConsoleMessage {
                level: proto::ConsoleLevel::Info,
                message: "Draw requested".to_string(),
                source: Some("versoview".to_string()),
                line: None,
                column: None,
            };
            send_event(transport, next_id, evt).await?;
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
    transport: &mut StdIoTransport<tokio::io::Stdin, tokio::io::Stdout>,
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
    transport: &mut StdIoTransport<tokio::io::Stdin, tokio::io::Stdout>,
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

fn take_id(next_id: &mut u64) -> u64 {
    let id = *next_id;
    *next_id = next_id.saturating_add(1);
    id
}
