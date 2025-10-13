#![allow(clippy::needless_return)]

use std::env;
use std::io::{Read, Write};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use bincode;
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
                Err(_) => {
                    // EOF or error: exit reader loop
                    break;
                }
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

#[test]
fn ipc_roundtrip_init_bind_load_and_events() {
    // Locate the compiled server binary. Cargo sets this env var for integration tests.
    let bin = env::var("CARGO_BIN_EXE_versoview").ok().or_else(|| {
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
    }).expect("versoview binary not found: set CARGO_BIN_EXE_versoview or ensure it's next to the test binary");

    // Spawn the server with piped stdio
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

    // Step 1: Init -> InitAck
    let init = proto::Envelope {
        version: proto::PROTOCOL_VERSION,
        id: next_id(&mut msg_id),
        flags: 0,
        message: proto::Message::Request(proto::Request::Init {
            client_name: "ipc_roundtrip_test".to_string(),
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
    let bytes = serialize(&init);
    write_frame(&mut stdin, &bytes);

    let resp_bytes = read_frame_timeout(&rx, Duration::from_secs(20)).expect("InitAck read");
    let env = deserialize(&resp_bytes);
    match env.message {
        proto::Message::Response(proto::Response::InitAck { in_reply_to, .. }) => {
            assert_eq!(in_reply_to, init.id, "InitAck correlation id");
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

    // Step 3: Load -> Ack
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

    // Step 4: Expect events (ConsoleMessage and Navigation)
    let mut saw_console = false;
    let mut saw_navigation = false;

    for _ in 0..10 {
        // Allow up to ~5 seconds while reading events
        let evt_bytes = match read_frame_timeout(&rx, Duration::from_millis(500)) {
            Ok(b) => b,
            Err(_) => {
                // No event in this slice; continue polling windows.
                Vec::new()
            }
        };

        if evt_bytes.is_empty() {
            continue;
        }

        let env = deserialize(&evt_bytes);
        match env.message {
            proto::Message::Event(proto::Event::ConsoleMessage {
                level: _, message, ..
            }) => {
                // Basic sanity: message should contain "Load requested"
                if message.contains("Load requested") {
                    saw_console = true;
                }
            }
            proto::Message::Event(proto::Event::Navigation { status, url, .. }) => {
                if matches!(status, proto::NavigationStatus::Committed) {
                    if let Some(u) = url {
                        if u == test_url {
                            saw_navigation = true;
                        }
                    }
                }
            }
            _ => {
                // Ignore other events in this basic integration test
            }
        }

        if saw_console && saw_navigation {
            break;
        }
    }

    assert!(saw_console, "expected at least one ConsoleMessage event");
    assert!(
        saw_navigation,
        "expected a Navigation::Committed event for the loaded URL"
    );

    // Step 5: Shutdown -> Ack
    let shutdown = proto::Envelope {
        version: proto::PROTOCOL_VERSION,
        id: next_id(&mut msg_id),
        flags: 0,
        message: proto::Message::Request(proto::Request::Shutdown),
    };
    write_frame(&mut stdin, &serialize(&shutdown));

    let resp_bytes = read_frame_timeout(&rx, Duration::from_secs(3)).expect("Shutdown Ack read");
    let env = deserialize(&resp_bytes);
    match env.message {
        proto::Message::Response(proto::Response::Ack { in_reply_to }) => {
            assert_eq!(in_reply_to, shutdown.id, "Shutdown Ack correlation id");
        }
        other => panic!("unexpected response to Shutdown: {:?}", other),
    }

    // Cleanup: drop pipes and wait for server to exit.
    drop(stdin);

    let _ = child.kill();
    let _ = child.wait();
}
