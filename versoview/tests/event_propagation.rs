#![allow(clippy::needless_return)]

use std::env;
use std::io::{Read, Write};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use bincode;
use photon_standalone::ipc_protocol as proto;
use serde_json::json;

// --- Minimal framed transport helpers (copy of ipc_roundtrip.rs utilities) ---

fn serialize(env: &proto::Envelope) -> Vec<u8> {
    bincode::serde::encode_to_vec(env, bincode::config::standard()).expect("serialize envelope")
}

fn deserialize(buf: &[u8]) -> proto::Envelope {
    let (env, _): (proto::Envelope, _) =
        bincode::serde::decode_from_slice(buf, bincode::config::standard())
            .expect("deserialize envelope");
    env
}

fn write_frame(writer: &mut impl Write, payload: &[u8]) {
    let len = u32::try_from(payload.len()).expect("frame too large");
    let header = len.to_le_bytes();
    writer.write_all(&header).expect("write header");
    if !payload.is_empty() {
        writer.write_all(payload).expect("write payload");
    }
    writer.flush().expect("flush");
}

fn read_exact(reader: &mut impl Read, buf: &mut [u8]) -> std::io::Result<()> {
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

fn read_frame_blocking(reader: &mut impl Read) -> std::io::Result<Vec<u8>> {
    let mut header = [0u8; 4];
    read_exact(reader, &mut header)?;
    let len = u32::from_le_bytes(header) as usize;
    let mut buf = vec![0u8; len];
    if len > 0 {
        read_exact(reader, &mut buf)?;
    }
    Ok(buf)
}

fn spawn_reader<R: Read + Send + 'static>(mut reader: R) -> mpsc::Receiver<Vec<u8>> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        loop {
            match read_frame_blocking(&mut reader) {
                Ok(bytes) => {
                    let _ = tx.send(bytes);
                }
                Err(_) => break, // EOF or error
            }
        }
    });
    rx
}

fn read_frame_timeout(
    rx: &mpsc::Receiver<Vec<u8>>,
    timeout: Duration,
) -> Result<Vec<u8>, &'static str> {
    rx.recv_timeout(timeout)
        .map_err(|_| "timeout reading frame")
}

fn next_id(id: &mut u64) -> u64 {
    let cur = *id;
    *id = id.saturating_add(1);
    cur
}

// --- Mapping helpers (mirror of versoview mapping in Step C) ---

#[cfg(feature = "tauri_ipc")]
fn status_to_str(s: proto::NavigationStatus) -> &'static str {
    match s {
        proto::NavigationStatus::Started => "started",
        proto::NavigationStatus::Committed => "committed",
        proto::NavigationStatus::Finished => "finished",
        proto::NavigationStatus::Failed => "failed",
    }
}

#[cfg(feature = "tauri_ipc")]
fn level_to_str(l: proto::ConsoleLevel) -> &'static str {
    match l {
        proto::ConsoleLevel::Debug => "debug",
        proto::ConsoleLevel::Info => "info",
        proto::ConsoleLevel::Warn => "warn",
        proto::ConsoleLevel::Error => "error",
    }
}

/// This test validates two things:
/// 1) versoview emits protocol events over the wire after Load (ConsoleMessage, Navigation).
/// 2) When the tauri_ipc feature is enabled, we can derive the adapter-mapped payloads and
///    verify that the adapter’s embedded EventBus subscriptions receive the emitted payloads.
///
/// Note: This does not cross the process boundary for the adapter. Instead, it exercises the
///       adapter’s pub/sub locally with payloads derived from real protocol events, ensuring
///       the mapping shape is consistent and the EventBus works as expected.
#[test]
fn event_propagation_maps_and_emits_via_adapter() {
    // Locate photon binary
    let bin = env::var("CARGO_BIN_EXE_photon").ok().or_else(|| {
        let exe = env::current_exe().ok()?;
        let exe_dir = exe.parent()?;
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
        for c in candidates.iter() {
            if c.exists() {
                return Some(c.canonicalize().ok()?.to_string_lossy().to_string());
            }
        }
        None
    }).expect("photon binary not found: set CARGO_BIN_EXE_photon or ensure it's next to the test binary");

    // Spawn with stdio transport
    let mut child = Command::new(bin)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("failed to spawn versoview");

    let mut stdin = child.stdin.take().expect("child stdin");
    let stdout = child.stdout.take().expect("child stdout");
    let rx = spawn_reader(stdout);

    let mut msg_id = 1u64;

    // Init
    let init = proto::Envelope {
        version: proto::PROTOCOL_VERSION,
        id: next_id(&mut msg_id),
        flags: 0,
        message: proto::Message::Request(proto::Request::Init {
            client_name: "event_propagation_test".to_string(),
            supported: proto::VersionRange {
                min: proto::PROTOCOL_MIN_VERSION,
                max: proto::PROTOCOL_MAX_VERSION,
            },
            config: proto::ControllerConfig {
                resources_dir: None,
                devtools_port: Some(0),
                user_agent: Some("verso-event-test".to_string()),
                init_scripts: vec![],
                ipc_tuning: None,
            },
        }),
    };
    write_frame(&mut stdin, &serialize(&init));
    let resp_bytes = read_frame_timeout(&rx, Duration::from_secs(10)).expect("InitAck read");
    let env = deserialize(&resp_bytes);
    match env.message {
        proto::Message::Response(proto::Response::InitAck { in_reply_to, .. }) => {
            assert_eq!(in_reply_to, init.id, "InitAck correlation id");
        }
        other => panic!("unexpected response to Init: {:?}", other),
    }

    // BindSurface
    let bind = proto::Envelope {
        version: proto::PROTOCOL_VERSION,
        id: next_id(&mut msg_id),
        flags: 0,
        message: proto::Message::Request(proto::Request::BindSurface {
            surface: proto::SurfaceDescriptor::Fallback {
                width: 800,
                height: 600,
                scale_factor: 1.0,
            },
        }),
    };
    write_frame(&mut stdin, &serialize(&bind));
    let resp_bytes = read_frame_timeout(&rx, Duration::from_secs(3)).expect("BindSurface Ack read");
    let env = deserialize(&resp_bytes);
    match env.message {
        proto::Message::Response(proto::Response::Ack { in_reply_to }) => {
            assert_eq!(in_reply_to, bind.id, "BindSurface Ack correlation id");
        }
        other => panic!("unexpected response to BindSurface: {:?}", other),
    }

    // Load about:blank
    let test_url = "about:blank";
    let load = proto::Envelope {
        version: proto::PROTOCOL_VERSION,
        id: next_id(&mut msg_id),
        flags: 0,
        message: proto::Message::Request(proto::Request::Load {
            url: test_url.to_string(),
        }),
    };
    write_frame(&mut stdin, &serialize(&load));
    let resp_bytes = read_frame_timeout(&rx, Duration::from_secs(3)).expect("Load Ack read");
    let env = deserialize(&resp_bytes);
    match env.message {
        proto::Message::Response(proto::Response::Ack { in_reply_to }) => {
            assert_eq!(in_reply_to, load.id, "Load Ack correlation id");
        }
        other => panic!("unexpected response to Load: {:?}", other),
    }

    // Collect protocol events
    let mut proto_console: Option<(proto::ConsoleLevel, String)> = None;
    let mut proto_navigation: Option<(proto::NavigationStatus, Option<String>, Option<u16>)> = None;

    for _ in 0..20 {
        let evt_bytes = match read_frame_timeout(&rx, Duration::from_millis(300)) {
            Ok(b) => b,
            Err(_) => Vec::new(),
        };
        if evt_bytes.is_empty() {
            continue;
        }
        let env = deserialize(&evt_bytes);
        match env.message {
            proto::Message::Event(proto::Event::ConsoleMessage { level, message, .. }) => {
                if message.contains("Load requested") {
                    proto_console = Some((level, message));
                }
            }
            proto::Message::Event(proto::Event::Navigation {
                status,
                url,
                http_status,
            }) => {
                if matches!(status, proto::NavigationStatus::Committed)
                    && matches!(url.as_deref(), Some("about:blank"))
                {
                    proto_navigation = Some((status, url, http_status));
                }
            }
            _ => {}
        }
        if proto_console.is_some() && proto_navigation.is_some() {
            break;
        }
    }

    assert!(
        proto_console.is_some(),
        "expected ConsoleMessage after Load"
    );
    assert!(
        proto_navigation.is_some(),
        "expected Navigation::Committed after Load"
    );

    // If tauri_ipc is enabled for this crate, validate adapter pub/sub with mapped payloads.
    #[cfg(feature = "tauri_ipc")]
    {
        use tauri_ipc_adapter::ChannelAdapter;

        let adapter = ChannelAdapter::new();

        // Subscribe to "console" and "navigation"
        let (tx_console, rx_console) = mpsc::channel::<serde_json::Value>();
        let (tx_nav, rx_nav) = mpsc::channel::<serde_json::Value>();

        let _id_console = adapter.subscribe(
            "console",
            Box::new(move |payload| {
                let _ = tx_console.send(payload.clone());
            }),
        );
        let _id_nav = adapter.subscribe(
            "navigation",
            Box::new(move |payload| {
                let _ = tx_nav.send(payload.clone());
            }),
        );

        // Emit derived events consistent with versoview's mapping
        if let Some((level, message)) = proto_console.as_ref() {
            let console_payload = json!({
                "level": level_to_str(*level),
                "message": message,
                "source": "versoview", // best-effort based on server code
                "line": null,
                "column": null,
            });
            adapter.emit_event("console", &console_payload);
            let recv = rx_console
                .recv_timeout(Duration::from_secs(2))
                .expect("console event not received");
            assert_eq!(recv["message"], console_payload["message"]);
            assert_eq!(recv["level"], console_payload["level"]);
        }

        if let Some((status, url, http_status)) = proto_navigation.as_ref() {
            let nav_payload = json!({
                "status": status_to_str(*status),
                "url": url,
                "http_status": http_status,
            });
            adapter.emit_event("navigation", &nav_payload);
            let recv = rx_nav
                .recv_timeout(Duration::from_secs(2))
                .expect("navigation event not received");
            assert_eq!(recv["status"], nav_payload["status"]);
            assert_eq!(recv["url"], nav_payload["url"]);
        }
    }

    // Shutdown
    let shutdown = proto::Envelope {
        version: proto::PROTOCOL_VERSION,
        id: next_id(&mut msg_id),
        flags: 0,
        message: proto::Message::Request(proto::Request::Shutdown),
    };
    write_frame(&mut stdin, &serialize(&shutdown));

    let _ = read_frame_timeout(&rx, Duration::from_secs(3)); // best-effort Ack read

    // Cleanup: drop pipes and wait for server to exit.
    drop(stdin);
    let _ = child.kill();
    let _ = child.wait();
}
