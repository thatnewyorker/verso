#![cfg(feature = "versoview-runtime")]
/*!
In-process controller stub for Verso webviews.

This module provides a feature-gated placeholder implementation of the
`WebviewController` trait that is intended to embed Servo/Verso directly
in-process once the real engine integration is available.

Current status:
- Stub implementation that mirrors the control flow of a real controller
  (initialize -> bind_surface -> load/eval/resize/draw/input -> shutdown).
- Stores basic state for validation and testing, but does not render or
  talk to a real engine yet.
- Compile-time gated behind the `versoview-runtime` cargo feature.

Future direction:
- Replace stub internals with actual Servo/Verso embedding code.
- Initialize the graphics context / swapchain with the provided raw handles.
- Wire navigation, scripting, input, and rendering to real engine APIs.
*/

use crate::controller::{ControllerConfig, ControllerError, InputEvent, WebviewController};
use crate::winit_runtime::NativeSurfaceHandles;
use winit::dpi::PhysicalSize;

/// Placeholder in-process controller that embeds Servo directly (future).
///
/// Threading:
/// - Treat methods as UI-thread-affine unless you add an internal message queue
///   and document thread safety explicitly.
///
/// Safety:
/// - `bind_surface` receives raw window/display handles. Ensure that any future
///   unsafe usage documents invariants and does not hold handles beyond the
///   lifetime of the native window.
#[derive(Debug, Default)]
pub struct InProcessController {
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
}

impl InProcessController {
    /// Construct a new in-process controller stub.
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns diagnostic counters useful during early wiring.
    #[allow(dead_code)]
    pub fn diagnostics(&self) -> (usize, usize, usize) {
        (
            self.input_events,
            self.draws_requested,
            self.draws_performed,
        )
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

impl WebviewController for InProcessController {
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
        // Future: create graphics context / swapchain for the raw handles.
        self.surface_bound = true;
        Ok(())
    }

    fn load(&mut self, url_or_resource: &str) -> Result<(), ControllerError> {
        self.ensure_ready()?;
        // Future: trigger real navigation in the embedded engine.
        self.last_url = Some(url_or_resource.to_string());
        Ok(())
    }

    fn eval_script(&mut self, source: &str) -> Result<(), ControllerError> {
        self.ensure_ready()?;
        // Future: evaluate in page context, possibly asynchronously.
        self.last_script = Some(source.to_string());
        Ok(())
    }

    fn resize(&mut self, size: PhysicalSize<u32>) -> Result<(), ControllerError> {
        self.ensure_ready()?;
        // Future: notify engine of new viewport/backing store size.
        self.last_size = Some(size);
        Ok(())
    }

    fn request_draw(&mut self) -> Result<(), ControllerError> {
        self.ensure_ready()?;
        // Future: schedule a frame on the engine’s render loop.
        self.draws_requested += 1;
        Ok(())
    }

    fn draw(&mut self) -> Result<(), ControllerError> {
        self.ensure_ready()?;
        // Future: kick the embedded compositor/presenter to draw/present now.
        self.draws_performed += 1;
        Ok(())
    }

    fn send_input(&mut self, _event: InputEvent) -> Result<(), ControllerError> {
        self.ensure_ready()?;
        // Future: translate and forward event to the engine.
        self.input_events += 1;
        Ok(())
    }

    fn set_visibility(&mut self, visible: bool) -> Result<(), ControllerError> {
        self.ensure_ready()?;
        // Future: inform engine to throttle/stop rendering if hidden.
        self.visible = visible;
        Ok(())
    }

    fn focus(&mut self) -> Result<(), ControllerError> {
        self.ensure_ready()?;
        // Future: request keyboard focus in the embedded engine/view.
        Ok(())
    }

    fn is_alive(&self) -> bool {
        self.alive
    }

    fn shutdown(&mut self) -> Result<(), ControllerError> {
        self.ensure_initialized()?;
        // Future: tear down engine, GPU resources, and any threads cleanly.
        self.alive = false;
        Ok(())
    }
}
