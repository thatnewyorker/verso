/*!
Controller abstraction for Verso webviews.

This module defines a minimal, framework-agnostic controller API that an embedder
(or this crate) can implement to host a Servo-based webview either in-process or
out-of-process. It also includes a simple `MockWebviewController` you can use in
tests or to validate integration wiring before a real controller is available.

Design goals:
- Small, explicit trait surface suitable for both in-process and IPC-backed controllers.
- Clear configuration and error types.
- Threading model: by default, treat calls as event-loop-thread-affine unless a
  given implementation documents otherwise.
- No external framework coupling; works with `winit` and raw window/display handles.

Notes on threading and lifetimes:
- Implementations may require all methods to be called on the event-loop (UI) thread.
  If that is the case, document and enforce it in your implementation (e.g., by checking
  thread IDs or providing a message queue).
- `bind_surface` receives `NativeSurfaceHandles` that encapsulate raw handles to a native
  window and (optionally) a native display. Implementations must not retain or use these
  handles after the associated native window is destroyed. Document any additional safety
  invariants in unsafe blocks where needed (not present in this mock implementation).

Feature gating:
- You can feature-gate real controller implementations and keep this trait available
  unconditionally so consumers can compile without a Servo/versoview dependency.
*/

use std::fmt;
use std::path::PathBuf;

/// Reuse the surface handle abstraction from the winit runtime glue.
use crate::winit_runtime::NativeSurfaceHandles;

/// Physical pixel size of the rendering target.
use winit::dpi::PhysicalSize;

/// Configuration used to initialize a controller instance.
#[derive(Debug, Clone, Default)]
pub struct ControllerConfig {
    /// Selects which controller backend to use, if your application supports multiple.
    pub mode: ControllerMode,
    /// Optional path to an external `versoview` binary (used in out-of-process mode).
    pub external_bin_path: Option<PathBuf>,
    /// Optional resources directory (assets, preload scripts, etc.).
    pub resources_dir: Option<PathBuf>,
    /// Optional devtools port; use 0 to request a random available port.
    pub devtools_port: Option<u16>,
    /// Optional user agent override.
    pub user_agent: Option<String>,
    /// Optional initialization scripts to inject early in page lifecycle.
    pub init_scripts: Vec<String>,
}

/// Selects the controller backend to use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControllerMode {
    /// Embed Servo directly in-process.
    InProcess,
    /// Drive an external `versoview` process via IPC.
    OutOfProcess,
}

impl Default for ControllerMode {
    fn default() -> Self {
        ControllerMode::InProcess
    }
}

/// Errors produced by controller operations.
///
/// Map these to higher-level runtime errors as needed (e.g., `WinitRuntimeError`).
#[derive(Debug)]
pub enum ControllerError {
    /// The controller has not been initialized.
    NotInitialized(&'static str),
    /// A native surface has not been bound yet.
    SurfaceNotBound,
    /// The controller is not in a state that allows the requested operation.
    InvalidState(&'static str),
    /// An operation is not supported by this controller.
    Unsupported(&'static str),
    /// Underlying platform/graphics/IPC error with a short message.
    Backend(&'static str),
    /// Underlying platform/graphics/IPC error with an owned message.
    BackendMsg(String),
}

impl fmt::Display for ControllerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ControllerError::NotInitialized(what) => write!(f, "not initialized: {what}"),
            ControllerError::SurfaceNotBound => write!(f, "surface not bound"),
            ControllerError::InvalidState(what) => write!(f, "invalid state: {what}"),
            ControllerError::Unsupported(what) => write!(f, "unsupported: {what}"),
            ControllerError::Backend(msg) => write!(f, "backend error: {msg}"),
            ControllerError::BackendMsg(msg) => write!(f, "backend error: {msg}"),
        }
    }
}

impl std::error::Error for ControllerError {}

/// Simplified input event representation forwarded from the embedder to the controller.
///
/// Implementations can choose to expand this or translate from a richer event model.
#[derive(Debug, Clone, PartialEq)]
pub enum InputEvent {
    /// Cursor moved in logical pixels within the surface.
    MouseMove {
        x: f64,
        y: f64,
    },
    /// Mouse button pressed/released.
    MouseButton {
        button: u8,
        state: ButtonState,
    },
    /// Wheel/trackpad scroll. Positive values typically indicate down/right.
    Scroll {
        delta_x: f32,
        delta_y: f32,
        is_precise: bool,
    },
    /// Keyboard key down (named key or simplified code).
    KeyDown {
        key: &'static str,
    },
    /// Keyboard key up (named key or simplified code).
    KeyUp {
        key: &'static str,
    },
    /// Text input (composed characters).
    CharInput {
        ch: char,
    },
    /// Focus changed for the view.
    FocusChanged {
        focused: bool,
    },
    /// Pointer entered/leaved the surface.
    PointerEntered,
    PointerLeft,
}

/// Button press/release state for pointer events.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ButtonState {
    Pressed,
    Released,
}

/// Asynchronous events that controllers can emit to the host.
#[derive(Debug, Clone, PartialEq)]
pub enum ControllerEvent {
    /// Navigation lifecycle notifications.
    Navigation {
        status: NavigationStatus,
        url: Option<String>,
        http_status: Option<u16>,
    },
    /// Console output from the page or engine.
    Console {
        level: ConsoleLevel,
        message: String,
        source: Option<String>,
        line: Option<u32>,
        column: Option<u32>,
    },
    /// DevTools port opened and available for connections.
    DevToolsPortOpened { port: u16, url: Option<String> },
    /// A new frame is ready to be presented (portable placeholder).
    ///
    /// Backends with zero-copy sharing should expose platform details elsewhere.
    FrameReady {
        width: u32,
        height: u32,
        /// Opaque token or identifier for the frame (backend-defined).
        frame_id: u64,
    },
}

/// Navigation lifecycle states.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NavigationStatus {
    Started,
    Committed,
    Finished,
    Failed,
}

/// Console log level.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConsoleLevel {
    Debug,
    Info,
    Warn,
    Error,
}

/// Callback type for receiving controller events.
///
/// The sink must be callable from background threads.
pub type ControllerEventSink = Box<dyn Fn(ControllerEvent) + Send + 'static>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
/// Negotiated controller capabilities exposed to the host.
///
/// Notes:
/// - These reflect the effective (negotiated) capabilities after handshake, if applicable.
/// - Backends that do not perform negotiation may return defaults or `None`.
pub struct ControllerCapabilities {
    /// Transport supports passing OS/native handles (e.g., SCM_RIGHTS, mach ports, DXGI handles).
    pub fd_passing: bool,
    /// Renderer can produce GPU-/kernel-backed frames for zero-copy import/presentation.
    pub zero_copy_frames: bool,
    /// Renderer can provide compressed-image fallbacks (e.g., PNG/JPEG/WEBP).
    pub compressed_frames: bool,
}

/// Trait that all controller backends must implement.
///
/// Threading:
/// - Unless otherwise documented, methods are expected to be called on the event-loop thread.
/// - Implementations that are thread-safe should document their guarantees and may accept calls
///   from multiple threads (e.g., by internally queueing work to a dedicated thread).
pub trait WebviewController: Send {
    /// Initialize the controller with static configuration.
    ///
    /// Call this once before binding a surface or loading content.
    fn initialize(&mut self, config: ControllerConfig) -> Result<(), ControllerError>;

    /// Bind to a native rendering surface using raw handles.
    ///
    /// Safety and lifetime: the implementation must not use these handles after the
    /// associated window has been destroyed. It is recommended to document any additional
    /// safety invariants in the implementation.
    fn bind_surface(&mut self, handles: NativeSurfaceHandles) -> Result<(), ControllerError>;

    /// Navigate to a URL or load a resource.
    fn load(&mut self, url_or_resource: &str) -> Result<(), ControllerError>;

    /// Evaluate JavaScript in the page context.
    ///
    /// Implementations may choose to execute synchronously or queue work for the next frame.
    fn eval_script(&mut self, source: &str) -> Result<(), ControllerError>;

    /// Notify the controller of a new physical pixel size.
    fn resize(&mut self, size: PhysicalSize<u32>) -> Result<(), ControllerError>;

    /// Request a draw on the next frame (if supported).
    fn request_draw(&mut self) -> Result<(), ControllerError>;

    /// Draw/present immediately (if supported by the backend).
    fn draw(&mut self) -> Result<(), ControllerError>;

    /// Forward a single input event to the webview.
    fn send_input(&mut self, event: InputEvent) -> Result<(), ControllerError>;

    /// Show/hide the webview, if supported.
    fn set_visibility(&mut self, visible: bool) -> Result<(), ControllerError>;

    /// Request input focus for the webview, if supported.
    fn focus(&mut self) -> Result<(), ControllerError>;

    /// Install an event sink to receive asynchronous controller events.
    ///
    /// Default: returns `Unsupported` for backends that do not produce events.
    fn set_event_sink(&mut self, _sink: ControllerEventSink) -> Result<(), ControllerError> {
        Err(ControllerError::Unsupported("event sink not supported"))
    }

    /// Returns negotiated capabilities if available (e.g., after an IPC handshake), or `None`
    /// if the backend does not perform capability negotiation.
    ///
    /// Default: `None` for backends that do not negotiate capabilities.
    fn capabilities(&self) -> Option<ControllerCapabilities> {
        None
    }

    /// Returns true if the controller is alive and operational.
    fn is_alive(&self) -> bool;

    /// Shut down the controller and free resources.
    fn shutdown(&mut self) -> Result<(), ControllerError>;
}

/// A minimal, no-op controller implementation useful for tests and early wiring.
///
/// Behavior:
/// - Tracks state transitions and simple counters.
/// - Validates call ordering: requires `initialize` then `bind_surface` before most operations.
/// - Does not actually render or communicate with a browser engine.
/// - Stores the last loaded URL and last script for inspection in tests.
///
/// Threading:
/// - Methods take `&mut self` and the type is `Send`; use from a single thread in tests.
///   Wrap in a `Mutex` if sharing between threads.
#[derive(Default)]
pub struct MockWebviewController {
    initialized: bool,
    surface_bound: bool,
    alive: bool,
    visible: bool,
    last_url: Option<String>,
    last_script: Option<String>,
    last_size: Option<PhysicalSize<u32>>,
    input_events: usize,
    draws_requested: usize,
    draws_performed: usize,
    config: Option<ControllerConfig>,
    event_sink: Option<ControllerEventSink>,
}

impl MockWebviewController {
    /// Inspect the number of input events received (for testing).
    pub fn input_event_count(&self) -> usize {
        self.input_events
    }

    /// Inspect how many times draw was requested (for testing).
    pub fn draws_requested(&self) -> usize {
        self.draws_requested
    }

    /// Inspect how many draws were performed (for testing).
    pub fn draws_performed(&self) -> usize {
        self.draws_performed
    }

    /// Inspect the last URL loaded (for testing).
    pub fn last_url(&self) -> Option<&str> {
        self.last_url.as_deref()
    }

    /// Inspect the last evaluated script (for testing).
    pub fn last_script(&self) -> Option<&str> {
        self.last_script.as_deref()
    }

    /// Inspect last known size (for testing).
    pub fn last_size(&self) -> Option<PhysicalSize<u32>> {
        self.last_size
    }

    /// Inspect config (for testing).
    pub fn config(&self) -> Option<&ControllerConfig> {
        self.config.as_ref()
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
        if !self.surface_bound {
            return Err(ControllerError::SurfaceNotBound);
        }
        if !self.alive {
            return Err(ControllerError::InvalidState("controller not alive"));
        }
        Ok(())
    }
}

impl WebviewController for MockWebviewController {
    fn initialize(&mut self, config: ControllerConfig) -> Result<(), ControllerError> {
        if self.initialized {
            return Err(ControllerError::InvalidState(
                "initialize() called more than once",
            ));
        }
        self.config = Some(config);
        self.initialized = true;
        self.alive = true;
        Ok(())
    }

    fn bind_surface(&mut self, _handles: NativeSurfaceHandles) -> Result<(), ControllerError> {
        self.ensure_initialized()?;
        if self.surface_bound {
            return Err(ControllerError::InvalidState(
                "bind_surface() called more than once",
            ));
        }
        self.surface_bound = true;
        Ok(())
    }

    fn load(&mut self, url_or_resource: &str) -> Result<(), ControllerError> {
        self.ensure_ready()?;
        self.last_url = Some(url_or_resource.to_string());
        if let Some(cb) = &self.event_sink {
            cb(ControllerEvent::Navigation {
                status: NavigationStatus::Committed,
                url: Some(url_or_resource.to_string()),
                http_status: Some(200),
            });
        }
        Ok(())
    }

    fn eval_script(&mut self, source: &str) -> Result<(), ControllerError> {
        self.ensure_ready()?;
        self.last_script = Some(source.to_string());
        if let Some(cb) = &self.event_sink {
            cb(ControllerEvent::Console {
                level: ConsoleLevel::Debug,
                message: "eval_script executed".to_string(),
                source: Some("mock".to_string()),
                line: None,
                column: None,
            });
        }
        Ok(())
    }

    fn resize(&mut self, size: PhysicalSize<u32>) -> Result<(), ControllerError> {
        self.ensure_ready()?;
        self.last_size = Some(size);
        Ok(())
    }

    fn request_draw(&mut self) -> Result<(), ControllerError> {
        self.ensure_ready()?;
        self.draws_requested += 1;
        Ok(())
    }

    fn draw(&mut self) -> Result<(), ControllerError> {
        self.ensure_ready()?;
        self.draws_performed += 1;
        Ok(())
    }

    fn send_input(&mut self, _event: InputEvent) -> Result<(), ControllerError> {
        self.ensure_ready()?;
        self.input_events += 1;
        Ok(())
    }

    fn set_visibility(&mut self, visible: bool) -> Result<(), ControllerError> {
        self.ensure_ready()?;
        self.visible = visible;
        Ok(())
    }

    fn focus(&mut self) -> Result<(), ControllerError> {
        self.ensure_ready()?;
        Ok(())
    }

    fn set_event_sink(&mut self, sink: ControllerEventSink) -> Result<(), ControllerError> {
        self.event_sink = Some(sink);
        Ok(())
    }

    fn is_alive(&self) -> bool {
        self.alive
    }

    fn shutdown(&mut self) -> Result<(), ControllerError> {
        self.ensure_initialized()?;
        self.alive = false;
        Ok(())
    }
}
