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
#[cfg(all(unix, feature = "zero_copy"))]
use crate::transport_unix::UnixSocketTransport;

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
    #[cfg(all(unix, feature = "zero_copy"))]
    pending_handles: Arc<Mutex<HashMap<u64, Vec<crate::ipc_transport::OpaqueHandle>>>>,
    #[cfg(all(unix, feature = "zero_copy"))]
    last_frame_valid: Arc<AtomicBool>,
    #[cfg(all(unix, feature = "zero_copy"))]
    last_frame_bytes: Arc<Mutex<Option<Vec<u8>>>>,

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
            #[cfg(all(unix, feature = "zero_copy"))]
            pending_handles: Arc::new(Mutex::new(HashMap::new())),
            #[cfg(all(unix, feature = "zero_copy"))]
            last_frame_valid: Arc::new(AtomicBool::new(false)),
            #[cfg(all(unix, feature = "zero_copy"))]
            last_frame_bytes: Arc::new(Mutex::new(None)),
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

#[cfg(all(unix, feature = "zero_copy"))]
pub struct ReceivedHandle {
    fd: std::os::unix::io::RawFd,
}

#[cfg(all(unix, feature = "zero_copy"))]
impl ReceivedHandle {
    pub fn from_opaque(handles: Vec<crate::ipc_transport::OpaqueHandle>) -> Option<Self> {
        handles
            .first()
            .map(|h| ReceivedHandle { fd: (*h).0 as i32 })
    }

    /// Map the underlying FD into memory read-only and return an owned Mmap.
    /// Safety: the server must ensure the file is at least `length` bytes long.
    pub fn mmap_read(
        &self,
        length: usize,
    ) -> Result<memmap2::Mmap, crate::controller::ControllerError> {
        // Duplicate the fd so the temporary File we construct does not affect our owned fd.
        let dup_fd = nix::unistd::dup(self.fd)
            .map_err(|e| crate::controller::ControllerError::BackendMsg(format!("dup fd: {e}")))?;
        // SAFETY: dup_fd is a freshly-duplicated, valid file descriptor we own.
        let file = unsafe { <std::fs::File as std::os::unix::io::FromRawFd>::from_raw_fd(dup_fd) };
        // SAFETY: mapping a file descriptor; kernel enforces page alignment internally.
        let mmap = unsafe { memmap2::MmapOptions::new().len(length).map(&file) }
            .map_err(|e| crate::controller::ControllerError::BackendMsg(format!("mmap: {e}")))?;
        // Dropping `file` here closes only the duplicated fd; the original fd remains owned by this handle.
        Ok(mmap)
    }

    /// Explicitly close the underlying FD early. Dropping the handle also closes it.
    pub fn close(self) {
        let fd = self.fd;
        std::mem::forget(self);
        let _ = nix::unistd::close(fd);
    }

    /// Transfer ownership of the raw fd to the caller. The handle will not close it on drop.
    pub fn into_raw_fd(self) -> std::os::unix::io::RawFd {
        let fd = self.fd;
        std::mem::forget(self);
        fd
    }
}

#[cfg(all(unix, feature = "zero_copy"))]
impl Drop for ReceivedHandle {
    fn drop(&mut self) {
        let _ = nix::unistd::close(self.fd);
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

        let mut init_scripts = cfg.init_scripts.clone();
        if let Some(path) = cfg.unix_socket_path.clone() {
            init_scripts.push(format!("__ipc_unix_socket_path={}", path.to_string_lossy()));
        }
        proto::ControllerConfig {
            resources_dir,
            devtools_port,
            user_agent: cfg.user_agent.clone(),
            init_scripts,
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
        // Prepare transport: prefer Unix domain socket if configured and supported; else fallback to stdio.
        #[cfg(all(unix, feature = "zero_copy"))]
        let rt_handle = self.take_rt()?.handle().clone();

        let child = self
            .child
            .as_mut()
            .ok_or_else(|| ControllerError::InvalidState("child not spawned"))?;

        #[cfg(all(unix, feature = "zero_copy"))]
        let mut transport: Box<dyn IpcTransport> = if let Some(path) = self
            .config
            .as_ref()
            .and_then(|c| c.unix_socket_path.clone())
        {
            match rt_handle.block_on(UnixSocketTransport::connect(&path)) {
                Ok(sock) => Box::new(sock),
                Err(e) => {
                    return Err(ControllerError::BackendMsg(format!(
                        "unix socket connect failed: {e}"
                    )));
                }
            }
        } else {
            let stdout = child
                .stdout
                .take()
                .ok_or_else(|| ControllerError::Backend("child stdout not available"))?;
            let stdin = child
                .stdin
                .take()
                .ok_or_else(|| ControllerError::Backend("child stdin not available"))?;
            Box::new(StdIoTransport::new(stdout, stdin))
        };

        #[cfg(not(all(unix, feature = "zero_copy")))]
        let mut transport: Box<dyn IpcTransport> = {
            let stdout = child
                .stdout
                .take()
                .ok_or_else(|| ControllerError::Backend("child stdout not available"))?;
            let stdin = child
                .stdin
                .take()
                .ok_or_else(|| ControllerError::Backend("child stdin not available"))?;
            Box::new(StdIoTransport::new(stdout, stdin))
        };

        // Channel to feed outbound payloads and close signal.
        let (tx, mut rx) = mpsc::channel::<BgCommand>(128);
        self.tx = Some(tx);

        let shared = Arc::clone(&self.shared);
        let alive = Arc::clone(&self.alive);
        let event_sink = Arc::clone(&self.event_sink);
        #[cfg(all(unix, feature = "zero_copy"))]
        let pending_handles = Arc::clone(&self.pending_handles);
        #[cfg(all(unix, feature = "zero_copy"))]
        let last_frame_valid = Arc::clone(&self.last_frame_valid);
        #[cfg(all(unix, feature = "zero_copy"))]
        let last_frame_bytes = Arc::clone(&self.last_frame_bytes);

        // Background transport task: multiplex sending and receiving.
        let bg = self.take_rt()?.spawn(async move {
            loop {
                select! {
                    // Receive next incoming frame
                    inbound = async {
                        #[cfg(all(unix, feature = "zero_copy"))]
                        {
                            if transport.supports_handle_passing() {
                                match transport.next_frame_with_handles().await {
                                    Ok((bytes, handles)) => Ok((bytes, Some(handles))),
                                    Err(e) => Err(e),
                                }
                            } else {
                                match transport.next_frame().await {
                                    Ok(bytes) => Ok((bytes, None::<Vec<crate::ipc_transport::OpaqueHandle>>)),
                                    Err(e) => Err(e),
                                }
                            }
                        }
                        #[cfg(not(all(unix, feature = "zero_copy")))]
                        {
                            match transport.next_frame().await {
                                Ok(bytes) => Ok((bytes, None::<Vec<()>>)),
                                Err(e) => Err(e),
                            }
                        }
                    } => {
                        match inbound {
                            Ok((bytes, handles_opt)) => {
                                #[cfg(not(all(unix, feature = "zero_copy")))]
                                let _ = &handles_opt;
                                // Deserialize envelope and route if Response
                                match bincode::serde::decode_from_slice::<proto::Envelope, _>(&bytes, bincode::config::standard()) {
                                    Ok((env, _)) => {
                                        match env.message {
                                            proto::Message::Response(resp) => {
                                                // Route response by in_reply_to
                                                let key = match &resp {
                                                    proto::Response::Ack { in_reply_to } => *in_reply_to,
                                                    proto::Response::Error { in_reply_to, .. } => *in_reply_to,
                                                    proto::Response::InitAck { in_reply_to, .. } => *in_reply_to,
                                                    proto::Response::InvokeResult { in_reply_to, .. } => *in_reply_to,
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
                                                        // Correlate any received FDs with HandleTokens in the descriptor (unix + zero_copy)
                                                        #[cfg(all(unix, feature = "zero_copy"))]
                                                        {
                                                            if let Some(mut handles) = handles_opt {
                                                                // Collect handle tokens in descriptor (order matters for naive mapping).
                                                                let mut tokens: Vec<u64> = Vec::new();
                                                                match &descriptor {
                                                                    proto::FrameDescriptor::LinuxDmabuf { planes, fence, .. } => {
                                                                        for p in planes {
                                                                            tokens.push(p.fd.0);
                                                                        }
                                                                        if let Some(f) = fence {
                                                                            tokens.push(f.0);
                                                                        }
                                                                    }
                                                                    proto::FrameDescriptor::MacOsIoSurface { io_surface, .. } => {
                                                                        tokens.push(io_surface.0);
                                                                    }
                                                                    proto::FrameDescriptor::WindowsDxgiSharedHandle { handle, .. } => {
                                                                        tokens.push(handle.0);
                                                                    }
                                                                    proto::FrameDescriptor::SharedMemoryImage { shm, .. } => {
                                                                        tokens.push(shm.0);
                                                                    }
                                                                    proto::FrameDescriptor::CompressedImage { payload, .. } => {
                                                                        if let proto::PayloadLocator::Handle(h) = payload {
                                                                            tokens.push(h.0);
                                                                        }
                                                                    }
                                                                }
                                                                // Store mapping: single token -> all handles, else zip in order.
                                                                let mut map = pending_handles.lock().unwrap();
                                                                if tokens.len() <= 1 {
                                                                    if let Some(tok) = tokens.get(0) {
                                                                        map.insert(*tok, handles);
                                                                    }
                                                                } else {
                                                                    for (i, tok) in tokens.into_iter().enumerate() {
                                                                        if i < handles.len() {
                                                                            map.insert(tok, vec![handles[i]]);
                                                                        }
                                                                    }
                                                                }
                                                            }
                                                        }
                                                        // Auto-consume SharedMemoryImage for demo verification (unix + zero_copy).
                                                        #[cfg(all(unix, feature = "zero_copy"))]
                                                        {
                                                            if let proto::FrameDescriptor::SharedMemoryImage { width, height, stride, format, shm, size } = descriptor.clone() {
                                                                if matches!(format, proto::PixelFormat::Rgba8888) {
                                                                    // Try to remove the handle mapping now to take ownership.
                                                                    if let Some(handles) = {
                                                                        let mut map = pending_handles.lock().unwrap();
                                                                        map.remove(&shm.0)
                                                                    } {
                                                                        if let Some(fd) = handles.first() {
                                                                            let len = size as usize;
                                                                            // Map and verify in a blocking task.
                                                                            if len > 0 {
                                                                                let lfv = last_frame_valid.clone();
                                                                                let lfb = last_frame_bytes.clone();
                                                                                let fd_u64 = fd.0;
                                                                                let _ = tokio::task::spawn_blocking(move || {
                                                                                    // SAFETY: fd received from SCM_RIGHTS, valid for mapping.
                                                                                    let fd_i32 = fd_u64 as i32;
                                                                                    // Duplicate to avoid affecting original ownership during File::from_raw_fd.
                                                                                    let dup = match nix::unistd::dup(fd_i32) {
                                                                                        Ok(v) => v,
                                                                                        Err(_) => {
                                                                                            lfv.store(false, Ordering::SeqCst);
                                                                                            let mut guard = lfb.lock().unwrap();
                                                                                            *guard = None;
                                                                                            return;
                                                                                        }
                                                                                    };
                                                                                    let file = unsafe { <std::fs::File as std::os::unix::io::FromRawFd>::from_raw_fd(dup) };
                                                                                    let mmap = unsafe { memmap2::MmapOptions::new().len(len).map(&file) };
                                                                                    match mmap {
                                                                                        Ok(m) => {
                                                                                            let stride_usize = stride as usize;
                                                                                            let mut ok = true;
                                                                                            'outer: for y in 0..(height as usize) {
                                                                                                for x in 0..(width as usize) {
                                                                                                    let idx = y * stride_usize + x * 4;
                                                                                                    if m[idx] != x as u8 || m[idx + 1] != y as u8 || m[idx + 2] != 0x80 || m[idx + 3] != 0xFF {
                                                                                                        ok = false;
                                                                                                        break 'outer;
                                                                                                    }
                                                                                                }
                                                                                            }
                                                                                            lfv.store(ok, Ordering::SeqCst);
                                                                                            let mut guard = lfb.lock().unwrap();
                                                                                            if ok {
                                                                                                *guard = Some(m.to_vec());
                                                                                            } else {
                                                                                                *guard = None;
                                                                                            }
                                                                                        }
                                                                                        Err(_) => {
                                                                                            lfv.store(false, Ordering::SeqCst);
                                                                                            let mut guard = lfb.lock().unwrap();
                                                                                            *guard = None;
                                                                                        }
                                                                                    }
                                                                                }).map(|_| ());
                                                                            }
                                                                        }
                                                                    }
                                                                }
                                                            }
                                                        }
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

    #[cfg(all(unix, feature = "zero_copy"))]
    /// Consume a pending handle by its token, transferring ownership to the caller.
    /// After this returns Ok, the token is removed from the internal map and cannot be taken again.
    pub fn consume_handle(&mut self, token: u64) -> Result<ReceivedHandle, ControllerError> {
        let handles = {
            let mut map = self
                .pending_handles
                .lock()
                .expect("pending_handles poisoned");
            map.remove(&token)
        }
        .ok_or_else(|| ControllerError::InvalidState("handle token not found"))?;
        ReceivedHandle::from_opaque(handles)
            .ok_or_else(|| ControllerError::InvalidState("no handles attached for token"))
    }

    #[cfg(all(unix, feature = "zero_copy"))]
    /// Return a snapshot of currently pending handle tokens (for debugging/tests).
    pub fn debug_list_pending_handle_tokens(&self) -> Vec<u64> {
        let map = self
            .pending_handles
            .lock()
            .expect("pending_handles poisoned");
        map.keys().copied().collect()
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

    #[cfg(all(unix, feature = "zero_copy"))]
    pub fn last_frame_valid(&self) -> bool {
        self.last_frame_valid.load(Ordering::SeqCst)
    }

    #[cfg(all(unix, feature = "zero_copy"))]
    pub fn take_last_frame_bytes(&mut self) -> Option<Vec<u8>> {
        self.last_frame_bytes.lock().unwrap().take()
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
