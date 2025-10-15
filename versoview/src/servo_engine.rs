/*!
ServoEngine: a feature-gated, real Servo-backed engine for `versoview`.

- When built with the `servo-engine` feature:
  - Spawns a dedicated worker thread that owns Servo, a SoftwareRenderingContext,
    and a WebView.
  - The public `ServoEngine` stays `Send` by communicating with the worker via
    channels; all non-Send Servo objects remain confined to the worker thread.
  - Implements `Engine` by forwarding requests to the worker:
    - `bind_surface` initializes the worker with a SoftwareRenderingContext and WebView.
    - `load` navigates the WebView to a given URL (data: URLs supported if parsed upstream).
    - `request_draw` and `draw_now` take a screenshot (async in Servo, synchronized with a oneshot).
    - `resize` resizes the WebView/context.
    - `input_events` is currently a no-op (TODO: map to Servo input events).
    - `shutdown` shuts down the worker and joins the thread.

- When the `servo-engine` feature is NOT enabled:
  - This file falls back to a shim that delegates to `DemoEngine`.

Notes:
- This POC returns frames as `CompressedImage::Png` with inline payload for portability.
- Zero-copy (dmabuf/IOSurface/DXGI) and full input mapping can be added incrementally.

Build:
- Enable with: `--features servo-engine` on the `versoview` crate.
*/

#![allow(dead_code)]

use crate::engine::{Engine, EngineCapabilities, EngineError, EngineFrame, EngineInit};

#[cfg(not(feature = "servo-engine"))]
use crate::engine::DemoEngine;

#[cfg(feature = "servo-engine")]
mod real {
    use super::*;
    use embedder_traits::EventLoopWaker;
    use image::{DynamicImage, ImageFormat};
    use std::path::PathBuf;
    use std::sync::mpsc::{self, Receiver, Sender};
    use std::thread::{self, JoinHandle};
    use std::time::{Duration, Instant};
    use url::Url;
    use verso_standalone::ipc_protocol as proto;

    // Servo imports (libservo re-exports)
    use euclid::Scale;
    use servo::servo_geometry::DeviceIndependentPixel;
    use servo::{
        RenderingContext, Servo, ServoBuilder, SoftwareRenderingContext, WebView, WebViewBuilder,
    };
    use webrender_api::units::DevicePixel;
    use winit::dpi::PhysicalSize;

    // Simple waker to satisfy Servo's requirement. We manually spin the event loop when needed.
    #[derive(Clone)]
    struct NopWaker;

    impl EventLoopWaker for NopWaker {
        fn clone_box(&self) -> Box<dyn EventLoopWaker> {
            Box::new(self.clone())
        }
        fn wake(&self) {
            // No-op: the worker thread manually spins Servo's event loop when needed.
        }
    }

    // Commands sent from the `ServoEngine` (host thread) to the worker thread.
    enum Cmd {
        Init {
            width: u32,
            height: u32,
            scale_factor: f32,
            resources_dir: Option<PathBuf>,
            user_agent: Option<String>,
            init_scripts: Vec<String>,
            devtools_port: Option<u16>,
            reply: Sender<Result<(), String>>,
        },
        Load {
            url: String,
            reply: Sender<Result<(), String>>,
        },
        Resize {
            width: u32,
            height: u32,
            reply: Sender<Result<(), String>>,
        },
        EvalScript {
            source: String,
            reply: Sender<Result<(), String>>,
        },
        DrawNow {
            reply: Sender<Result<FramePng, String>>,
        },
        RequestDraw {
            reply: Sender<Result<FramePng, String>>,
        },
        Shutdown,
    }

    // A simple frame container for the worker -> host channel.
    struct FramePng {
        width: u32,
        height: u32,
        png: Vec<u8>,
    }

    // State kept by the worker thread only.
    struct Worker {
        servo: Option<Servo>,
        webview: Option<WebView>,
        size: PhysicalSize<u32>,
        scale_factor: f32,
    }

    impl Worker {
        fn new() -> Self {
            Self {
                servo: None,
                webview: None,
                size: PhysicalSize::new(1, 1),
                scale_factor: 1.0,
            }
        }

        fn init(
            &mut self,
            width: u32,
            height: u32,
            scale_factor: f32,
            resources_dir: Option<PathBuf>,
            _user_agent: Option<String>,
            _init_scripts: Vec<String>,
            _devtools_port: Option<u16>,
        ) -> Result<(), String> {
            let size = PhysicalSize::new(width.max(1), height.max(1));
            self.size = size;
            self.scale_factor = if scale_factor.is_finite() && scale_factor > 0.0 {
                scale_factor
            } else {
                1.0
            };

            // Create a headless software rendering context.
            let sw = SoftwareRenderingContext::new(size)
                .map_err(|e| format!("SoftwareRenderingContext::new failed: {e:?}"))?;
            let rc = std::rc::Rc::new(sw);

            // Build Servo instance.
            let mut builder = ServoBuilder::new(rc.clone()).event_loop_waker(Box::new(NopWaker));

            // Optional: resources_dir could be used to configure initial resources.
            if let Some(_dir) = resources_dir {
                // Placeholder for future resource configuration.
            }

            let servo = builder.build();
            // Optional logging setup; comment out to reduce noise.
            // servo.setup_logging();

            // Create a WebView with our scale factor and initial size.
            let webview = WebViewBuilder::new(&servo)
                .hidpi_scale_factor(Scale::<f32, DeviceIndependentPixel, DevicePixel>::new(
                    self.scale_factor,
                ))
                .size(size)
                .build();

            // Focus and bring to top to drive readiness (harmless in headless).
            webview.focus_and_raise_to_top(true);

            self.servo = Some(servo);
            self.webview = Some(webview);
            Ok(())
        }

        fn load(&mut self, url: String) -> Result<(), String> {
            let webview = self
                .webview
                .as_ref()
                .ok_or_else(|| "Worker.load: webview not initialized".to_string())?;
            let url_parsed = Url::parse(&url)
                .or_else(|_| Url::parse("about:blank"))
                .map_err(|e| format!("Url parse failed: {e}"))?;
            webview.load(url_parsed);
            Ok(())
        }

        fn resize(&mut self, width: u32, height: u32) -> Result<(), String> {
            let webview = self
                .webview
                .as_ref()
                .ok_or_else(|| "Worker.resize: webview not initialized".to_string())?;

            let new_size = PhysicalSize::new(width.max(1), height.max(1));
            self.size = new_size;

            // Update the viewport rect and rendering context size.
            let mut rect = webview.rect();
            rect.set_size(euclid::size2(new_size.width as f32, new_size.height as f32));
            webview.move_resize(rect);
            webview.resize(new_size);

            Ok(())
        }

        fn spin_until<T>(
            &mut self,
            mut try_recv: impl FnMut() -> Option<T>,
            timeout: Duration,
        ) -> Option<T> {
            let deadline = Instant::now() + timeout;
            loop {
                // Run one tick of Servo's event loop.
                if let Some(servo) = self.servo.as_ref() {
                    servo.spin_event_loop();
                }
                // See if we got the result.
                if let Some(val) = try_recv() {
                    return Some(val);
                }
                if Instant::now() >= deadline {
                    return None;
                }
                // Throttle a bit to avoid busy-spinning.
                std::thread::sleep(Duration::from_millis(5));
            }
        }

        fn draw_now_png(&mut self) -> Result<FramePng, String> {
            let webview = self
                .webview
                .as_ref()
                .ok_or_else(|| "Worker.draw_now_png: webview not initialized".to_string())?;

            // Ask for a screenshot of the whole viewport. This will wait for the page
            // to reach a "ready" state inside Servo (images, fonts, etc).
            let (tx, rx) = mpsc::channel();
            webview.take_screenshot(None, move |res| {
                let _ = tx.send(res);
            });

            // Wait for the result while spinning Servo's event loop.
            let res = self
                .spin_until(|| rx.try_recv().ok(), Duration::from_secs(2))
                .ok_or_else(|| "Screenshot timeout".to_string())?;

            let img = res.map_err(|e| format!("Screenshot error: {:?}", e))?;
            let width = img.width();
            let height = img.height();

            // Encode to PNG for transport portability.
            let dynimg = DynamicImage::ImageRgba8(img);
            let mut buf = Vec::new();
            dynimg
                .write_to(&mut std::io::Cursor::new(&mut buf), ImageFormat::Png)
                .map_err(|e| format!("PNG encode failed: {e}"))?;

            Ok(FramePng {
                width,
                height,
                png: buf,
            })
        }
    }

    // The worker thread entry: receives commands and drives Servo on a dedicated thread.
    fn worker_thread(rx: Receiver<Cmd>) {
        let mut worker = Worker::new();

        while let Ok(cmd) = rx.recv() {
            match cmd {
                Cmd::Init {
                    width,
                    height,
                    scale_factor,
                    resources_dir,
                    user_agent,
                    init_scripts,
                    devtools_port,
                    reply,
                } => {
                    let res = worker.init(
                        width,
                        height,
                        scale_factor,
                        resources_dir,
                        user_agent,
                        init_scripts,
                        devtools_port,
                    );
                    let _ = reply.send(res.map_err(|e| e.to_string()));
                }
                Cmd::Load { url, reply } => {
                    let res = worker.load(url);
                    let _ = reply.send(res);
                }
                Cmd::Resize {
                    width,
                    height,
                    reply,
                } => {
                    let res = worker.resize(width, height);
                    let _ = reply.send(res);
                }
                Cmd::EvalScript { source: _, reply } => {
                    // TODO: Map into Servo evaluate_javascript if needed.
                    let _ = reply.send(Ok(()));
                }
                Cmd::DrawNow { reply } | Cmd::RequestDraw { reply } => {
                    let res = worker.draw_now_png().map_err(|e| e.to_string());
                    let _ = reply.send(res);
                }
                Cmd::Shutdown => {
                    // Drop Servo and WebView by clearing state, then exit thread.
                    worker.webview = None;
                    worker.servo = None;
                    break;
                }
            }
        }
    }

    pub struct ServoEngine {
        // Init/config
        resources_dir: Option<PathBuf>,
        user_agent: Option<String>,
        init_scripts: Vec<String>,
        devtools_port: Option<u16>,
        prefer_zero_copy: bool,

        // Viewport/surface
        width: u32,
        height: u32,
        scale_factor: f32,

        // Worker
        tx: Option<Sender<Cmd>>,
        handle: Option<JoinHandle<()>>,
        initialized: bool,
    }

    impl Default for ServoEngine {
        fn default() -> Self {
            Self {
                resources_dir: None,
                user_agent: None,
                init_scripts: vec![],
                devtools_port: None,
                prefer_zero_copy: false,
                width: 0,
                height: 0,
                scale_factor: 1.0,
                tx: None,
                handle: None,
                initialized: false,
            }
        }
    }

    impl ServoEngine {
        pub fn new() -> Self {
            Self::default()
        }

        fn ensure_worker(&mut self) -> Result<&Sender<Cmd>, EngineError> {
            if self.tx.is_none() {
                let (tx, rx) = mpsc::channel();
                let handle = thread::Builder::new()
                    .name("versoview-servo-worker".to_string())
                    .spawn(move || worker_thread(rx))
                    .map_err(|e| EngineError {
                        msg: format!("spawn worker failed: {e}"),
                    })?;
                self.tx = Some(tx);
                self.handle = Some(handle);
            }
            Ok(self.tx.as_ref().unwrap())
        }

        fn init_on_worker(&mut self) -> Result<(), EngineError> {
            if self.initialized {
                return Ok(());
            }
            let tx = self.ensure_worker()?;
            let (rtx, rrx) = mpsc::channel();

            let width = self.width.max(1);
            let height = self.height.max(1);
            let scale = if self.scale_factor.is_finite() && self.scale_factor > 0.0 {
                self.scale_factor
            } else {
                1.0
            };

            tx.send(Cmd::Init {
                width,
                height,
                scale_factor: scale,
                resources_dir: self.resources_dir.clone(),
                user_agent: self.user_agent.clone(),
                init_scripts: self.init_scripts.clone(),
                devtools_port: self.devtools_port,
                reply: rtx,
            })
            .map_err(|e| EngineError {
                msg: format!("send Init cmd failed: {e}"),
            })?;

            let res = rrx
                .recv()
                .map_err(|e| EngineError {
                    msg: format!("recv Init reply failed: {e}"),
                })?
                .map_err(|e| EngineError { msg: e })?;
            self.initialized = true;
            Ok(res)
        }
    }

    impl Engine for ServoEngine {
        fn init(&mut self, init: EngineInit) -> Result<EngineCapabilities, EngineError> {
            self.resources_dir = init.resources_dir;
            self.user_agent = init.user_agent;
            self.init_scripts = init.init_scripts;
            self.devtools_port = init.devtools_port;
            self.prefer_zero_copy = init.prefer_zero_copy;

            // Advertise compressed PNG frames for now. Zero-copy paths can be added later.
            Ok(EngineCapabilities {
                fd_passing: false,
                zero_copy_frames: false,
                compressed_frames: true,
                supported_compressed: vec![proto::CompressedFormat::Png],
            })
        }

        fn bind_surface(&mut self, surface: &proto::SurfaceDescriptor) -> Result<(), EngineError> {
            match surface {
                proto::SurfaceDescriptor::Fallback {
                    width,
                    height,
                    scale_factor,
                } => {
                    self.width = (*width).max(1);
                    self.height = (*height).max(1);
                    self.scale_factor = *scale_factor;
                }
                _ => {
                    if self.width == 0 || self.height == 0 {
                        self.width = 800;
                        self.height = 600;
                    }
                }
            }
            // Initialize the worker with current size.
            self.init_on_worker()?;
            Ok(())
        }

        fn load(&mut self, url_or_data: &str) -> Result<(), EngineError> {
            // For now, require a valid URL (including data: URLs). If invalid, fall back to about:blank upstream.
            self.init_on_worker()?;
            let tx = self.tx.as_ref().expect("worker initialized");
            let (rtx, rrx) = mpsc::channel();
            tx.send(Cmd::Load {
                url: url_or_data.to_string(),
                reply: rtx,
            })
            .map_err(|e| EngineError {
                msg: format!("send Load cmd failed: {e}"),
            })?;
            rrx.recv()
                .map_err(|e| EngineError {
                    msg: format!("recv Load reply failed: {e}"),
                })?
                .map_err(|e| EngineError { msg: e })
        }

        fn eval_script(&mut self, source: &str) -> Result<(), EngineError> {
            self.init_on_worker()?;
            let tx = self.tx.as_ref().expect("worker initialized");
            let (rtx, rrx) = mpsc::channel();
            tx.send(Cmd::EvalScript {
                source: source.to_string(),
                reply: rtx,
            })
            .map_err(|e| EngineError {
                msg: format!("send EvalScript cmd failed: {e}"),
            })?;
            rrx.recv()
                .map_err(|e| EngineError {
                    msg: format!("recv EvalScript reply failed: {e}"),
                })?
                .map_err(|e| EngineError { msg: e })
        }

        fn resize(&mut self, width: u32, height: u32) -> Result<(), EngineError> {
            self.width = width.max(1);
            self.height = height.max(1);
            self.init_on_worker()?;
            let tx = self.tx.as_ref().expect("worker initialized");
            let (rtx, rrx) = mpsc::channel();
            tx.send(Cmd::Resize {
                width: self.width,
                height: self.height,
                reply: rtx,
            })
            .map_err(|e| EngineError {
                msg: format!("send Resize cmd failed: {e}"),
            })?;
            rrx.recv()
                .map_err(|e| EngineError {
                    msg: format!("recv Resize reply failed: {e}"),
                })?
                .map_err(|e| EngineError { msg: e })
        }

        fn input_events(&mut self, _events: &[proto::IpcInputEvent]) -> Result<(), EngineError> {
            // TODO: Map Verso input events into Servo input events and forward to the compositor.
            Ok(())
        }

        fn request_draw(&mut self) -> Result<Option<EngineFrame>, EngineError> {
            // For now, same as draw_now: capture a frame immediately.
            self.draw_now()
        }

        fn draw_now(&mut self) -> Result<Option<EngineFrame>, EngineError> {
            self.init_on_worker()?;
            let tx = self.tx.as_ref().expect("worker initialized");
            let (rtx, rrx) = mpsc::channel();
            tx.send(Cmd::DrawNow { reply: rtx })
                .map_err(|e| EngineError {
                    msg: format!("send DrawNow cmd failed: {e}"),
                })?;
            let frame = rrx
                .recv()
                .map_err(|e| EngineError {
                    msg: format!("recv DrawNow reply failed: {e}"),
                })?
                .map_err(|e| EngineError { msg: e })?;

            Ok(Some(EngineFrame::CompressedImage {
                width: frame.width,
                height: frame.height,
                format: proto::CompressedFormat::Png,
                data: frame.png,
            }))
        }

        fn open_devtools(&mut self, requested: Option<u16>) -> Result<Option<u16>, EngineError> {
            // TODO: Wire Servo devtools port if desired.
            let chosen = match (requested, self.devtools_port) {
                (Some(0), Some(cfg)) => Some(cfg),
                (Some(0), None) => Some(0),
                (Some(p), _) => Some(p),
                (None, cfg) => cfg,
            };
            Ok(chosen)
        }

        fn shutdown(&mut self) -> Result<(), EngineError> {
            if let Some(tx) = self.tx.take() {
                let _ = tx.send(Cmd::Shutdown);
            }
            if let Some(handle) = self.handle.take() {
                let _ = handle.join();
            }
            self.initialized = false;
            Ok(())
        }
    }
}

#[cfg(feature = "servo-engine")]
pub use real::ServoEngine;

#[cfg(not(feature = "servo-engine"))]
mod shim {
    use super::*;
    use crate::engine::{DemoEngine, EngineCapabilities, EngineFrame, EngineInit};
    use verso_standalone::ipc_protocol as proto;

    // ServoEngine – shim that delegates to DemoEngine when `servo-engine` feature is disabled.
    pub struct ServoEngine {
        inner: DemoEngine,
    }

    impl ServoEngine {
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

        fn bind_surface(&mut self, surface: &proto::SurfaceDescriptor) -> Result<(), EngineError> {
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

        fn input_events(&mut self, events: &[proto::IpcInputEvent]) -> Result<(), EngineError> {
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
}

#[cfg(not(feature = "servo-engine"))]
pub use shim::ServoEngine;
