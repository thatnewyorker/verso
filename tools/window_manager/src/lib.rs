/*!
Window manager and builder API for Verso apps.

Overview
- This crate provides a small, dependency-light window + webview builder inspired by Tauri’s
  ergonomics, but focused on Verso/Dioxus workflows and keeping defaults minimal.
- It does not create OS windows by itself. Instead, it returns a `VersoWindow` handle that
 :
  - carries the requested configuration,
  - stores event hooks (title change, new-window, focus, close),
  - exposes common runtime actions (show/hide/close/eval_script/send_ipc_event), and
  - can optionally be wired to a real backend via `Backend` callbacks.
- Consumers can integrate this builder with `verso-standalone`’s `WinitRuntime` and controller
  when desired (e.g., in example apps), while keeping this crate generic and reusable.

Key design goals
- Ergonomic, discoverable API for Windows + Webviews (a la Tauri’s `WebviewWindowBuilder`).
- Small and dependency-light by default (serde + winit types only).
- Feature-gated behavior left up to integrators (this crate has no global singletons).
- Ready for progressive enhancement: you can add backend callbacks later or mutate handlers.

Typical usage
- Build a window description using `VersoWindowBuilder`.
- Optionally attach event hooks during build.
- Call `build()` (returns a `VersoWindow` using a no-op backend), or
  `build_with_backend(backend)` to wire to a host that knows how to talk to your runtime.
- Later, call methods on `VersoWindow` (show/hide/close/eval_script/send_ipc_event).
- If you have a host runtime (e.g., `verso-standalone` + `winit`), provide a `Backend`
  that implements these actions and triggers the registered event hooks as appropriate.

Note
- This crate does not depend on `verso-standalone` to avoid circular deps. Integration lives
  in the host app or higher-level toolbox crates.
*/

#![forbid(unsafe_code)]
#![deny(missing_docs)]

use serde::{Deserialize, Serialize};
use std::fmt;

#[cfg(feature = "tracing")]
use tracing::{debug, info, warn};

/// Window size in logical units (DPI-independent).
///
/// Use integers to reduce ambiguity in common desktop use-cases.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogicalSize {
    /// Width in logical pixels.
    pub width: u32,
    /// Height in logical pixels.
    pub height: u32,
}

impl LogicalSize {
    /// Create a new logical size.
    pub fn new(width: u32, height: u32) -> Self {
        Self { width, height }
    }
}

/// Options that configure a window + webview at creation time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WindowOptions {
    /// Initial title for the window.
    pub title: String,
    /// Initial size in logical pixels.
    pub size: Option<LogicalSize>,
    /// Minimum size in logical pixels.
    pub min_size: Option<LogicalSize>,
    /// Maximum size in logical pixels.
    pub max_size: Option<LogicalSize>,
    /// Whether to draw window decorations (titlebar, borders).
    pub decorations: bool,
    /// Whether the window should remain on top of other windows.
    pub always_on_top: bool,
    /// Whether the window is resizable by the user.
    pub resizable: bool,
    /// Initialization scripts injected early into the page.
    pub initialization_scripts: Vec<String>,
    /// Optional devtools port; 0 to request a random available port in supporting runtimes.
    pub devtools_port: Option<u16>,
}

impl Default for WindowOptions {
    fn default() -> Self {
        Self {
            title: "Verso App".to_string(),
            size: None,
            min_size: None,
            max_size: None,
            decorations: true,
            always_on_top: false,
            resizable: true,
            initialization_scripts: Vec::new(),
            devtools_port: None,
        }
    }
}

/// Hooks that can be registered to observe or control common window/webview lifecycle.
#[derive(Default)]
struct EventHooks {
    on_title_changed: Option<Box<dyn Fn(&str) + Send + Sync + 'static>>,
    on_new_window: Option<Box<dyn Fn(&WindowOptions) + Send + Sync + 'static>>,
    on_focus: Option<Box<dyn Fn() + Send + Sync + 'static>>,
    on_close: Option<Box<dyn Fn() + Send + Sync + 'static>>,
}

/// Backend actions that a host can provide to make `VersoWindow` operational.
///
/// By default, a `VersoWindow` created via `build()` uses a null backend where
/// these operations are no-ops. Host code can pass a concrete backend with
/// closures that bridge to a real runtime (e.g., winit + versoview).
pub struct Backend {
    /// Show the native window.
    pub show: Option<Box<dyn FnMut() + Send + 'static>>,
    /// Hide the native window.
    pub hide: Option<Box<dyn FnMut() + Send + 'static>>,
    /// Close/destroy the native window.
    pub close: Option<Box<dyn FnMut() + Send + 'static>>,
    /// Evaluate JavaScript in the page context.
    pub eval_script: Option<Box<dyn Fn(&str) -> Result<(), String> + Send + Sync + 'static>>,
    /// Send a named IPC event with an optional payload to the webview.
    pub send_ipc_event:
        Option<Box<dyn Fn(&str, &serde_json::Value) -> Result<(), String> + Send + Sync + 'static>>,
}

impl Default for Backend {
    fn default() -> Self {
        Self {
            show: None,
            hide: None,
            close: None,
            eval_script: None,
            send_ipc_event: None,
        }
    }
}

impl Backend {
    /// Returns a backend with no-op actions (the default).
    pub fn null() -> Self {
        Self::default()
    }
}

/// Error type for window operations.
#[derive(Debug)]
pub enum WindowError {
    /// Operation is unsupported by the current backend.
    Unsupported(&'static str),
    /// Operation failed with a provided message.
    Failed(String),
    /// The window was already closed or invalid.
    Closed,
}

impl fmt::Display for WindowError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WindowError::Unsupported(msg) => write!(f, "unsupported: {msg}"),
            WindowError::Failed(msg) => write!(f, "failed: {msg}"),
            WindowError::Closed => write!(f, "window is closed"),
        }
    }
}

impl std::error::Error for WindowError {}

/// A handle representing a logical Verso window and its associated webview.
///
/// Notes
/// - This type does not own any OS-level resources by itself; a host backend may.
/// - Event hooks are stored internally and can be triggered by the host or indirectly
///   when certain methods are called (e.g., `close`).
pub struct VersoWindow {
    opts: WindowOptions,
    hooks: EventHooks,
    backend: Backend,
    visible: bool,
    closed: bool,
    id: u64,
}

impl VersoWindow {
    /// Internal constructor.
    fn new(opts: WindowOptions, hooks: EventHooks, backend: Backend, id: u64) -> Self {
        Self {
            opts,
            hooks,
            backend,
            visible: false,
            closed: false,
            id,
        }
    }

    /// Returns a stable identifier for this window handle.
    pub fn id(&self) -> u64 {
        self.id
    }

    /// Returns a copy of the current window options (immutable snapshot).
    pub fn options(&self) -> &WindowOptions {
        &self.opts
    }

    /// Set the title and notify listeners.
    pub fn set_title(&mut self, title: impl Into<String>) {
        let new_title = title.into();
        self.opts.title = new_title.clone();
        if let Some(cb) = &self.hooks.on_title_changed {
            cb(&new_title);
        }
    }

    /// Show the window if supported by the backend.
    pub fn show(&mut self) -> Result<(), WindowError> {
        if self.closed {
            return Err(WindowError::Closed);
        }
        self.visible = true;
        if let Some(f) = self.backend.show.as_mut() {
            f();
            Ok(())
        } else {
            Ok(())
        }
    }

    /// Hide the window if supported by the backend.
    pub fn hide(&mut self) -> Result<(), WindowError> {
        if self.closed {
            return Err(WindowError::Closed);
        }
        self.visible = false;
        if let Some(f) = self.backend.hide.as_mut() {
            f();
            Ok(())
        } else {
            Ok(())
        }
    }

    /// Close the window, notify listeners, and mark the handle as closed.
    ///
    /// Hosts should also destroy underlying OS resources in their backend handler.
    pub fn close(&mut self) -> Result<(), WindowError> {
        if self.closed {
            return Ok(());
        }
        self.closed = true;
        if let Some(f) = self.backend.close.as_mut() {
            f();
        }
        if let Some(cb) = &self.hooks.on_close {
            cb();
        }
        Ok(())
    }

    /// Evaluate a JavaScript snippet in the page context if supported by the backend.
    pub fn eval_script(&self, source: &str) -> Result<(), WindowError> {
        if self.closed {
            return Err(WindowError::Closed);
        }
        if let Some(f) = &self.backend.eval_script {
            f(source).map_err(WindowError::Failed)
        } else {
            Ok(())
        }
    }

    /// Send a named IPC event with optional JSON payload to the webview if supported.
    pub fn send_ipc_event(
        &self,
        name: &str,
        payload: &serde_json::Value,
    ) -> Result<(), WindowError> {
        if self.closed {
            return Err(WindowError::Closed);
        }
        if let Some(f) = &self.backend.send_ipc_event {
            f(name, payload).map_err(WindowError::Failed)
        } else {
            Ok(())
        }
    }

    /// Trigger a synthetic "focus" event to listeners (host can call this on native focus).
    pub fn notify_focused(&self) {
        if let Some(cb) = &self.hooks.on_focus {
            cb();
        }
    }

    /// Trigger a synthetic "new-window" event to listeners, passing a proposed `WindowOptions`.
    ///
    /// Typically used when the webview requests a new window (e.g., target="_blank").
    pub fn notify_new_window(&self, proposed: &WindowOptions) {
        if let Some(cb) = &self.hooks.on_new_window {
            cb(proposed);
        }
    }

    /// Returns whether the window is currently visible (best-effort).
    pub fn is_visible(&self) -> bool {
        self.visible
    }

    /// Returns whether the window was closed via `close()`.
    pub fn is_closed(&self) -> bool {
        self.closed
    }
}

/// Fluent builder for `VersoWindow` + webview configuration.
#[derive(Default)]
pub struct VersoWindowBuilder {
    opts: WindowOptions,
    hooks: EventHooks,
}

impl VersoWindowBuilder {
    /// Create a new builder with default options.
    pub fn new() -> Self {
        Self {
            opts: WindowOptions::default(),
            hooks: EventHooks::default(),
        }
    }

    /// Set the initial title.
    pub fn title(mut self, title: impl Into<String>) -> Self {
        self.opts.title = title.into();
        self
    }

    /// Set the initial size (logical units).
    pub fn size(mut self, width: u32, height: u32) -> Self {
        self.opts.size = Some(LogicalSize::new(width, height));
        self
    }

    /// Set the minimum size (logical units).
    pub fn min_size(mut self, width: u32, height: u32) -> Self {
        self.opts.min_size = Some(LogicalSize::new(width, height));
        self
    }

    /// Set the maximum size (logical units).
    pub fn max_size(mut self, width: u32, height: u32) -> Self {
        self.opts.max_size = Some(LogicalSize::new(width, height));
        self
    }

    /// Enable/disable window decorations.
    pub fn decorations(mut self, decorations: bool) -> Self {
        self.opts.decorations = decorations;
        self
    }

    /// Enable/disable always-on-top behavior.
    pub fn always_on_top(mut self, always_on_top: bool) -> Self {
        self.opts.always_on_top = always_on_top;
        self
    }

    /// Enable/disable window resizing by the user.
    pub fn resizable(mut self, resizable: bool) -> Self {
        self.opts.resizable = resizable;
        self
    }

    /// Append one initialization script.
    pub fn initialization_script(mut self, script: impl Into<String>) -> Self {
        self.opts.initialization_scripts.push(script.into());
        self
    }

    /// Replace initialization scripts with the given list.
    pub fn initialization_scripts<I, S>(mut self, scripts: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.opts.initialization_scripts = scripts.into_iter().map(Into::into).collect();
        self
    }

    /// Set the desired devtools port (0 or None to auto-select).
    pub fn devtools_port(mut self, port: Option<u16>) -> Self {
        self.opts.devtools_port = port;
        self
    }

    /// Register a callback for document title changes.
    pub fn on_title_changed(mut self, f: impl Fn(&str) + Send + Sync + 'static) -> Self {
        self.hooks.on_title_changed = Some(Box::new(f));
        self
    }

    /// Register a callback when the webview requests a new window.
    pub fn on_new_window(mut self, f: impl Fn(&WindowOptions) + Send + Sync + 'static) -> Self {
        self.hooks.on_new_window = Some(Box::new(f));
        self
    }

    /// Register a callback when the window gains focus.
    pub fn on_focus(mut self, f: impl Fn() + Send + Sync + 'static) -> Self {
        self.hooks.on_focus = Some(Box::new(f));
        self
    }

    /// Register a callback when the window is closing/closed.
    pub fn on_close(mut self, f: impl Fn() + Send + Sync + 'static) -> Self {
        self.hooks.on_close = Some(Box::new(f));
        self
    }

    /// Build a `VersoWindow` using a null backend (no OS integration).
    ///
    /// Use `build_with_backend` to supply real backend actions.
    pub fn build(self) -> VersoWindow {
        static NEXT_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        let id = NEXT_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        VersoWindow::new(self.opts, self.hooks, Backend::null(), id)
    }

    /// Build a `VersoWindow` using the provided backend actions.
    pub fn build_with_backend(self, backend: Backend) -> VersoWindow {
        static NEXT_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        let id = NEXT_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        VersoWindow::new(self.opts, self.hooks, backend, id)
    }

    /// Convert into the raw `WindowOptions` (descriptor) without creating a handle.
    pub fn into_options(self) -> WindowOptions {
        self.opts
    }
}

// Re-export commonly used items for convenience.
pub use LogicalSize as VersoLogicalSize;
pub use VersoWindowBuilder as WindowBuilder;
pub use WindowOptions as VersoWindowOptions;
