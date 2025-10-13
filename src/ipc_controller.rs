#![cfg(feature = "versoview-runtime")]
/*!
IpcController – async IPC client for versoview

This implementation:
- Spawns the `versoview` child process with piped stdio.
- Establishes a framed transport over stdio (length-prefixed).
- Performs a versioned handshake (Init -> InitAck).
- Sends BindSurface with a portable descriptor (fallback initially).
- Provides synchronous WebviewController methods by dispatching requests onto
  a background Tokio runtime and awaiting acknowledgements with timeouts.
- Receives responses/events on a background task and routes replies via
  correlation IDs to waiting callers.

Notes:
- Events from the server are currently consumed and ignored (TODO: integrate a
  host callback or a runtime user-event channel for forwarding).
- Surface binding uses a Fallback descriptor for portability; platform-specific
  zero-copy paths can be added later as transports that support handle passing.

Safety and threading:
- The WebviewController trait is synchronous; this client runs an internal Tokio
  multi-thread runtime and small background tasks. Methods block briefly while awaiting acks.
*/

use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};

use bytes::Bytes;
use tokio::{
    process::{Child, Command},
    runtime::{Builder, Runtime},
    select,
    sync::{mpsc, oneshot},
    task::JoinHandle,
    time::timeout,
};
use winit::dpi::PhysicalSize;

use crate::controller::{
    ButtonState, ControllerCapabilities, ControllerConfig, ControllerError, ControllerEvent,
    ControllerEventSink, InputEvent, WebviewController,
};
use crate::winit_runtime::NativeSurfaceHandles;
use crate::{verso_devtools_port, verso_path, verso_resource_directory};

use crate::ipc_protocol as proto;
use crate::ipc_transport::{IpcTransport, StdIoTransport};

/// Maximum time we wait for a request ack before treating it as a failure.
const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

/// Internal command to gracefully shut down background tasks.
#[derive(Debug)]
enum BgCommand {
    Payload(Bytes),
    Close,
}

/// Shared state for request/response correlation.
struct SharedState {
    /// Map from correlation id -> oneshot sender to deliver a Response.
    pending: HashMap<u64, oneshot::Sender<proto::Response>>,
}

/// IPC-based controller that communicates with an external `versoview` process.
pub struct IpcController {
    // Lifecycle
    initialized: bool,
    connected: bool,
    alive: Arc<AtomicBool>,

    // Config and diagnostics
    config: Option<ControllerConfig>,
    last_url: Option<String>,
    last_script: Option<String>,
    last_size: Option<PhysicalSize<u32>>,
    input_events: usize,
    draws_requested: usize,
    draws_performed: usize,
    visible: bool,
    negotiated_caps: Option<proto::Capabilities>,

    // Async event sink for forwarding server events
    event_sink: Arc<Mutex<Option<ControllerEventSink>>>,

    // Async runtime and worker tasks
    rt: Option<Runtime>,
    child: Option<Child>,
    bg_task: Option<JoinHandle<()>>,
    router_task: Option<JoinHandle<()>>,
    tx: Option<mpsc::Sender<BgCommand>>,
    shared: Arc<Mutex<SharedState>>,
    next_id: Arc<AtomicU64>,
}

impl Default for IpcController {
    fn default() -> Self {
        Self {
            initialized: false,
            connected: false,
            alive: Arc::new(AtomicBool::new(false)),
            config: None,
            last_url: None,
            last_script: None,
            last_size: None,
            input_events: 0,
            draws_requested: 0,
            draws_performed: 0,
            visible: true,
            negotiated_caps: None,
            event_sink: Arc::new(Mutex::new(None)),
            rt: None,
            child: None,
            bg_task: None,
            router_task: None,
            tx: None,
            shared: Arc::new(Mutex::new(SharedState {
                pending: HashMap::new(),
            })),
            next_id: Arc::new(AtomicU64::new(1)),
        }
    }
}

impl IpcController {
    pub fn new() -> Self {
        Self::default()
    }

    fn ensure_initialized(&self) -> Result<(), ControllerError> {
        if !self.initialized {
            return Err(ControllerError::NotInitialized(
                "call initialize() before using the controller",
            ));
        }
        Ok(())
    }

    fn ensure_ready(&self) -> Result<(), ControllerError> {
        self.ensure_initialized()?;
        if !self.connected {
            return Err(ControllerError::SurfaceNotBound);
        }
        if !self.alive.load(Ordering::SeqCst) {
            return Err(ControllerError::InvalidState("controller not alive"));
        }
        Ok(())
    }

    fn take_rt(&self) -> Result<&Runtime, ControllerError> {
        self.rt
            .as_ref()
            .ok_or_else(|| ControllerError::InvalidState("tokio runtime not initialized"))
    }

    fn next_msg_id(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }

    fn serialize_envelope(env: &proto::Envelope) -> Result<Bytes, ControllerError> {
        let cfg = bincode::config::standard();
        bincode::serde::encode_to_vec(env, cfg)
            .map(Bytes::from)
            .map_err(|e| ControllerError::BackendMsg(format!("serialize: {e}")))
    }

    fn send_request_and_wait(
        &mut self,
        req: proto::Request,
    ) -> Result<proto::Response, ControllerError> {
        let env = proto::Envelope {
            version: proto::PROTOCOL_VERSION,
            id: self.next_msg_id(),
            flags: 0,
            message: proto::Message::Request(req),
        };
        let payload = Self::serialize_envelope(&env)?;
        let (tx_resp, rx_resp) = oneshot::channel::<proto::Response>();

        // Register pending before sending
        {
            let mut guard = self.shared.lock().unwrap();
            guard.pending.insert(env.id, tx_resp);
        }

        // Send payload
        let tx = self
            .tx
            .as_ref()
            .ok_or_else(|| ControllerError::InvalidState("transport not ready"))?
            .clone();
        let rt = self.take_rt()?.handle().clone();
        // Dispatch send and wait for response with a timeout
        let fut = async move {
            // Send frame
            if tx.send(BgCommand::Payload(payload)).await.is_err() {
                return Err(ControllerError::Backend("send failed"));
            }
            // Await correlated response
            match timeout(DEFAULT_REQUEST_TIMEOUT, rx_resp).await {
                Ok(Ok(resp)) => Ok(resp),
                Ok(Err(_canceled)) => Err(ControllerError::Backend("response channel canceled")),
                Err(_elapsed) => Err(ControllerError::Backend("request timed out")),
            }
        };
        rt.block_on(fut)
    }

    fn send_fire_and_forget(&mut self, req: proto::Request) -> Result<(), ControllerError> {
        let env = proto::Envelope {
            version: proto::PROTOCOL_VERSION,
            id: self.next_msg_id(),
            flags: 0,
            message: proto::Message::Request(req),
        };
        let payload = Self::serialize_envelope(&env)?;
        let tx = self
            .tx
            .as_ref()
            .ok_or_else(|| ControllerError::InvalidState("transport not ready"))?
            .clone();
        let rt = self.take_rt()?.handle().clone();
        let fut = async move {
            tx.send(BgCommand::Payload(payload))
                .await
                .map_err(|_| ControllerError::Backend("send failed"))
        };
        rt.block_on(fut)
    }

    fn to_proto_config(cfg: &ControllerConfig) -> proto::ControllerConfig {
        // Resource directory selection: prefer explicit config, else global setting, else None
        let resources_dir = cfg
            .resources_dir
            .clone()
            .or_else(|| verso_resource_directory())
            .map(|p| proto::PathString(p.to_string_lossy().to_string()));

        // Devtools: prefer explicit config, else global setting, else None
        let devtools_port = cfg.devtools_port.or_else(|| verso_devtools_port());

        proto::ControllerConfig {
            resources_dir,
            devtools_port,
            user_agent: cfg.user_agent.clone(),
            init_scripts: cfg.init_scripts.clone(),
            ipc_tuning: None,
        }
    }

    fn to_proto_surface_descriptor(_handles: &NativeSurfaceHandles) -> proto::SurfaceDescriptor {
        // First cut: use a platform-agnostic fallback; platform-specialized mapping can be added later.
        proto::SurfaceDescriptor::Fallback {
            width: 0,
            height: 0,
            scale_factor: 1.0,
        }
    }

    fn map_mouse_button(code: u8) -> proto::MouseButton {
        match code {
            1 => proto::MouseButton::Left,
            2 => proto::MouseButton::Right,
            3 => proto::MouseButton::Middle,
            other => proto::MouseButton::Other(other),
        }
    }

    fn map_input_event(ev: InputEvent) -> proto::IpcInputEvent {
        match ev {
            InputEvent::MouseMove { x, y } => proto::IpcInputEvent::MouseMove {
                x,
                y,
                timestamp_us: None,
            },
            InputEvent::MouseButton { button, state } => {
                let btn = Self::map_mouse_button(button);
                let st = match state {
                    ButtonState::Pressed => proto::ButtonState::Pressed,
                    ButtonState::Released => proto::ButtonState::Released,
                };
                proto::IpcInputEvent::MouseButton {
                    button: btn,
                    state: st,
                    timestamp_us: None,
                }
            }
            InputEvent::Scroll {
                delta_x,
                delta_y,
                is_precise,
            } => proto::IpcInputEvent::Scroll {
                delta_x,
                delta_y,
                is_precise,
                timestamp_us: None,
            },
            InputEvent::KeyDown { key } => proto::IpcInputEvent::KeyDown {
                key: key.to_string(),
                timestamp_us: None,
            },
            InputEvent::KeyUp { key } => proto::IpcInputEvent::KeyUp {
                key: key.to_string(),
                timestamp_us: None,
            },
            InputEvent::CharInput { ch } => proto::IpcInputEvent::CharInput {
                ch,
                timestamp_us: None,
            },
            InputEvent::FocusChanged { focused } => proto::IpcInputEvent::FocusChanged {
                focused,
                timestamp_us: None,
            },
            InputEvent::PointerEntered => {
                proto::IpcInputEvent::PointerEntered { timestamp_us: None }
            }
            InputEvent::PointerLeft => proto::IpcInputEvent::PointerLeft { timestamp_us: None },
        }
    }

    fn spawn_child(&mut self, bin_path: PathBuf) -> Result<(), ControllerError> {
        let mut cmd = Command::new(&bin_path);
        // Use piped stdio for framed transport
        cmd.stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::inherit())
            .kill_on_drop(true);
        let child = cmd
            .spawn()
            .map_err(|e| ControllerError::BackendMsg(format!("spawn versoview: {e}")))?;
        self.child = Some(child);
        Ok(())
    }

    fn start_transport_tasks(&mut self) -> Result<(), ControllerError> {
        let child = self
            .child
            .as_mut()
            .ok_or_else(|| ControllerError::InvalidState("child not spawned"))?;

        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| ControllerError::Backend("child stdout not available"))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| ControllerError::Backend("child stdin not available"))?;

        let mut transport = StdIoTransport::new(stdout, stdin);

        // Channel to feed outbound payloads and close signal.
        let (tx, mut rx) = mpsc::channel::<BgCommand>(128);
        self.tx = Some(tx);

        let shared = Arc::clone(&self.shared);
        let alive = Arc::clone(&self.alive);
        let event_sink = Arc::clone(&self.event_sink);

        // Background transport task: multiplex sending and receiving.
        let bg = self.take_rt()?.spawn(async move {
            loop {
                select! {
                    // Receive next incoming frame
                    inbound = transport.next_frame() => {
                        match inbound {
                            Ok(bytes) => {
                                // Deserialize envelope and route if Response
                                // zero_copy (future): when feature and transport support handle-passing,
                                // prefer transport.next_frame_with_handles() here and capture the returned handle vector.
                                // For now we intentionally ignore handles and only decode the envelope bytes.
                                match bincode::serde::decode_from_slice::<proto::Envelope, _>(&bytes, bincode::config::standard()) {
                                    Ok((env, _)) => {
                                        match env.message {
                                            proto::Message::Response(resp) => {
                                                // Route response by in_reply_to
                                                let key = match &resp {
                                                    proto::Response::Ack { in_reply_to } => *in_reply_to,
                                                    proto::Response::Error { in_reply_to, .. } => *in_reply_to,
                                                    proto::Response::InitAck { in_reply_to, .. } => *in_reply_to,
                                                };
                                                let maybe_tx = {
                                                    let mut st = shared.lock().unwrap();
                                                    st.pending.remove(&key)
                                                };
                                                if let Some(tx) = maybe_tx {
                                                    let _ = tx.send(resp);
                                                }
                                            }
                                            proto::Message::Event(evt) => {
                                                // Map proto events to controller events and forward via sink if present.
                                                match evt {
                                                    proto::Event::ConsoleMessage { level, message, source, line, column } => {
                                                        let lvl = match level {
                                                            proto::ConsoleLevel::Debug => crate::controller::ConsoleLevel::Debug,
                                                            proto::ConsoleLevel::Info => crate::controller::ConsoleLevel::Info,
                                                            proto::ConsoleLevel::Warn => crate::controller::ConsoleLevel::Warn,
                                                            proto::ConsoleLevel::Error => crate::controller::ConsoleLevel::Error,
                                                        };
                                                        if let Some(cb) = event_sink.lock().unwrap().as_ref() {
                                                            cb(ControllerEvent::Console {
                                                                level: lvl,
                                                                message,
                                                                source,
                                                                line,
                                                                column,
                                                            });
                                                        }
                                                    }
                                                    proto::Event::Navigation { status, url, http_status } => {
                                                        let st = match status {
                                                            proto::NavigationStatus::Started => crate::controller::NavigationStatus::Started,
                                                            proto::NavigationStatus::Committed => crate::controller::NavigationStatus::Committed,
                                                            proto::NavigationStatus::Finished => crate::controller::NavigationStatus::Finished,
                                                            proto::NavigationStatus::Failed => crate::controller::NavigationStatus::Failed,
                                                        };
                                                        if let Some(cb) = event_sink.lock().unwrap().as_ref() {
                                                            cb(ControllerEvent::Navigation {
                                                                status: st,
                                                                url,
                                                                http_status,
                                                            });
                                                        }
                                                    }
                                                    proto::Event::DevToolsPortOpened { port, url } => {
                                                        if let Some(cb) = event_sink.lock().unwrap().as_ref() {
                                                            cb(ControllerEvent::DevToolsPortOpened { port, url });
                                                        }
                                                    }
                                                    proto::Event::FrameReady { frame_id, descriptor } => {
                                                        // Extract dimensions if available
                                                        let (width, height) = match descriptor {
                                                            proto::FrameDescriptor::LinuxDmabuf { width, height, .. } => (width, height),
                                                            proto::FrameDescriptor::MacOsIoSurface { width, height, .. } => (width, height),
                                                            proto::FrameDescriptor::WindowsDxgiSharedHandle { width, height, .. } => (width, height),
                                                            proto::FrameDescriptor::SharedMemoryImage { width, height, .. } => (width, height),
                                                            proto::FrameDescriptor::CompressedImage { width, height, .. } => (width, height),
                                                        };
                                                        if let Some(cb) = event_sink.lock().unwrap().as_ref() {
                                                            cb(ControllerEvent::FrameReady {
                                                                width,
                                                                height,
                                                                frame_id,
                                                            });
                                                        }
                                                    }
                                                }
                                            }
                                            proto::Message::Request(_) => {
                                                // Server should not send a Request; ignore/bail if needed.
                                            }
                                        }
                                    }
                                    Err(_e) => {
                                        // Malformed frame; keep running or decide to break.
                                        // For now, continue.
                                    }
                                }
                            }
                            Err(_e) => {
                                // Transport ended or failed; mark as not alive and break.
                                alive.store(false, Ordering::SeqCst);
                                break;
                            }
                        }
                    }
                    // Outbound payload to send
                    cmd = rx.recv() => {
                        match cmd {
                            Some(BgCommand::Payload(bytes)) => {
                                if let Err(_e) = transport.send_frame(bytes).await {
                                    // Sending failed; mark dead and break.
                                    alive.store(false, Ordering::SeqCst);
                                    break;
                                }
                            }
                            Some(BgCommand::Close) | None => {
                                // Graceful close requested or sender dropped.
                                let _ = transport.close().await;
                                break;
                            }
                        }
                    }
                }
            }
        });
        self.bg_task = Some(bg);

        // Child monitoring not implemented: tokio::process::Child cannot be cloned to wait concurrently.

        Ok(())
    }

    fn do_handshake(&mut self) -> Result<(), ControllerError> {
        let cfg = self
            .config
            .as_ref()
            .ok_or_else(|| ControllerError::InvalidState("missing config"))?;
        let proto_cfg = Self::to_proto_config(cfg);

        // Send Init and await InitAck
        let init_req = proto::Request::Init {
            client_name: "verso-standalone".to_string(),
            supported: proto::VersionRange::default(),
            config: proto_cfg,
        };
        match self.send_request_and_wait(init_req)? {
            proto::Response::InitAck {
                in_reply_to: _,
                chosen_version: _,
                server_name: _,
                capabilities,
            } => {
                self.negotiated_caps = Some(capabilities);
                // OK
            }
            other => {
                return Err(ControllerError::BackendMsg(format!(
                    "unexpected response to Init: {:?}",
                    other
                )));
            }
        }

        Ok(())
    }
}

impl WebviewController for IpcController {
    fn initialize(&mut self, config: ControllerConfig) -> Result<(), ControllerError> {
        if self.initialized {
            return Err(ControllerError::InvalidState(
                "initialize() called more than once",
            ));
        }

        // Start a small multi-thread Tokio runtime for this controller.
        let rt = Builder::new_multi_thread()
            .enable_all()
            .worker_threads(2)
            .thread_name("verso-ipc")
            .build()
            .map_err(|e| ControllerError::BackendMsg(format!("tokio init: {e}")))?;
        self.rt = Some(rt);

        self.config = Some(config);
        self.initialized = true;
        self.alive.store(false, Ordering::SeqCst);
        Ok(())
    }

    fn bind_surface(&mut self, handles: NativeSurfaceHandles) -> Result<(), ControllerError> {
        self.ensure_initialized()?;
        if self.connected {
            return Err(ControllerError::InvalidState(
                "bind_surface() called more than once",
            ));
        }

        // Resolve versoview binary path: prefer explicit config, else global path.
        let bin = self
            .config
            .as_ref()
            .and_then(|c| c.external_bin_path.clone())
            .unwrap_or_else(|| verso_path().to_path_buf());

        // Spawn the child and start the framed transport.
        self.spawn_child(bin)?;
        self.start_transport_tasks()?;

        // Child is up; mark alive (until crash or shutdown).
        self.alive.store(true, Ordering::SeqCst);

        // Perform handshake
        self.do_handshake()?;

        // Bind surface (fallback descriptor for now)
        let surface = Self::to_proto_surface_descriptor(&handles);
        match self.send_request_and_wait(proto::Request::BindSurface { surface })? {
            proto::Response::Ack { .. } => {}
            proto::Response::Error { code, message, .. } => {
                return Err(ControllerError::BackendMsg(format!(
                    "bind surface error: {:?}: {}",
                    code, message
                )));
            }
            other => {
                return Err(ControllerError::BackendMsg(format!(
                    "unexpected response to BindSurface: {:?}",
                    other
                )));
            }
        }

        self.connected = true;
        Ok(())
    }

    fn load(&mut self, url_or_resource: &str) -> Result<(), ControllerError> {
        self.ensure_ready()?;
        self.last_url = Some(url_or_resource.to_string());
        // Await Ack to keep ordering disciplined.
        match self.send_request_and_wait(proto::Request::Load {
            url: url_or_resource.to_string(),
        })? {
            proto::Response::Ack { .. } => Ok(()),
            proto::Response::Error { code, message, .. } => Err(ControllerError::BackendMsg(
                format!("load error: {:?}: {}", code, message),
            )),
            other => Err(ControllerError::BackendMsg(format!(
                "unexpected response to Load: {:?}",
                other
            ))),
        }
    }

    fn eval_script(&mut self, source: &str) -> Result<(), ControllerError> {
        self.ensure_ready()?;
        self.last_script = Some(source.to_string());
        // Fire-and-forget is acceptable, but we await Ack for early error detection.
        match self.send_request_and_wait(proto::Request::EvalScript {
            source: source.to_string(),
            request_id: None,
        })? {
            proto::Response::Ack { .. } => Ok(()),
            proto::Response::Error { code, message, .. } => Err(ControllerError::BackendMsg(
                format!("eval_script error: {:?}: {}", code, message),
            )),
            other => Err(ControllerError::BackendMsg(format!(
                "unexpected response to EvalScript: {:?}",
                other
            ))),
        }
    }

    fn resize(&mut self, size: PhysicalSize<u32>) -> Result<(), ControllerError> {
        self.ensure_ready()?;
        self.last_size = Some(size);
        match self.send_request_and_wait(proto::Request::Resize {
            width: size.width,
            height: size.height,
        })? {
            proto::Response::Ack { .. } => Ok(()),
            proto::Response::Error { code, message, .. } => Err(ControllerError::BackendMsg(
                format!("resize error: {:?}: {}", code, message),
            )),
            other => Err(ControllerError::BackendMsg(format!(
                "unexpected response to Resize: {:?}",
                other
            ))),
        }
    }

    fn request_draw(&mut self) -> Result<(), ControllerError> {
        self.ensure_ready()?;
        self.draws_requested += 1;
        // Fire-and-forget is fine for request draw; don't block UI unnecessarily.
        self.send_fire_and_forget(proto::Request::RequestDraw)
    }

    fn draw(&mut self) -> Result<(), ControllerError> {
        self.ensure_ready()?;
        self.draws_performed += 1;
        // Prefer Ack to ensure present is accepted by server.
        match self.send_request_and_wait(proto::Request::DrawNow)? {
            proto::Response::Ack { .. } => Ok(()),
            proto::Response::Error { code, message, .. } => Err(ControllerError::BackendMsg(
                format!("draw error: {:?}: {}", code, message),
            )),
            other => Err(ControllerError::BackendMsg(format!(
                "unexpected response to DrawNow: {:?}",
                other
            ))),
        }
    }

    fn send_input(&mut self, event: InputEvent) -> Result<(), ControllerError> {
        self.ensure_ready()?;
        self.input_events += 1;
        let e = Self::map_input_event(event);
        // Fire-and-forget; inputs are high-frequency.
        self.send_fire_and_forget(proto::Request::InputEvent { events: vec![e] })
    }

    fn set_visibility(&mut self, visible: bool) -> Result<(), ControllerError> {
        self.ensure_ready()?;
        // Protocol does not define visibility yet; track locally.
        self.visible = visible;
        Ok(())
    }

    fn focus(&mut self) -> Result<(), ControllerError> {
        self.ensure_ready()?;
        // Protocol does not define focus yet; no-op for now.
        Ok(())
    }

    fn set_event_sink(&mut self, sink: ControllerEventSink) -> Result<(), ControllerError> {
        *self.event_sink.lock().unwrap() = Some(sink);
        Ok(())
    }

    fn capabilities(&self) -> Option<ControllerCapabilities> {
        self.negotiated_caps
            .as_ref()
            .map(|caps| ControllerCapabilities {
                fd_passing: caps.fd_passing,
                zero_copy_frames: caps.zero_copy_frames,
                compressed_frames: caps.compressed_frames,
            })
    }

    fn is_alive(&self) -> bool {
        self.alive.load(std::sync::atomic::Ordering::SeqCst)
    }

    fn shutdown(&mut self) -> Result<(), ControllerError> {
        self.ensure_initialized()?;

        // Best-effort: ask server for graceful shutdown
        if self.connected && self.is_alive() {
            let _ = self.send_fire_and_forget(proto::Request::Shutdown);
        }

        // Close the transport task
        if let Some(tx) = self.tx.take() {
            let rt = self.take_rt()?.handle().clone();
            let _ = rt.block_on(async move {
                // ignore errors
                let _ = tx.send(BgCommand::Close).await;
            });
        }

        // Kill the child if still alive
        if let Some(mut child) = self.child.take() {
            let rt = self.take_rt()?.handle().clone();
            let _ = rt.block_on(async move {
                let _ = child.start_kill();
                let _ = child.wait().await;
            });
        }

        // Join background tasks
        if let Some(handle) = self.bg_task.take() {
            let rt = self.take_rt()?.handle().clone();
            let _ = rt.block_on(async move {
                let _ = handle.await;
            });
        }
        if let Some(handle) = self.router_task.take() {
            let rt = self.take_rt()?.handle().clone();
            let _ = rt.block_on(async move {
                let _ = handle.await;
            });
        }

        self.alive.store(false, Ordering::SeqCst);
        self.connected = false;

        Ok(())
    }
}
