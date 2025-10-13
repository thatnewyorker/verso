#![cfg(feature = "versoview-runtime")]
/*!
Versioned IPC protocol definitions for the Verso ↔ versoview controller link.

Overview:
- Transport-agnostic, serde-serializable messages intended to be framed with a
  length-prefix (e.g., u32 LE) over stdio, domain sockets, or named pipes.
- A single `Envelope` wraps a version, correlation ID, flags, and a typed `Message`.
- Requests, responses, and events are distinguished at the message level.
- Handshake negotiates a chosen protocol version and advertises capabilities.
- Input events support batching/coalescing.
- Frame sharing uses a portable `FrameDescriptor` with platform-specific variants.

Notes:
- This module is gated behind the `versoview-runtime` feature. The crate enables
  `serde` (optional dependency) when this feature is active.
- Binary encoding is recommended for hot paths (e.g., via `bincode` or `serde_cbor`).
*/

use serde::{Deserialize, Serialize};

/// Current protocol version supported by this library.
pub const PROTOCOL_VERSION: u16 = 1;

/// Minimum protocol version supported (for range negotiation).
pub const PROTOCOL_MIN_VERSION: u16 = 1;

/// Maximum protocol version supported (for range negotiation).
pub const PROTOCOL_MAX_VERSION: u16 = PROTOCOL_VERSION;

/// A compact, versioned message envelope.
///
/// Framing suggestion:
/// - Prepend a u32 length (LE) to the serialized bytes to form a single frame on the wire.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Envelope {
    /// Protocol version of this message.
    pub version: u16,
    /// Correlation ID used to match requests and replies.
    pub id: u64,
    /// Optional flags for future use (bitfield).
    pub flags: u32,
    /// The typed message payload.
    pub message: Message,
}

/// Directional message categories sent over the wire.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]

pub enum Message {
    Request(Request),
    Response(Response),
    Event(Event),
}

/// Requests sent by the client (host) to the server (versoview).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]

pub enum Request {
    /// Initial handshake from the client to the server.
    Init {
        /// Identifies the client for diagnostics.
        client_name: String,
        /// Supported protocol version range.
        supported: VersionRange,
        /// Client configuration (subset required for server setup).
        config: ControllerConfig,
    },

    /// Binds the renderer to a native surface (platform-agnostic descriptor).
    BindSurface { surface: SurfaceDescriptor },

    /// Navigate to a URL or load a resource.
    Load { url: String },

    /// Evaluate JavaScript in the page context.
    ///
    /// A `request_id` can be supplied to correlate async results if the server
    /// chooses to return specific replies or events for this evaluation.
    EvalScript {
        source: String,
        request_id: Option<u64>,
    },

    /// Notify a size change (physical pixels).
    Resize { width: u32, height: u32 },

    /// Request a draw on the next frame.
    RequestDraw,

    /// Request an immediate draw/present if supported.
    DrawNow,

    /// One or more coalesced/batched input events.
    InputEvent { events: Vec<IpcInputEvent> },

    /// Request graceful shutdown.
    Shutdown,
}

/// Responses sent by the server (versoview) in reply to requests.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]

pub enum Response {
    /// Acknowledgement for a request with no extra payload.
    Ack { in_reply_to: u64 },

    /// Error reply with a code and message.
    Error {
        in_reply_to: u64,
        code: ErrorCode,
        message: String,
    },

    /// Reply to `Init` request with the chosen protocol version and capabilities.
    InitAck {
        in_reply_to: u64,
        chosen_version: u16,
        server_name: String,
        capabilities: Capabilities,
    },
}

/// Asynchronous events sent by the server (versoview) to the client (host).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]

pub enum Event {
    /// A new frame is ready to be presented by the host.
    FrameReady {
        frame_id: u64,
        descriptor: FrameDescriptor,
    },

    /// Navigation lifecycle notifications.
    Navigation {
        status: NavigationStatus,
        url: Option<String>,
        http_status: Option<u16>,
    },

    /// Console output from the page or engine.
    ConsoleMessage {
        level: ConsoleLevel,
        message: String,
        source: Option<String>,
        line: Option<u32>,
        column: Option<u32>,
    },

    /// DevTools port opened and available for connections.
    DevToolsPortOpened {
        port: u16,
        /// Optional convenient URL (e.g., "http://127.0.0.1:PORT/")
        url: Option<String>,
    },
}

/// Protocol version range advertised during handshake.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct VersionRange {
    pub min: u16,
    pub max: u16,
}

impl Default for VersionRange {
    fn default() -> Self {
        Self {
            min: PROTOCOL_MIN_VERSION,
            max: PROTOCOL_MAX_VERSION,
        }
    }
}

/// Subset of controller configuration relevant to the server initialization.
///
/// This mirrors, but intentionally does not depend on, the public `ControllerConfig`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ControllerConfig {
    /// Optional resources directory (assets, preload scripts, etc.).
    pub resources_dir: Option<PathString>,
    /// Optional devtools port; 0 to request a random port.
    pub devtools_port: Option<u16>,
    /// Optional user agent override.
    pub user_agent: Option<String>,
    /// Optional initialization scripts to inject early in page lifecycle.
    pub init_scripts: Vec<String>,
    /// Optional limits and tuning knobs for IPC batching/backpressure.
    pub ipc_tuning: Option<IpcTuning>,
}

/// IPC tuning knobs for batching and backpressure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct IpcTuning {
    /// Maximum number of unacknowledged in-flight requests before backpressure.
    pub max_in_flight: u16,
    /// Coalescing window in microseconds for high-frequency inputs.
    pub coalesce_window_us: u32,
}

/// Portable representation of a native surface or a render target binding.
///
/// For zero-copy GPU paths, further out-of-band handles (FDs, mach ports, etc.)
/// may be required depending on the transport used; those should be coordinated
/// by higher-level transport extensions or follow-up messages.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]

pub enum SurfaceDescriptor {
    /// Wayland surface. `surface_id` is an opaque token negotiated by the host/server.
    Wayland {
        surface_id: HandleToken,
        display_id: Option<HandleToken>,
    },
    /// X11 window.
    X11 {
        window: u64,
        display: Option<HandleToken>,
    },
    /// Windows HWND/HMONITOR pairing.
    Windows { hwnd: u64, hmonitor: Option<u64> },
    /// macOS NSView/NSWindow pairing via opaque tokens.
    MacOs {
        ns_view: u64,
        ns_window: Option<u64>,
    },
    /// Fallback descriptor when a native handle cannot be sent; e.g., logical size only.
    Fallback {
        width: u32,
        height: u32,
        scale_factor: f32,
    },
}

/// A batch-friendly input event format for IPC.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]

pub enum IpcInputEvent {
    MouseMove {
        x: f64,
        y: f64,
        timestamp_us: Option<u64>,
    },
    MouseButton {
        button: MouseButton,
        state: ButtonState,
        timestamp_us: Option<u64>,
    },
    Scroll {
        delta_x: f32,
        delta_y: f32,
        is_precise: bool,
        timestamp_us: Option<u64>,
    },
    KeyDown {
        key: String,
        timestamp_us: Option<u64>,
    },
    KeyUp {
        key: String,
        timestamp_us: Option<u64>,
    },
    CharInput {
        ch: char,
        timestamp_us: Option<u64>,
    },
    FocusChanged {
        focused: bool,
        timestamp_us: Option<u64>,
    },
    PointerEntered {
        timestamp_us: Option<u64>,
    },
    PointerLeft {
        timestamp_us: Option<u64>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MouseButton {
    Left,
    Middle,
    Right,
    Other(u8),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ButtonState {
    Pressed,
    Released,
}

/// Capability flags and advertised options from the server.
///
/// Semantics:
/// - `fd_passing`: The transport supports passing OS/native handles out-of-band (e.g., SCM_RIGHTS on
///   Unix, mach ports/IOSurface on macOS, duplicated HANDLE/DXGI shared handles on Windows). When true,
///   the server may attach one or more handles to messages and expect the client to import them.
/// - `zero_copy_frames`: The server can produce GPU-/kernel-backed frame resources and advertise them via
///   `Event::FrameReady` with a `FrameDescriptor` containing a `HandleToken`. The actual OS handle(s) are
///   delivered by the transport alongside the serialized envelope.
/// - `compressed_frames`: The server can send compressed image fallbacks (e.g., PNG/JPEG/WEBP) when
///   zero-copy is unavailable or not negotiated.
/// - `supported_compressed`: List of compressed formats the server may emit for fallbacks.
/// - `extra`: Reserved for forward-compatibility feature flags.
///
/// Negotiation:
/// - During `Init`/`InitAck`, the client and server intersect capabilities. If either side does not support
///   `fd_passing` or `zero_copy_frames`, the client should fall back to compressed frames when available.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Capabilities {
    /// Server supports passing file descriptors/ancillary handles.
    pub fd_passing: bool,
    /// Server supports zero-copy frame sharing (see `FrameDescriptor`).
    pub zero_copy_frames: bool,
    /// Server supports compressed bitmap fallback.
    pub compressed_frames: bool,
    /// Supported compressed formats for fallback paths.
    pub supported_compressed: Vec<CompressedFormat>,
    /// Additional named capabilities for forward-compatibility.
    pub extra: Vec<String>,
}

/// Frame descriptor for presenting frames with zero-copy or fallback paths.
///
/// Platform variants carry the minimal information needed for the host to
/// import and present the frame. Synchronization (fences/semaphores) can be
/// included via optional handle tokens if supported by the platform.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]

pub enum FrameDescriptor {
    /// Linux dmabuf-based import.
    LinuxDmabuf {
        width: u32,
        height: u32,
        fourcc: u32,
        modifier: u64,
        planes: Vec<DmabufPlane>,
        /// Optional fence/sync FD token.
        fence: Option<HandleToken>,
    },

    /// macOS IOSurface-based import.
    MacOsIoSurface {
        width: u32,
        height: u32,
        pixel_format: u32,
        /// IOSurface ID or mach port token (opaque to the host).
        io_surface: HandleToken,
    },

    /// Windows DXGI shared handle import.
    WindowsDxgiSharedHandle {
        width: u32,
        height: u32,
        dxgi_format: u32,
        /// Opaque shared handle value (duplicated to the host).
        handle: HandleToken,
    },

    /// Shared-memory bitmap fallback (uncompressed).
    SharedMemoryImage {
        width: u32,
        height: u32,
        stride: u32,
        format: PixelFormat,
        /// Handle token identifying the shared memory segment.
        shm: HandleToken,
        /// Total byte size of the buffer.
        size: u64,
    },

    /// Compressed bitmap fallback.
    CompressedImage {
        width: u32,
        height: u32,
        format: CompressedFormat,
        /// The compressed payload as an out-of-band handle or inline bytes.
        ///
        /// For transports without FD/handle passing, small images may be sent inline.
        payload: PayloadLocator,
    },
}

/// A single plane of a dmabuf image.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DmabufPlane {
    pub fd: HandleToken,
    pub offset: u32,
    pub stride: u32,
    pub plane_index: u8,
}

/// Pixel format for uncompressed bitmaps.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PixelFormat {
    Rgba8888,
    Bgra8888,
    Rgbx8888,
    Bgrx8888,
    Rg16f,
    Rgba16f,
    Unknown(u32),
}

/// Compressed image formats for fallback frames.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CompressedFormat {
    Png,
    Webp,
    Jpeg,
    Avif,
    /// A vendor/codec-specific format identified by a numeric code.
    Other(u32),
}

/// Where to find the payload bytes for a frame or resource.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]

pub enum PayloadLocator {
    /// Inline bytes (use sparingly; prefer handle-based for large payloads).
    Inline(Vec<u8>),
    /// Opaque handle token to a shared resource.
    Handle(HandleToken),
}

/// Navigation lifecycle states.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum NavigationStatus {
    Started,
    Committed,
    Finished,
    Failed,
}

/// Console log level.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ConsoleLevel {
    Debug,
    Info,
    Warn,
    Error,
}

/// Error codes used in `Response::Error`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ErrorCode {
    /// The request is not valid in the current state.
    InvalidState,
    /// The request is not recognized or not implemented.
    Unsupported,
    /// The message failed validation or decoding.
    BadMessage,
    /// The surface could not be created or bound.
    SurfaceError,
    /// An internal error occurred in the server.
    Internal,
}

/// A portable handle token used to correlate an OS/native handle with a serialized message.
///
/// Rules:
/// - The token is an opaque identifier meaningful only within the lifetime of a transport session.
/// - The actual handle is transmitted out-of-band by the transport (e.g., SCM_RIGHTS, mach ports,
///   DXGI shared handles) in the same logical operation as the serialized envelope that carries
///   this token.
/// - The token itself never encodes OS handle data; it is only used to match the received handle(s)
///   to a specific `FrameDescriptor` or message.
/// - Ownership transfer: unless the protocol version or message specifies otherwise, receivers take
///   ownership of transferred handles and are responsible for closing/freeing them when done.
/// - Lifetime: tokens must not be reused until prior resources associated with the token have been
///   released, to avoid accidental aliasing.
///
/// Platform examples:
/// - POSIX FDs (Linux/BSD) via SCM_RIGHTS for dmabuf or shared memory.
/// - macOS IOSurface IDs or mach ports.
/// - Windows duplicated HANDLEs or DXGI shared handles.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct HandleToken(pub u64);

/// A UTF-8 path string wrapper for OS-agnostic serialization.
///
/// For platforms with non-UTF-8 paths, the path should be pre-encoded or
/// referenced via a `HandleToken` instead of using this type.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PathString(pub String);

impl From<String> for PathString {
    fn from(s: String) -> Self {
        PathString(s)
    }
}

impl From<&str> for PathString {
    fn from(s: &str) -> Self {
        PathString(s.to_owned())
    }
}
