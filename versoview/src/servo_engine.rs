/*!
ServoEngine adapter that implements the `Engine` trait by delegating to the in-crate `DemoEngine`.

Purpose:
- Acts as a placeholder integration layer for a future Servo-backed engine.
- Keeps the public `Engine` boundary intact so the rest of the IPC server code remains unchanged.
- Allows switching from the demo engine to the real Servo integration by replacing the internals
  of this adapter without modifying the transport/protocol code.

Future work:
- Replace delegations to `DemoEngine` with real calls into your Servo fork’s embedding APIs.
- Ensure thread affinity and async boundaries align with Servo’s compositor/runtime requirements.
*/

use crate::engine::{DemoEngine, Engine, EngineCapabilities, EngineError, EngineFrame, EngineInit};

/// ServoEngine – placeholder adapter that forwards to `DemoEngine`.
///
/// Replace the internals of this type with real calls into your Servo fork.
/// The `Engine` trait surface intentionally mirrors a minimal set of operations
/// `versoview` requires, keeping integration points small and explicit.
pub struct ServoEngine {
    inner: DemoEngine,
}

impl ServoEngine {
    /// Construct a new placeholder ServoEngine backed by the demo engine.
    pub fn new() -> Self {
        Self {
            inner: DemoEngine::new(),
        }
    }
}

impl Default for ServoEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl Engine for ServoEngine {
    fn init(&mut self, init: EngineInit) -> Result<EngineCapabilities, EngineError> {
        self.inner.init(init)
    }

    fn bind_surface(
        &mut self,
        surface: &verso_standalone::ipc_protocol::SurfaceDescriptor,
    ) -> Result<(), EngineError> {
        self.inner.bind_surface(surface)
    }

    fn load(&mut self, url_or_data: &str) -> Result<(), EngineError> {
        self.inner.load(url_or_data)
    }

    fn eval_script(&mut self, source: &str) -> Result<(), EngineError> {
        self.inner.eval_script(source)
    }

    fn resize(&mut self, width: u32, height: u32) -> Result<(), EngineError> {
        self.inner.resize(width, height)
    }

    fn input_events(
        &mut self,
        events: &[verso_standalone::ipc_protocol::IpcInputEvent],
    ) -> Result<(), EngineError> {
        self.inner.input_events(events)
    }

    fn request_draw(&mut self) -> Result<Option<EngineFrame>, EngineError> {
        self.inner.request_draw()
    }

    fn draw_now(&mut self) -> Result<Option<EngineFrame>, EngineError> {
        self.inner.draw_now()
    }

    fn open_devtools(&mut self, requested: Option<u16>) -> Result<Option<u16>, EngineError> {
        self.inner.open_devtools(requested)
    }

    fn shutdown(&mut self) -> Result<(), EngineError> {
        self.inner.shutdown()
    }
}
