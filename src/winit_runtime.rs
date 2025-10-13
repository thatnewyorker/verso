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

use crate::ControllerMode;
#[cfg(not(feature = "versoview-runtime"))]
use crate::controller::MockWebviewController;
use crate::controller::{ButtonState, ControllerConfig, InputEvent, WebviewController};
use raw_window_handle::{HasDisplayHandle, HasWindowHandle};
use raw_window_handle::{RawDisplayHandle, RawWindowHandle};
use winit::application::ApplicationHandler;
use winit::event::{DeviceEvent, ElementState, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, EventLoop, EventLoopProxy};
use winit::window::{CursorGrabMode, CursorIcon, Window, WindowAttributes, WindowId};

/// Publicly re-export common winit identifiers that downstream users often need.
pub use winit::dpi::{LogicalSize, PhysicalSize};
pub use winit::keyboard::Key;

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
    GetNativeSurfaceHandles {
        window_id: WindowId,
        respond_to: mpsc::Sender<NativeSurfaceHandles>,
    },
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
pub struct DispatchContext<'a, T: 'static = ()> {
    runtime: &'a WinitRuntime<T>,
    host: Option<&'a mut VersoWebviewHost>,
}

impl<'a, T: 'static> DispatchContext<'a, T> {
    pub fn runtime(&self) -> &WinitRuntime<T> {
        self.runtime
    }

    pub fn host_mut(&mut self) -> Option<&mut VersoWebviewHost> {
        self.host.as_deref_mut()
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
    controller: Option<Box<dyn WebviewController>>,
}

impl VersoWebviewHost {
    /// Create a new host for the given window handle. You are expected to call
    /// `bind_native_surface` soon after to attach the Verso/Servo renderer.
    pub fn new(window: WinitWindowHandle) -> Self {
        Self {
            window,
            controller: None,
        }
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
        handles: NativeSurfaceHandles,
        config: ControllerConfig,
    ) -> Result<(), WinitRuntimeError> {
        // Select a controller backend based on feature flags and the provided config.
        let mut controller: Box<dyn WebviewController>;
        #[cfg(feature = "versoview-runtime")]
        {
            controller = match config.mode {
                ControllerMode::InProcess => {
                    Box::new(crate::in_process_controller::InProcessController::new())
                }
                ControllerMode::OutOfProcess => {
                    Box::new(crate::ipc_controller::IpcController::new())
                }
            };
        }
        #[cfg(not(feature = "versoview-runtime"))]
        {
            controller = Box::new(MockWebviewController::default());
        }
        controller
            .initialize(config)
            .map_err(|_| WinitRuntimeError::Backend("controller initialize failed"))?;
        controller
            .bind_surface(handles)
            .map_err(|_| WinitRuntimeError::Backend("controller bind failed"))?;
        self.controller = Some(controller);
        Ok(())
    }

    /// Load a URL or local resource by forwarding to the controller.
    pub fn load(&mut self, url: &str) -> Result<(), WinitRuntimeError> {
        let ctrl = self
            .controller
            .as_mut()
            .ok_or(WinitRuntimeError::NotInitialized("controller not bound"))?;
        ctrl.load(url)
            .map_err(|_| WinitRuntimeError::Backend("controller load failed"))
    }

    /// Evaluate JavaScript in the page context via the controller.
    pub fn eval_script(&mut self, source: &str) -> Result<(), WinitRuntimeError> {
        let ctrl = self
            .controller
            .as_mut()
            .ok_or(WinitRuntimeError::NotInitialized("controller not bound"))?;
        ctrl.eval_script(source)
            .map_err(|_| WinitRuntimeError::Backend("controller eval_script failed"))
    }

    /// Notify the controller that the window was resized.
    pub fn resize(&mut self, new_size: PhysicalSize<u32>) -> Result<(), WinitRuntimeError> {
        let ctrl = self
            .controller
            .as_mut()
            .ok_or(WinitRuntimeError::NotInitialized("controller not bound"))?;
        ctrl.resize(new_size)
            .map_err(|_| WinitRuntimeError::Backend("controller resize failed"))
    }

    /// Paint or present now if supported by the controller.
    pub fn draw(&mut self) -> Result<(), WinitRuntimeError> {
        let ctrl = self
            .controller
            .as_mut()
            .ok_or(WinitRuntimeError::NotInitialized("controller not bound"))?;
        ctrl.draw()
            .map_err(|_| WinitRuntimeError::Backend("controller draw failed"))
    }

    /// Forward a single input event to the controller.
    pub fn send_input(&mut self, event: InputEvent) -> Result<(), WinitRuntimeError> {
        let ctrl = self
            .controller
            .as_mut()
            .ok_or(WinitRuntimeError::NotInitialized("controller not bound"))?;
        ctrl.send_input(event)
            .map_err(|_| WinitRuntimeError::Backend("controller send_input failed"))
    }
}

/// Internal registration record for a single window.
struct WindowRecord<T> {
    subscribers: Vec<Box<dyn EventSubscriber<T>>>,
    host: Option<VersoWebviewHost>,
}

impl<T> Default for WindowRecord<T> {
    fn default() -> Self {
        Self {
            subscribers: Vec::new(),
            host: None,
        }
    }
}

/// A thin runtime layer that owns (or integrates with) a `winit` event loop,
/// coordinates windows, and dispatches events to subscribers.
///
/// This struct is safe to share across threads for posting events and registering
/// subscribers; the actual `winit::window::Window` objects are owned by the event-loop
/// thread in the `App` handler.
#[derive(Clone)]
pub struct WinitRuntime<T: 'static = ()> {
    event_loop_proxy: Arc<Mutex<Option<EventLoopProxy<RuntimeEvent<T>>>>>,
    windows: Arc<Mutex<HashMap<WindowId, WindowRecord<T>>>>,
    on_user_event: Arc<Mutex<Option<Box<dyn FnMut(T)>>>>,
    /// External user event channel (sender) for cross-thread injections.
    external_user_tx: Arc<Mutex<Option<mpsc::Sender<T>>>>,
    /// External user event channel (receiver) drained on the event-loop thread.
    external_user_rx: Arc<Mutex<Option<mpsc::Receiver<T>>>>,
}

impl<T> Default for WinitRuntime<T>
where
    T: 'static + Send,
{
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
            external_user_tx: Arc::new(Mutex::new(None)),
            external_user_rx: Arc::new(Mutex::new(None)),
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

    /// Attach a VersoWebviewHost to a window so subscribers can forward events to the controller.
    pub fn attach_host(
        &self,
        window: WinitWindowHandle,
        host: VersoWebviewHost,
    ) -> Result<(), WinitRuntimeError> {
        let mut map = self.windows.lock().unwrap();
        let record = map
            .get_mut(&window.id)
            .ok_or(WinitRuntimeError::UnknownWindow(window.id))?;
        record.host = Some(host);
        Ok(())
    }

    /// Attach a VersoWebviewHost and bridge controller events into user events.
    ///
    /// The provided `map` converts `ControllerEvent` into your runtime's user event type `T`,
    /// which will be posted as a `RuntimeEvent::User(T)` to the event loop.
    pub fn attach_host_with_events(
        &self,
        window: WinitWindowHandle,
        mut host: VersoWebviewHost,
        map: impl Fn(crate::controller::ControllerEvent) -> T + Send + 'static,
    ) -> Result<(), WinitRuntimeError>
    where
        T: 'static + Send,
    {
        if let Some(ctrl) = host.controller.as_mut() {
            let tx = self
                .external_user_tx
                .lock()
                .unwrap()
                .as_ref()
                .cloned()
                .ok_or(WinitRuntimeError::NotInitialized(
                    "user event channel not available",
                ))?;
            let sink: crate::controller::ControllerEventSink = Box::new(move |evt| {
                let user_evt = map(evt);
                let _ = tx.send(user_evt);
            });
            let _ = ctrl.set_event_sink(sink);
        }
        let mut mapw = self.windows.lock().unwrap();
        let record = mapw
            .get_mut(&window.id)
            .ok_or(WinitRuntimeError::UnknownWindow(window.id))?;
        record.host = Some(host);
        Ok(())
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

    /// Request raw native surface handles for a given window via the event loop.
    pub fn get_native_surface_handles(
        &self,
        window: WinitWindowHandle,
    ) -> Result<NativeSurfaceHandles, WinitRuntimeError> {
        let (tx, rx) = mpsc::channel::<NativeSurfaceHandles>();
        self.post_internal(InternalCommand::GetNativeSurfaceHandles {
            window_id: window.id(),
            respond_to: tx,
        })?;
        rx.recv()
            .map_err(|_| WinitRuntimeError::Backend("failed to receive native handles"))
    }

    /// Run the event loop and dispatch events to registered subscribers.
    ///
    /// This blocks until the application exits.
    pub fn run(self, on_user_event: impl FnMut(T) + 'static) -> Result<(), WinitRuntimeError> {
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
        // Initialize external user event channel for cross-thread injections.
        {
            let (tx, rx) = mpsc::channel::<T>();
            let mut extx = self.external_user_tx.lock().unwrap();
            *extx = Some(tx);
            let mut exrx = self.external_user_rx.lock().unwrap();
            *exrx = Some(rx);
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

impl<T> WinitRuntime<T>
where
    T: 'static + Send,
{
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
}

/// Internal application handler bridging winit and our runtime.
struct App<T: 'static> {
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

        // Temporarily take the host out to obtain a mutable reference without borrowing conflicts.
        let mut host_opt = record.host.take();

        let mut ctx = DispatchContext {
            runtime: &self.runtime,
            host: host_opt.as_mut(),
        };

        for sub in record.subscribers.iter_mut() {
            f(sub.as_mut(), &mut ctx);
        }

        // Put the host back.
        record.host = host_opt;

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
                    w.set_title(&title);
                }
            }
            InternalCommand::SetCursorIcon(id, icon) => {
                if let Some(w) = self.windows.get(&id) {
                    w.set_cursor(icon);
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
            InternalCommand::GetNativeSurfaceHandles {
                window_id,
                respond_to,
            } => {
                if let Some(w) = self.windows.get(&window_id) {
                    if let Ok(wh) = w.window_handle() {
                        let handles = NativeSurfaceHandles {
                            window: wh.as_raw(),
                            display: w.display_handle().ok().map(|d| d.as_raw()),
                        };
                        let _ = respond_to.send(handles);
                    }
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
            host: None,
        };
        let mut map = self.runtime.windows.lock().unwrap();
        for record in map.values_mut() {
            for sub in record.subscribers.iter_mut() {
                sub.on_device_event(&event, &mut ctx);
            }
        }
    }

    fn about_to_wait(&mut self, _event_loop: &ActiveEventLoop) {
        // Drain external user event channel and dispatch to the user callback.
        loop {
            let next = {
                let mut guard = self.runtime.external_user_rx.lock().unwrap();
                if let Some(rx) = guard.as_mut() {
                    rx.try_recv().ok()
                } else {
                    None
                }
            };
            if let Some(ev) = next {
                if let Some(cb) = self.runtime.on_user_event.lock().unwrap().as_mut() {
                    cb(ev);
                }
            } else {
                break;
            }
        }
        // Additional redraw orchestration can be performed here as needed.
    }
}

// =======================
// Controller event subscriber
// =======================

/// A basic subscriber that forwards winit events to the controller
/// via the VersoWebviewHost attached to this window.
pub struct ControllerEventSubscriber {}

impl ControllerEventSubscriber {
    pub fn new(_window: WinitWindowHandle) -> Self {
        Self {}
    }
}

impl<T> EventSubscriber<T> for ControllerEventSubscriber
where
    T: Send + 'static,
{
    fn on_window_event(
        &mut self,
        _window: WinitWindowHandle,
        event: &WindowEvent,
        ctx: &mut DispatchContext<T>,
    ) {
        if let Some(host) = ctx.host_mut() {
            match event {
                WindowEvent::CursorMoved { position, .. } => {
                    let _ = host.send_input(InputEvent::MouseMove {
                        x: position.x,
                        y: position.y,
                    });
                }
                WindowEvent::MouseInput { state, button, .. } => {
                    let btn = map_winit_mouse_button(*button);
                    let pressed = matches!(state, ElementState::Pressed);
                    let _ = host.send_input(InputEvent::MouseButton {
                        button: btn,
                        state: if pressed {
                            ButtonState::Pressed
                        } else {
                            ButtonState::Released
                        },
                    });
                }
                WindowEvent::MouseWheel { delta, .. } => {
                    let (dx, dy, precise) = match delta {
                        MouseScrollDelta::LineDelta(x, y) => (*x as f32, *y as f32, false),
                        MouseScrollDelta::PixelDelta(p) => (p.x as f32, p.y as f32, true),
                    };
                    let _ = host.send_input(InputEvent::Scroll {
                        delta_x: dx,
                        delta_y: dy,
                        is_precise: precise,
                    });
                }
                WindowEvent::KeyboardInput { event, .. } => {
                    if let Some(name) = map_winit_key(&event.logical_key) {
                        match event.state {
                            ElementState::Pressed => {
                                let _ = host.send_input(InputEvent::KeyDown { key: name });
                            }
                            ElementState::Released => {
                                let _ = host.send_input(InputEvent::KeyUp { key: name });
                            }
                        }
                    }
                }
                WindowEvent::Ime(winit::event::Ime::Commit(text)) => {
                    for ch in text.chars() {
                        let _ = host.send_input(InputEvent::CharInput { ch });
                    }
                }
                WindowEvent::Focused(focused) => {
                    let _ = host.send_input(InputEvent::FocusChanged { focused: *focused });
                }
                WindowEvent::CursorEntered { .. } => {
                    let _ = host.send_input(InputEvent::PointerEntered);
                }
                WindowEvent::CursorLeft { .. } => {
                    let _ = host.send_input(InputEvent::PointerLeft);
                }
                WindowEvent::Resized(new_size) => {
                    let _ = host.resize(*new_size);
                }
                WindowEvent::ScaleFactorChanged { .. } => {
                    // Without a new size available here, trigger a redraw path.
                    let _ = host.draw();
                }
                WindowEvent::ModifiersChanged(_mods) => {
                    // Modifier changes can be tracked if the controller requires them.
                }
                _ => {}
            }
        }
    }

    fn on_redraw_requested(&mut self, _window: WinitWindowHandle, _ctx: &mut DispatchContext<T>) {
        // No-op: drawing is handled in on_draw for explicit present.
    }

    fn on_draw(&mut self, _window: WinitWindowHandle, ctx: &mut DispatchContext<T>) {
        if let Some(host) = ctx.host_mut() {
            let _ = host.draw();
        }
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
        winit::event::MouseButton::Other(x) => 10 + (x % 245) as u8, // keep within u8
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
