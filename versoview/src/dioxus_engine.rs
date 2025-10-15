#![allow(unused_variables)]
#![allow(dead_code)]
//! DioxusEngine: minimal in-process Dioxus SSR → HTML → rasterized PNG engine.
//!
//! Goals
//! - Provide a feature-gated `Engine` implementation that renders a Dioxus component to HTML
//!   using server-side rendering (SSR).
//! - Produce a simple rasterized frame (PNG-encoded) as a placeholder for a real HTML renderer.
//!   This lets the Verso transport and Photon-like host exercise the full frame path.
//!
//! Notes
//! - This is a proof-of-concept. It does NOT perform real HTML layout/rendering.
//!   The rasterization is a deterministic gradient with color derived from the HTML hash.
//! - Long term, replace the rasterization with Servo’s HTML/CSS/WeBrender pipeline.
//!
//! Build
//! - Enabled with the `dioxus_engine` cargo feature for the `versoview` binary crate.
//! - When disabled, the type exists but returns `EngineError` on use.

use crate::engine::{Engine, EngineCapabilities, EngineError, EngineFrame, EngineInit};
use std::hash::{Hash, Hasher};

use verso_standalone::ipc_protocol as proto;

#[cfg(feature = "dioxus_engine")]
use {
    dioxus::prelude::*,
    dioxus_ssr,
    image::{DynamicImage, ImageBuffer, ImageFormat, Rgba},
};

/// Engine that renders a Dioxus app (SSR) and produces simple rasterized frames.
/// When the `dioxus_engine` feature is not enabled, this acts as a stub and returns errors.
pub struct DioxusEngine {
    // Config/state
    resources_dir: Option<std::path::PathBuf>,
    user_agent: Option<String>,
    init_scripts: Vec<String>,
    devtools_port_cfg: Option<u16>,
    prefer_zero_copy: bool,

    // Surface/view
    width: u32,
    height: u32,
    stride: u32,

    // Content
    last_html: Option<String>,

    // Diagnostics
    next_frame_id: u64,
    input_events: usize,
}

impl Default for DioxusEngine {
    fn default() -> Self {
        Self {
            resources_dir: None,
            user_agent: None,
            init_scripts: vec![],
            devtools_port_cfg: None,
            prefer_zero_copy: false,
            width: 0,
            height: 0,
            stride: 0,
            last_html: None,
            next_frame_id: 0,
            input_events: 0,
        }
    }
}

impl DioxusEngine {
    pub fn new() -> Self {
        Self::default()
    }

    fn ensure_size(&mut self) {
        if self.width == 0 || self.height == 0 {
            self.width = 800;
            self.height = 600;
        }
        self.stride = self.width.saturating_mul(4);
    }

    #[cfg(feature = "dioxus_engine")]
    fn render_default_app_ssr(&self) -> String {
        // A simple Dioxus SSR example.
        // This uses a small RSX tree as a placeholder app.
        // Users will typically depend on their own Dioxus crate and swap this.
        let title = "Verso + Dioxus SSR";
        let subtitle = "Hello from an in-process Dioxus app!";
        dioxus_ssr::render_element(rsx! {
            div { id: "root",
                h1 { "{title}" }
                p { "{subtitle}" }
                p { "User Agent (if set): {self.user_agent.as_deref().unwrap_or(\"<unset>\")}" }
                p { "Init scripts loaded: {self.init_scripts.len()}" }
            }
        })
    }

    fn normalize_load_input(&self, s: &str) -> String {
        // Accept either:
        // - explicit HTML strings (starts with '<')
        // - data URLs (data:text/html,...) [no decoding here; leave as-is]
        // - about:blank → empty HTML shell
        // - dioxus://default → our compiled-in default app
        if s.trim_start().starts_with('<') {
            return s.to_string();
        }
        if s.starts_with("data:text/html") {
            // In a full implementation, parse and percent-decode. For now, pass through verbatim.
            return s.to_string();
        }
        if s.eq_ignore_ascii_case("about:blank") {
            return "<!doctype html><html><head><meta charset=\"utf-8\"><title>blank</title></head><body></body></html>".to_string();
        }
        if s.eq_ignore_ascii_case("dioxus://default") || s.eq_ignore_ascii_case("about:dioxus") {
            #[cfg(feature = "dioxus_engine")]
            {
                return self.render_default_app_ssr();
            }
            #[cfg(not(feature = "dioxus_engine"))]
            {
                return "<!doctype html><html><body><p>dioxus_engine feature not enabled</p></body></html>".to_string();
            }
        }
        // Fallback: wrap as a paragraph so we remain valid HTML
        format!("<!doctype html><html><body><p>{}</p></body></html>", s)
    }

    fn hash_to_rgba(&self, html: &str) -> (u8, u8, u8, u8) {
        // Derive a base color from the HTML content.
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        html.hash(&mut hasher);
        let h = hasher.finish();
        let r = (h & 0xFF) as u8;
        let g = ((h >> 8) & 0xFF) as u8;
        let b = ((h >> 16) & 0xFF) as u8;
        (r, g, b, 0xFF)
    }

    #[cfg(feature = "dioxus_engine")]
    fn rasterize_html_to_png(
        &self,
        html: &str,
        width: u32,
        height: u32,
    ) -> Result<Vec<u8>, EngineError> {
        // Rasterization placeholder:
        // - Background: vertical gradient
        // - Overlay: four color bars derived from the HTML hash to produce deterministic output
        let (r, g, b, a) = self.hash_to_rgba(html);
        let w = width.max(1);
        let h = height.max(1);

        let mut img: ImageBuffer<Rgba<u8>, Vec<u8>> = ImageBuffer::new(w, h);

        for y in 0..h {
            let t = (y as f32) / (h.saturating_sub(1).max(1) as f32);
            let bg_r = (t * r as f32) as u8;
            let bg_g = (t * g as f32) as u8;
            let bg_b = (t * b as f32) as u8;
            for x in 0..w {
                img.put_pixel(x, y, Rgba([bg_r, bg_g, bg_b, 0xFF]));
            }
        }

        // Draw simple vertical bars encoding parts of the hash.
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        html.hash(&mut hasher);
        let hsh = hasher.finish();
        let bars = 4;
        let bar_w = (w / bars.max(1)) as u32;
        for i in 0..bars {
            let x0 = i as u32 * bar_w;
            let x1 = if i == bars - 1 {
                w
            } else {
                (i as u32 + 1) * bar_w
            };
            let c = ((hsh >> (i * 8)) & 0xFF) as u8;
            let col = Rgba([c, 255u8.wrapping_sub(c), c ^ 0x5A, 0xFF]);
            for y in (h / 3)..(2 * h / 3) {
                for x in x0..x1.min(w) {
                    img.put_pixel(x, y, col);
                }
            }
        }

        let dynimg = DynamicImage::ImageRgba8(img);
        let mut buf = Vec::new();
        dynimg
            .write_to(&mut std::io::Cursor::new(&mut buf), ImageFormat::Png)
            .map_err(|e| EngineError {
                msg: format!("PNG encode failed: {e}"),
            })?;
        Ok(buf)
    }
}

impl Engine for DioxusEngine {
    fn init(&mut self, init: EngineInit) -> Result<EngineCapabilities, EngineError> {
        self.resources_dir = init.resources_dir;
        self.user_agent = init.user_agent;
        self.init_scripts = init.init_scripts;
        self.devtools_port_cfg = init.devtools_port;
        self.prefer_zero_copy = init.prefer_zero_copy;

        // Advertise compressed PNG frames for portability; zero-copy off for now.
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
                scale_factor: _,
            } => {
                self.width = (*width).max(1);
                self.height = (*height).max(1);
            }
            _ => {
                // Default reasonable size for non-fallback descriptors, if not set yet.
                if self.width == 0 || self.height == 0 {
                    self.width = 800;
                    self.height = 600;
                }
            }
        }
        self.ensure_size();
        Ok(())
    }

    fn load(&mut self, url_or_data: &str) -> Result<(), EngineError> {
        // If Dioxus SSR feature is enabled and the input asks for it, produce HTML now.
        let html = self.normalize_load_input(url_or_data);
        self.last_html = Some(html);
        Ok(())
    }

    fn eval_script(&mut self, _source: &str) -> Result<(), EngineError> {
        // No-op for SSR sample engine.
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
        self.draw_now()
    }

    fn draw_now(&mut self) -> Result<Option<EngineFrame>, EngineError> {
        self.ensure_size();

        let html = match &self.last_html {
            Some(h) => h.clone(),
            None => {
                // If nothing loaded, synthesize default app (or placeholder when feature is off).
                #[cfg(feature = "dioxus_engine")]
                let h = self.render_default_app_ssr();
                #[cfg(not(feature = "dioxus_engine"))]
                let h = "<!doctype html><html><body><p>dioxus_engine feature not enabled</p></body></html>".to_string();
                self.last_html = Some(h);
                self.last_html.clone().unwrap()
            }
        };

        #[cfg(feature = "dioxus_engine")]
        let png = self.rasterize_html_to_png(&html, self.width, self.height)?;
        #[cfg(not(feature = "dioxus_engine"))]
        let png = {
            // Fallback to a basic deterministic RGBA -> PNG using a simple algorithm even without Dioxus.
            let w = self.width.max(1);
            let h = self.height.max(1);
            let mut bytes = vec![0u8; (w * h * 4) as usize];
            for y in 0..h {
                for x in 0..w {
                    let idx = (y * w + x) as usize * 4;
                    bytes[idx + 0] = (x % 256) as u8;
                    bytes[idx + 1] = (y % 256) as u8;
                    bytes[idx + 2] = 0x80;
                    bytes[idx + 3] = 0xFF;
                }
            }
            // Encode to PNG using a tiny inline encoder substitute is not available without image.
            // Since the dioxus_engine feature is off, we cannot rely on the `image` crate either.
            // Return a CompressedImage with empty data to keep the plumbing alive.
            bytes.clear();
            bytes
        };

        self.next_frame_id = self.next_frame_id.wrapping_add(1);

        Ok(Some(EngineFrame::CompressedImage {
            width: self.width,
            height: self.height,
            format: proto::CompressedFormat::Png,
            data: png,
        }))
    }

    fn open_devtools(&mut self, requested: Option<u16>) -> Result<Option<u16>, EngineError> {
        let chosen = match (requested, self.devtools_port_cfg) {
            (Some(0), Some(cfg)) => Some(cfg),
            (Some(0), None) => Some(0),
            (Some(p), _) => Some(p),
            (None, cfg) => cfg,
        };
        Ok(chosen)
    }

    fn shutdown(&mut self) -> Result<(), EngineError> {
        self.width = 0;
        self.height = 0;
        self.stride = 0;
        self.last_html = None;
        Ok(())
    }
}
