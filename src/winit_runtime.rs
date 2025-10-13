/*!
Winit runtime integration for Verso (standalone).

This module implements a minimal, functional integration using the
`winit` 0.29 ApplicationHandler model:

- `WinitRuntime` manages an event loop proxy and a per-window subscriber registry.
- `WinitWindowHandle` is a lightweight identifier for windows (WindowId).
- `VersoWebviewHost` is a placeholder for the Servo/Verso integration.
- `WinitRuntime::run` drives the winit event loop via `ApplicationHandler`.
- `WinitRuntime::create_window_with` posts an internal command to create a window on
  the event loop thread and synchronously returns a `WinitWindowHandle`.

Notes:
- Rendering/webview binding is kept out of this file; the Verso host will be wired
  in a subsequent step.
- Window creation must occur on the event loop thread; requests are marshalled via an
  internal event and a blocking oneshot response channel (from a non-UI thread).
- If you call `create_window_with` from the same thread that runs `run`, you will deadlock.
  Spawn a worker thread or trigger creation in response to a winit event instead.
*/

use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, Mutex, mpsc};

use raw_window_handle::{RawDisplayHandle, RawWindowHandle};
use winit::application::ApplicationHandler;
use winit::event::{DeviceEvent, WindowEvent};
use winit::event_loop::{ActiveEventLoop, EventLoop, EventLoopProxy};
use winit::window::{CursorGrabMode, CursorIcon, Window, WindowAttributes, WindowId};

/// Publicly re-export common winit identifiers that downstream users often need.
pub use winit::dpi::{LogicalPosition, LogicalSize, PhysicalPosition, PhysicalSize};
pub use winit::keyboard::{Key, ModifiersState, NamedKey};

/// Errors that may be produced by the runtime layer.
#[derive(Debug)]
pub enum WinitRuntimeError {
    /// A requested window was not found in the runtime.
    UnknownWindow(WindowId),
    /// The runtime has not been initialized to the stage required by the call.
    NotInitialized(&'static str),
    /// A platform or backend specific error occurred (opaque for now).
    Backend(&'static str),
}

impl fmt::Display for WinitRuntimeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WinitRuntimeError::UnknownWindow(id) => write!(f, "unknown window: {:?}", id),
            WinitRuntimeError::NotInitialized(what) => write!(f, "not initialized: {what}"),
            WinitRuntimeError::Backend(msg) => write!(f, "backend error: {msg}"),
        }
    }
}

impl std::error::Error for WinitRuntimeError {}

/// Custom event type that wraps user-defined events alongside runtime-internal commands.
#[derive(Debug)]
pub enum RuntimeEvent<T = ()> {
    User(T),
    Internal(InternalCommand),
}

/// Internal runtime commands for window manipulation and redraw orchestration.
/// These will be handled by the runtime on the event loop thread.
#[derive(Debug)]
pub enum InternalCommand {
    // Creation
    CreateWindow {
        attrs: WindowAttributes,
        respond_to: mpsc::Sender<WindowId>,
    },

    // Per-window controls
    RequestRedraw(WindowId),
    SetTitle(WindowId, String),
    SetCursorIcon(WindowId, CursorIcon),
    SetCursorVisible(WindowId, bool),
    SetCursorGrab(WindowId, bool),
    CloseWindow(WindowId),
    // Future: Resize, Minimize, Fullscreen, etc.
}

/// A lightweight handle that identifies a window owned by the `WinitRuntime`.
///
/// Design notes:
/// - Store only the `WindowId`. All operations are dispatched via the runtime.
/// - This avoids sharing an actual `winit::window::Window` across threads or lifetimes.
/// - In the event loop, the runtime will map `WindowId` back to the real `Window`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct WinitWindowHandle {
    id: WindowId,
}

impl WinitWindowHandle {
    /// Create a new handle for a `WindowId`.
    pub fn new(id: WindowId) -> Self {
        Self { id }
    }

    /// Return the associated `WindowId`.
    pub fn id(&self) -> WindowId {
        self.id
    }

    /// Request a redraw for this window. This posts an internal event handled by the runtime.
    pub fn request_redraw<T>(&self, runtime: &WinitRuntime<T>) -> Result<(), WinitRuntimeError>
    where
        T: 'static + Send,
    {
        runtime.post_internal(InternalCommand::RequestRedraw(self.id))
    }

    /// Ask the runtime to set the window's title.
    pub fn set_title<T>(
        &self,
        runtime: &WinitRuntime<T>,
        title: impl Into<String>,
    ) -> Result<(), WinitRuntimeError>
    where
        T: 'static + Send,
    {
        runtime.post_internal(InternalCommand::SetTitle(self.id, title.into()))
    }

    /// Ask the runtime to adjust the cursor icon.
    pub fn set_cursor_icon<T>(
        &self,
        runtime: &WinitRuntime<T>,
        icon: CursorIcon,
    ) -> Result<(), WinitRuntimeError>
    where
        T: 'static + Send,
    {
        runtime.post_internal(InternalCommand::SetCursorIcon(self.id, icon))
    }

    /// Ask the runtime to show/hide the cursor.
    pub fn set_cursor_visible<T>(
        &self,
        runtime: &WinitRuntime<T>,
        visible: bool,
    ) -> Result<(), WinitRuntimeError>
    where
        T: 'static + Send,
    {
        runtime.post_internal(InternalCommand::SetCursorVisible(self.id, visible))
    }

    /// Ask the runtime to grab/ungrab the cursor.
    pub fn set_cursor_grab<T>(
        &self,
        runtime: &WinitRuntime<T>,
        grab: bool,
    ) -> Result<(), WinitRuntimeError>
    where
        T: 'static + Send,
    {
        runtime.post_internal(InternalCommand::SetCursorGrab(self.id, grab))
    }

    /// Ask the runtime to "close" this window (best-effort; platform semantics vary).
    pub fn close<T>(&self, runtime: &WinitRuntime<T>) -> Result<(), WinitRuntimeError>
    where
        T: 'static + Send,
    {
        runtime.post_internal(InternalCommand::CloseWindow(self.id))
    }
}

/// A subscriber that can receive window-scoped events from the runtime.
///
/// Register subscribers per window to receive `WindowEvent`s, `DeviceEvent`s,
/// and redraw notifications. Subscribers are invoked in registration order.
pub trait EventSubscriber<T = ()>: Send {
    /// Called for `winit::event::WindowEvent`.
    fn on_window_event(
        &mut self,
        _window: WinitWindowHandle,
        _event: &WindowEvent,
        _ctx: &mut DispatchContext<T>,
    ) {
    }

    /// Called for `winit::event::DeviceEvent`.
    ///
    /// Note: Device events are not window-scoped in winit; they are broadcast
    /// to all subscribers of all windows for convenience.
    fn on_device_event(&mut self, _event: &DeviceEvent, _ctx: &mut DispatchContext<T>) {}

    /// Called when `RedrawRequested` is emitted for the window.
    fn on_redraw_requested(&mut self, _window: WinitWindowHandle, _ctx: &mut DispatchContext<T>) {}

    /// Called when the runtime wants the subscriber to draw/present now.
    /// This may be used to integrate a renderer that prefers explicit present hooks.
    fn on_draw(&mut self, _window: WinitWindowHandle, _ctx: &mut DispatchContext<T>) {}
}

/// Mutable dispatch-time context passed to event subscribers.
pub struct DispatchContext<'a, T = ()> {
    runtime: &'a WinitRuntime<T>,
}

impl<'a, T> DispatchContext<'a, T> {
    pub fn runtime(&self) -> &WinitRuntime<T> {
        self.runtime
    }
}

/// Handle returned by `VersoWebviewHost::bind_native_surface` describing the
/// target to which Servo/Verso will render. Platform-specific details are represented
/// by `RawWindowHandle` + `RawDisplayHandle`.
#[derive(Debug, Clone, Copy)]
pub struct NativeSurfaceHandles {
    pub window: RawWindowHandle,
    pub display: Option<RawDisplayHandle>,
}

/// A placeholder for the Verso/Servo webview host bound to a `WinitWindowHandle`.
///
/// In a subsequent implementation pass, this type will own and drive a `verso::VersoviewController`
/// instance that renders into the native window represented by the handle.
pub struct VersoWebviewHost {
    window: WinitWindowHandle,
    // Future: store the Verso controller here.
    // controller: verso::VersoviewController,
}

impl VersoWebviewHost {
    /// Create a new host for the given window handle. You are expected to call
    /// `bind_native_surface` soon after to attach the Verso/Servo renderer.
    pub fn new(window: WinitWindowHandle) -> Self {
        Self { window }
    }

    /// The window this host is bound to.
    pub fn window(&self) -> WinitWindowHandle {
        self.window
    }

    /// Bind the Verso/Servo webview to the native surface described by the given handles.
    ///
    /// Later, this will call into `verso` to initialize the controller using these handles
    /// and any globally configured paths/resources (see crate-level helpers).
    pub fn bind_native_surface(
        &mut self,
        _handles: NativeSurfaceHandles,
    ) -> Result<(), WinitRuntimeError> {
        // TODO(verso): Initialize and attach Verso controller using `_handles`.
        Ok(())
    }

    /// Load a URL or local resource. Placeholder for the Verso navigation API.
    pub fn load(&mut self, _url: &str) -> Result<(), WinitRuntimeError> {
        // TODO(verso): Forward to the controller.
        Ok(())
    }

    /// Evaluate JavaScript in the page context. Placeholder for Verso scripting.
    pub fn eval_script(&mut self, _source: &str) -> Result<(), WinitRuntimeError> {
        // TODO(verso): Forward to the controller.
        Ok(())
    }

    /// Notify the host that the window was resized. Placeholder for viewport updates.
    pub fn resize(&mut self, _new_size: PhysicalSize<u32>) -> Result<(), WinitRuntimeError> {
        // TODO(verso): Forward to the controller and request redraw if needed.
        Ok(())
    }

    /// Paint or present now if the controller uses explicit drawing hooks.
    pub fn draw(&mut self) -> Result<(), WinitRuntimeError> {
        // TODO(verso): Call into the compositor/presenter if needed.
        Ok(())
    }
}

/// Internal registration record for a single window.
#[derive(Default)]
struct WindowRecord<T> {
    subscribers: Vec<Box<dyn EventSubscriber<T>>>,
}

/// A thin runtime layer that owns (or integrates with) a `winit` event loop,
/// coordinates windows, and dispatches events to subscribers.
///
/// This struct is safe to share across threads for posting events and registering
/// subscribers; the actual `winit::window::Window` objects are owned by the event-loop
/// thread in the `App` handler.
pub struct WinitRuntime<T = ()> {
    event_loop_proxy: Arc<Mutex<Option<EventLoopProxy<RuntimeEvent<T>>>>>,
    windows: Arc<Mutex<HashMap<WindowId, WindowRecord<T>>>>,
    on_user_event: Arc<Mutex<Option<Box<dyn FnMut(T)>>>>,
}

impl<T> Default for WinitRuntime<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> WinitRuntime<T>
where
    T: 'static + Send,
{
    /// Construct a new runtime.
    pub fn new() -> Self {
        Self {
            event_loop_proxy: Arc::new(Mutex::new(None)),
            windows: Arc::new(Mutex::new(HashMap::new())),
            on_user_event: Arc::new(Mutex::new(None)),
        }
    }

    /// Create a new platform window via the event loop and return a `WinitWindowHandle`.
    ///
    /// This must be called from a thread other than the event loop thread, or in
    /// response to a user event (to avoid deadlock). It blocks until the window
    /// is created and an id is returned by the event loop thread.
    pub fn create_window_with(
        &self,
        attrs: WindowAttributes,
    ) -> Result<WinitWindowHandle, WinitRuntimeError> {
        let (tx, rx) = mpsc::channel::<WindowId>();
        let cmd = InternalCommand::CreateWindow {
            attrs,
            respond_to: tx,
        };
        self.post_internal(cmd)?;
        let id = rx
            .recv()
            .map_err(|_| WinitRuntimeError::Backend("failed to receive window id"))?;
        Ok(WinitWindowHandle::new(id))
    }

    /// Convenience: create a default window with a title.
    pub fn create_window(
        &self,
        title: &str,
        size: Option<LogicalSize<f64>>,
    ) -> Result<WinitWindowHandle, WinitRuntimeError> {
        let mut attrs = Window::default_attributes().with_title(title.to_string());
        if let Some(sz) = size {
            attrs = attrs.with_inner_size(sz);
        }
        self.create_window_with(attrs)
    }

    /// Register an event subscriber for a given window.
    pub fn add_subscriber(
        &self,
        window: WinitWindowHandle,
        subscriber: impl EventSubscriber<T> + 'static,
    ) -> Result<(), WinitRuntimeError> {
        let mut map = self.windows.lock().unwrap();
        let record = map
            .get_mut(&window.id)
            .ok_or(WinitRuntimeError::UnknownWindow(window.id))?;
        record.subscribers.push(Box::new(subscriber));
        Ok(())
    }

    /// Post an internal command to the event loop thread using the `EventLoopProxy`.
    pub fn post_internal(&self, cmd: InternalCommand) -> Result<(), WinitRuntimeError> {
        let proxy = self
            .event_loop_proxy
            .lock()
            .unwrap()
            .as_ref()
            .cloned()
            .ok_or(WinitRuntimeError::NotInitialized(
                "event loop proxy not available",
            ))?;
        proxy
            .send_event(RuntimeEvent::Internal(cmd))
            .map_err(|_| WinitRuntimeError::Backend("failed to post event"))
    }

    /// Post a user-defined event to the event loop thread.
    pub fn post_user_event(&self, event: T) -> Result<(), WinitRuntimeError> {
        let proxy = self
            .event_loop_proxy
            .lock()
            .unwrap()
            .as_ref()
            .cloned()
            .ok_or(WinitRuntimeError::NotInitialized(
                "event loop proxy not available",
            ))?;
        proxy
            .send_event(RuntimeEvent::User(event))
            .map_err(|_| WinitRuntimeError::Backend("failed to post user event"))
    }

    /// Run the event loop and dispatch events to registered subscribers.
    ///
    /// This blocks until the application exits.
    pub fn run(mut self, on_user_event: impl FnMut(T) + 'static) -> Result<(), WinitRuntimeError> {
        // Build the event loop
        let event_loop = EventLoop::<RuntimeEvent<T>>::with_user_event()
            .build()
            .map_err(|_| WinitRuntimeError::Backend("failed to build event loop"))?;

        // Prepare state
        let proxy = event_loop.create_proxy();
        {
            let mut guard = self.event_loop_proxy.lock().unwrap();
            *guard = Some(proxy);
        }
        {
            let mut cb = self.on_user_event.lock().unwrap();
            *cb = Some(Box::new(on_user_event));
        }

        // Wrap state into the App handler and run
        let mut app = App {
            runtime: self,
            windows: HashMap::new(),
        };

        // If run_app returns an error, map it; otherwise we complete successfully.
        event_loop
            .run_app(&mut app)
            .map_err(|_| WinitRuntimeError::Backend("event loop error"))
    }
}

/// Internal application handler bridging winit and our runtime.
struct App<T> {
    runtime: WinitRuntime<T>,
    windows: HashMap<WindowId, Window>,
}

impl<T> App<T>
where
    T: 'static + Send,
{
    fn dispatch_to_window_subscribers(
        &mut self,
        window: WinitWindowHandle,
        f: impl Fn(&mut dyn EventSubscriber<T>, &mut DispatchContext<T>),
    ) -> Result<(), WinitRuntimeError> {
        let mut map = self.runtime.windows.lock().unwrap();
        let Some(record) = map.get_mut(&window.id) else {
            return Err(WinitRuntimeError::UnknownWindow(window.id));
        };

        let mut ctx = DispatchContext {
            runtime: &self.runtime,
        };

        for sub in record.subscribers.iter_mut() {
            f(sub.as_mut(), &mut ctx);
        }
        Ok(())
    }

    fn handle_internal_command(
        &mut self,
        event_loop: &ActiveEventLoop,
        cmd: InternalCommand,
    ) -> Result<(), WinitRuntimeError> {
        match cmd {
            InternalCommand::CreateWindow { attrs, respond_to } => {
                let window = event_loop
                    .create_window(attrs)
                    .map_err(|_| WinitRuntimeError::Backend("failed to create window"))?;
                let id = window.id();
                self.windows.insert(id, window);
                // Initialize subscriber list
                self.runtime
                    .windows
                    .lock()
                    .unwrap()
                    .entry(id)
                    .or_insert_with(WindowRecord::default);
                // Respond back with the new id
                let _ = respond_to.send(id);
            }
            InternalCommand::RequestRedraw(id) => {
                if let Some(w) = self.windows.get(&id) {
                    w.request_redraw();
                }
            }
            InternalCommand::SetTitle(id, title) => {
                if let Some(w) = self.windows.get(&id) {
                    w.set_title(title);
                }
            }
            InternalCommand::SetCursorIcon(id, icon) => {
                if let Some(w) = self.windows.get(&id) {
                    w.set_cursor_icon(icon);
                }
            }
            InternalCommand::SetCursorVisible(id, visible) => {
                if let Some(w) = self.windows.get(&id) {
                    w.set_cursor_visible(visible);
                }
            }
            InternalCommand::SetCursorGrab(id, grab) => {
                if let Some(w) = self.windows.get(&id) {
                    let mode = if grab {
                        CursorGrabMode::Confined
                    } else {
                        CursorGrabMode::None
                    };
                    let _ = w.set_cursor_grab(mode);
                }
            }
            InternalCommand::CloseWindow(id) => {
                // Best-effort: hide the window and remove it from our registry.
                if let Some(w) = self.windows.get(&id) {
                    w.set_visible(false);
                }
                self.windows.remove(&id);
                self.runtime.windows.lock().unwrap().remove(&id);
                // If there are no more windows, exit the loop.
                if self.windows.is_empty() {
                    event_loop.exit();
                }
            }
        }
        Ok(())
    }
}

impl<T> ApplicationHandler<RuntimeEvent<T>> for App<T>
where
    T: 'static + Send,
{
    fn resumed(&mut self, _event_loop: &ActiveEventLoop) {
        // App has entered the "resumed" state; platforms may deliver events now.
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: RuntimeEvent<T>) {
        match event {
            RuntimeEvent::User(e) => {
                if let Some(cb) = self.runtime.on_user_event.lock().unwrap().as_mut() {
                    cb(e);
                }
            }
            RuntimeEvent::Internal(cmd) => {
                let _ = self.handle_internal_command(event_loop, cmd);
            }
        }
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        window_id: WindowId,
        event: WindowEvent,
    ) {
        // Dispatch to subscribers
        let handle = WinitWindowHandle::new(window_id);

        // Special-case: close request
        if let WindowEvent::CloseRequested = event {
            // Remove our state and exit if none remain
            self.windows.remove(&window_id);
            self.runtime.windows.lock().unwrap().remove(&window_id);
            if self.windows.is_empty() {
                event_loop.exit();
            }
            return;
        }

        // Redraw request hook
        if let WindowEvent::RedrawRequested = event {
            let _ = self.dispatch_to_window_subscribers(handle, |sub, ctx| {
                sub.on_redraw_requested(handle, ctx);
                sub.on_draw(handle, ctx);
            });
            return;
        }

        let _ = self.dispatch_to_window_subscribers(handle, |sub, ctx| {
            sub.on_window_event(handle, &event, ctx);
        });
    }

    fn device_event(
        &mut self,
        _event_loop: &ActiveEventLoop,
        _device_id: winit::event::DeviceId,
        event: DeviceEvent,
    ) {
        // Broadcast device events to all subscribers of all windows
        let mut ctx = DispatchContext {
            runtime: &self.runtime,
        };
        let mut map = self.runtime.windows.lock().unwrap();
        for record in map.values_mut() {
            for sub in record.subscribers.iter_mut() {
                sub.on_device_event(&event, &mut ctx);
            }
        }
    }

    fn about_to_wait(&mut self, _event_loop: &ActiveEventLoop) {
        // Called just before the event loop sleeps. This is a good point to request redraws
        // if you have animations or need to drive a render loop.
    }
}

// =======================
// Mapping helpers
// =======================

/// Translate a winit mouse button into a simple numeric code.
///
/// Placeholder: return a simple numeric code for now, to be adapted to Verso APIs later.
pub fn map_winit_mouse_button(button: winit::event::MouseButton) -> u8 {
    match button {
        winit::event::MouseButton::Left => 1,
        winit::event::MouseButton::Right => 2,
        winit::event::MouseButton::Middle => 3,
        winit::event::MouseButton::Back => 4,
        winit::event::MouseButton::Forward => 5,
        winit::event::MouseButton::Other(x) => (10 + (x % 245) as u8), // keep within u8
    }
}

/// Translate a winit keyboard `Key` into a simplified representation.
///
/// Placeholder: dumps NamedKey variants into ASCII-ish codes when possible.
/// Text input composition should be handled via `ReceivedCharacter`.
pub fn map_winit_key(key: &Key) -> Option<&'static str> {
    use winit::keyboard::Key::*;
    match key {
        Character(_) => None, // text input is handled via `ReceivedCharacter`
        Named(n) => {
            use winit::keyboard::NamedKey::*;
            let s = match n {
                Enter => "Enter",
                Tab => "Tab",
                Space => "Space",
                ArrowUp => "ArrowUp",
                ArrowDown => "ArrowDown",
                ArrowLeft => "ArrowLeft",
                ArrowRight => "ArrowRight",
                Escape => "Escape",
                Backspace => "Backspace",
                Delete => "Delete",
                Insert => "Insert",
                Home => "Home",
                End => "End",
                PageUp => "PageUp",
                PageDown => "PageDown",
                F1 => "F1",
                F2 => "F2",
                F3 => "F3",
                F4 => "F4",
                F5 => "F5",
                F6 => "F6",
                F7 => "F7",
                F8 => "F8",
                F9 => "F9",
                F10 => "F10",
                F11 => "F11",
                F12 => "F12",
                _ => "Unmapped",
            };
            Some(s)
        }
        // Dead keys and other complex inputs should be handled by composition/text input paths.
        Dead(_) => Some("Dead"),
        Unidentified(_) => Some("Unidentified"),
    }
}

/// Convert logical size to physical pixels for a given scale factor.
pub fn logical_to_physical(size: LogicalSize<f64>, scale_factor: f64) -> PhysicalSize<u32> {
    let phys = size.to_physical::<u32>(scale_factor);
    PhysicalSize::new(phys.width, phys.height)
}
