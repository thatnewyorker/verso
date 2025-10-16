/*!
Engine integration abstraction for the `versoview` server and a demo engine.

This module defines:
- `Engine` trait: a small surface that versoview can call to drive a rendering engine.
- `EngineInit`, `EngineCapabilities`: configuration and negotiated capabilities.
- `EngineFrame` and related handle/resource types: portable frame outputs that main.rs
  can convert into IPC `FrameDescriptor`s and optional out-of-band handles.
- `DemoEngine`: a simple reference engine that generates deterministic RGBA gradients
  and, when enabled, can produce a shared-memory handle (memfd on Unix) for zero-copy.

Design goals:
- Decouple versoview’s IPC server logic from any specific rendering backend or OS-specific
  handle creation details (memfd, dmabuf, IOSurface, DXGI shared handles).
- Keep platform-specific logic inside the engine implementation and expose a uniform,
  high-level `EngineFrame` that the IPC server converts to protocol descriptors.

Notes:
- The demo engine favors simplicity and determinism over performance.
- For zero-copy demos on Unix, enable the `zero_copy` cargo feature for the binary crate.
*/

use std::fmt;
use std::path::PathBuf;

use photon_standalone::ipc_protocol as proto;

#[cfg(all(unix, feature = "zero_copy"))]
use std::os::unix::io::OwnedFd;

#[cfg(all(unix, feature = "zero_copy"))]
use std::io::Write;

#[cfg(all(unix, feature = "zero_copy"))]
use nix::sys::memfd::{MemfdCreateFlag, memfd_create};

/// Initialization parameters for an engine instance.
///
/// The versoview server should construct this from the handshake/config it receives.
#[derive(Debug, Clone, Default)]
pub struct EngineInit {
    /// Optional resource directory for assets, preload scripts, etc.
    pub resources_dir: Option<PathBuf>,
    /// Optional preferred devtools port; 0 means "choose an ephemeral port".
    pub devtools_port: Option<u16>,
    /// Optional user agent override.
    pub user_agent: Option<String>,
    /// Initialization scripts to inject early in page lifecycle.
    pub init_scripts: Vec<String>,
    /// Hint to prefer zero-copy paths when available (engine may ignore).
    pub prefer_zero_copy: bool,
}

/// Engine capability flags the server can advertise to the client.
///
/// This mirrors the intent of the IPC `Capabilities` type, but at the engine boundary.
#[derive(Debug, Clone, Default)]
pub struct EngineCapabilities {
    pub fd_passing: bool,
    pub zero_copy_frames: bool,
    pub compressed_frames: bool,
    pub supported_compressed: Vec<proto::CompressedFormat>,
}

/// Errors surfaced by engine operations.
#[derive(Debug)]
pub struct EngineError {
    pub msg: String,
}

impl fmt::Display for EngineError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.msg)
    }
}

impl std::error::Error for EngineError {}

impl EngineError {
    pub fn new(msg: impl Into<String>) -> Self {
        Self { msg: msg.into() }
    }
}

/// A single plane in a Linux dmabuf image with an owned handle (Unix-only).
#[cfg(all(unix, feature = "zero_copy"))]
#[derive(Debug)]
pub struct DmabufPlaneHandle {
    pub fd: OwnedFd,
    pub offset: u32,
    pub stride: u32,
    pub plane_index: u8,
}

/// Portable representation of a frame resource produced by the engine.
///
/// The IPC server converts this into an appropriate `proto::FrameDescriptor` and,
/// when applicable, attaches out-of-band handles to the transport.
#[derive(Debug)]
pub enum EngineFrame {
    /// A raw uncompressed bitmap owned by the engine (inline copy).
    SharedMemoryInline {
        width: u32,
        height: u32,
        stride: u32,
        format: proto::PixelFormat,
        /// Full buffer contents (row-major).
        bytes: Vec<u8>,
    },

    /// A shared-memory buffer with an OS handle that the host can import (Unix-only demo).
    #[cfg(all(unix, feature = "zero_copy"))]
    SharedMemoryHandle {
        width: u32,
        height: u32,
        stride: u32,
        format: proto::PixelFormat,
        /// The memfd (or other shm fd) that backs this image.
        fd: OwnedFd,
        /// Total number of valid bytes in the shm region.
        size: u64,
    },

    /// Linux dmabuf-based frame (Unix-only).
    #[cfg(all(unix, feature = "zero_copy"))]
    LinuxDmabuf {
        width: u32,
        height: u32,
        fourcc: u32,
        modifier: u64,
        planes: Vec<DmabufPlaneHandle>,
        /// Optional explicit fence fd for synchronization.
        fence: Option<OwnedFd>,
    },

    /// macOS IOSurface-based frame (placeholder; implement in a macOS engine).
    #[cfg(target_os = "macos")]
    MacOsIoSurface {
        width: u32,
        height: u32,
        pixel_format: u32,
        /// An opaque IOSurface identifier or mach port (representation TBD).
        io_surface_id: u32,
    },

    /// Windows DXGI shared-handle-based frame (placeholder; implement on Windows).
    #[cfg(target_os = "windows")]
    WindowsDxgiSharedHandle {
        width: u32,
        height: u32,
        dxgi_format: u32,
        /// Opaque duplicated HANDLE value (representation TBD).
        handle_value: u64,
    },

    /// Compressed fallback (e.g., PNG/WEBP/JPEG).
    CompressedImage {
        width: u32,
        height: u32,
        format: proto::CompressedFormat,
        /// Compressed payload bytes.
        data: Vec<u8>,
    },
}

/// Minimal engine interface that `versoview` expects to drive.
pub trait Engine: Send {
    /// Initialize the engine. Returns negotiated capabilities.
    fn init(&mut self, init: EngineInit) -> Result<EngineCapabilities, EngineError>;

    /// Bind a rendering surface (or accept a fallback descriptor).
    fn bind_surface(&mut self, surface: &proto::SurfaceDescriptor) -> Result<(), EngineError>;

    /// Navigate to a URL or load a resource/string data (data URLs allowed).
    fn load(&mut self, url_or_data: &str) -> Result<(), EngineError>;

    /// Evaluate JavaScript in the page context.
    fn eval_script(&mut self, source: &str) -> Result<(), EngineError>;

    /// Update the viewport/backing store size (physical pixels).
    fn resize(&mut self, width: u32, height: u32) -> Result<(), EngineError>;

    /// Feed a batch of input events.
    fn input_events(&mut self, events: &[proto::IpcInputEvent]) -> Result<(), EngineError>;

    /// Ask the engine to schedule a frame; may return a frame immediately if available.
    fn request_draw(&mut self) -> Result<Option<EngineFrame>, EngineError>;

    /// Draw/present immediately; may return the frame payload if available.
    fn draw_now(&mut self) -> Result<Option<EngineFrame>, EngineError>;

    /// Optionally open DevTools and return the listening port (or None if unsupported).
    fn open_devtools(&mut self, requested: Option<u16>) -> Result<Option<u16>, EngineError> {
        let _ = requested;
        Ok(None)
    }

    /// Tear down engine resources.
    fn shutdown(&mut self) -> Result<(), EngineError>;
}

/// A simple reference implementation that generates deterministic RGBA gradients.
///
/// Behavior:
/// - Tracks a logical size; draw operations produce a gradient buffer for that size.
/// - When `prefer_zero_copy` is true and supported, can return a `SharedMemoryHandle`
///   using a memfd on Unix so the host can import/mmap it without copying.
/// - Otherwise returns an inline `SharedMemoryInline` buffer.
/// - Does not perform real navigation or scripting; only logs/records operations.
///
/// Intended for protocol/transport verification and demo flows.
#[derive(Default)]
pub struct DemoEngine {
    // Configuration and state
    resources_dir: Option<PathBuf>,
    user_agent: Option<String>,
    init_scripts: Vec<String>,
    devtools_port_cfg: Option<u16>,
    prefer_zero_copy: bool,

    // Surface / view state
    width: u32,
    height: u32,
    stride: u32, // bytes per row for RGBA (width * 4)
    last_url: Option<String>,
    next_frame_id: u64,

    // Input diagnostics
    input_events: usize,
}

impl DemoEngine {
    pub fn new() -> Self {
        Self {
            stride: 0,
            ..Default::default()
        }
    }

    fn ensure_size(&mut self) {
        if self.width == 0 || self.height == 0 {
            self.width = 64;
            self.height = 64;
        }
        self.stride = self.width.saturating_mul(4);
    }

    fn make_rgba_gradient(&self) -> Vec<u8> {
        let w = self.width as usize;
        let h = self.height as usize;
        let stride = self.stride as usize;
        let mut bytes = vec![0u8; stride * h];
        for y in 0..h {
            for x in 0..w {
                let idx = y * stride + x * 4;
                // Deterministic pattern mirrors the test memfd verifier in the host.
                bytes[idx + 0] = x as u8; // R
                bytes[idx + 1] = y as u8; // G
                bytes[idx + 2] = 0x80; // B
                bytes[idx + 3] = 0xFF; // A
            }
        }
        bytes
    }

    #[cfg(all(unix, feature = "zero_copy"))]
    fn make_memfd_with_bytes(
        &self,
        name: &str,
        data: &[u8],
    ) -> Result<(OwnedFd, u64), EngineError> {
        use std::ffi::CString;
        use std::os::unix::io::FromRawFd;

        let cname = CString::new(name).map_err(|e| EngineError::new(format!("memfd name: {e}")))?;
        let fd = memfd_create(&cname, MemfdCreateFlag::empty())
            .map_err(|e| EngineError::new(format!("memfd_create: {e}")))?;

        // SAFETY: `fd` is a fresh, owned file descriptor.
        let mut file = unsafe { <std::fs::File as std::os::unix::io::FromRawFd>::from_raw_fd(fd) };

        let size = data.len() as u64;
        file.set_len(size)
            .map_err(|e| EngineError::new(format!("memfd set_len: {e}")))?;
        file.write_all(data)
            .map_err(|e| EngineError::new(format!("memfd write_all: {e}")))?;

        // Convert to OwnedFd by re-borrowing raw fd from file.
        let raw_fd = file.as_raw_fd();
        // Avoid closing twice: prevent file from closing raw_fd on drop by forgetting it.
        std::mem::forget(file);

        // SAFETY: raw_fd is valid and currently owned by this function.
        let owned = unsafe { OwnedFd::from_raw_fd(raw_fd) };
        Ok((owned, size))
    }
}

impl Engine for DemoEngine {
    fn init(&mut self, init: EngineInit) -> Result<EngineCapabilities, EngineError> {
        self.resources_dir = init.resources_dir;
        self.user_agent = init.user_agent;
        self.init_scripts = init.init_scripts;
        self.devtools_port_cfg = init.devtools_port;
        self.prefer_zero_copy = init.prefer_zero_copy;

        // Advertise capabilities: demo supports compressed fallback and inline RGBA.
        // Zero-copy depends on build + platform; we only claim it when Unix + feature.
        let mut caps = EngineCapabilities {
            fd_passing: cfg!(all(unix, feature = "zero_copy")),
            zero_copy_frames: cfg!(all(unix, feature = "zero_copy")),
            compressed_frames: true,
            supported_compressed: vec![proto::CompressedFormat::Png],
        };

        // If caller indicates zero-copy is preferred but not compiled-in, keep flags false.
        if init.prefer_zero_copy && !caps.zero_copy_frames {
            // no-op, demo can't honor prefer_zero_copy without feature support
        }

        Ok(caps)
    }

    fn bind_surface(&mut self, surface: &proto::SurfaceDescriptor) -> Result<(), EngineError> {
        match surface {
            proto::SurfaceDescriptor::Fallback {
                width,
                height,
                scale_factor: _,
            } => {
                self.width = (*width).max(1);
                self.height = (*height).max(1);
            }
            // For the demo engine, treat all other descriptors as an initial size hint.
            _ => {
                // Default minimal size if none provided via a fallback descriptor.
                if self.width == 0 || self.height == 0 {
                    self.width = 64;
                    self.height = 64;
                }
            }
        }
        self.ensure_size();
        Ok(())
    }

    fn load(&mut self, url_or_data: &str) -> Result<(), EngineError> {
        self.last_url = Some(url_or_data.to_string());
        Ok(())
    }

    fn eval_script(&mut self, _source: &str) -> Result<(), EngineError> {
        // Demo: accept scripts but do nothing.
        Ok(())
    }

    fn resize(&mut self, width: u32, height: u32) -> Result<(), EngineError> {
        self.width = width.max(1);
        self.height = height.max(1);
        self.ensure_size();
        Ok(())
    }

    fn input_events(&mut self, events: &[proto::IpcInputEvent]) -> Result<(), EngineError> {
        self.input_events += events.len();
        Ok(())
    }

    fn request_draw(&mut self) -> Result<Option<EngineFrame>, EngineError> {
        // Demo: produce a frame immediately.
        self.draw_now()
    }

    fn draw_now(&mut self) -> Result<Option<EngineFrame>, EngineError> {
        self.ensure_size();

        // Produce a deterministic RGBA gradient.
        let bytes = self.make_rgba_gradient();
        self.next_frame_id = self.next_frame_id.wrapping_add(1);

        // Prefer zero-copy when available and requested.
        #[cfg(all(unix, feature = "zero_copy"))]
        if self.prefer_zero_copy {
            let (fd, size) = self.make_memfd_with_bytes("versoview_demo", &bytes)?;
            return Ok(Some(EngineFrame::SharedMemoryHandle {
                width: self.width,
                height: self.height,
                stride: self.stride,
                format: proto::PixelFormat::Rgba8888,
                fd,
                size,
            }));
        }

        // Fallback to inline shared-memory
        Ok(Some(EngineFrame::SharedMemoryInline {
            width: self.width,
            height: self.height,
            stride: self.stride,
            format: proto::PixelFormat::Rgba8888,
            bytes,
        }))
    }

    fn open_devtools(&mut self, requested: Option<u16>) -> Result<Option<u16>, EngineError> {
        // Demo: honor requested 0 => allocate ephemeral (simulate with a fixed) or return the config.
        let chosen = match (requested, self.devtools_port_cfg) {
            (Some(0), Some(cfg)) => Some(cfg),
            (Some(0), None) => Some(0),
            (Some(p), _) => Some(p),
            (None, cfg) => cfg,
        };
        Ok(chosen)
    }

    fn shutdown(&mut self) -> Result<(), EngineError> {
        // No resources to release in the demo engine beyond state reset.
        self.width = 0;
        self.height = 0;
        self.stride = 0;
        Ok(())
    }
}
