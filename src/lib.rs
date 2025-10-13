//! Verso (Standalone) – Winit-based runtime helpers
//!
//! This crate provides framework-agnostic helpers for configuring and launching
//! Verso-based webviews while your application drives a native event loop using
//! `winit`. It intentionally avoids coupling to any application framework.
//!
//! What this crate helps you do:
//! - Configure the Verso executable location (the `versoview` controller).
//! - Optionally set a resources directory for Verso-managed assets.
//! - Optionally enable a devtools port for connecting via Firefox (`about:debugging`).
//!
//! Expectations and scope:
//! - You own the event loop and windowing with `winit`.
//! - You use the `verso` crate directly to create and control webviews.
//! - This crate does not provide bundling/packaging or framework integrations.
//!
//! Getting started (high level):
//! 1. Add this crate and the `verso` crate to your project.
//! 2. Ensure the `versoview` executable is discoverable at runtime (for example,
//!    next to your app binary) or set its path with `set_verso_path()` at startup.
//! 3. Optionally call `set_verso_resource_directory()` and `set_verso_devtools_port()`
//!    before creating any webviews.
//! 4. Use `winit` to run your event loop and use the `verso` crate to create and manage webviews.
//!
//! Notes:
//! - This crate contains no framework-specific APIs; it is designed to be used
//!   with your own `winit` application structure.
//! - Any previous framework-specific instructions have been removed in this fork.
//!
//! Platform support:
//! - Desktop platforms supported by `verso` and `winit` (Linux, Windows, macOS).

use std::{
    env::current_exe,
    path::{Path, PathBuf},
    sync::{Mutex, OnceLock},
};

mod winit_runtime;

/// Public API re-exports for the winit integration.
pub use crate::winit_runtime::{
    NativeSurfaceHandles, VersoWebviewHost, WinitRuntime, WinitWindowHandle, logical_to_physical,
    map_winit_key, map_winit_mouse_button,
};

static VERSO_PATH: OnceLock<PathBuf> = OnceLock::new();

/// Sets the Verso executable path to ues for the webviews,
/// must be called before you create any webviews if you don't have the `externalBin` setup
///
/// Example:
///
/// Configure the Verso executable path early in your app startup (before creating any webviews).
pub fn set_verso_path(path: impl Into<PathBuf>) {
    VERSO_PATH
        .set(path.into())
        .expect("Verso path is already set, you can't set it multiple times");
}

fn get_verso_path() -> &'static Path {
    VERSO_PATH.get_or_init(|| {
        relative_command_path("versoview").expect(
            "Verso path not set! You need to call set_verso_path before creating any webviews!",
        )
    })
}

fn relative_command_path(name: &str) -> Option<PathBuf> {
    let extension = if cfg!(windows) { ".exe" } else { "" };
    current_exe()
        .ok()?
        .parent()?
        .join(format!("{name}{extension}"))
        .canonicalize()
        .ok()
}

static VERSO_RESOURCES_DIRECTORY: Mutex<Option<PathBuf>> = Mutex::new(None);

/// Sets the Verso resources directory to ues for the webviews,
/// note this only affects webviews created after you set this
///
/// Example:
///
/// Set the resources directory before creating any Verso webviews so new instances will use it.
pub fn set_verso_resource_directory(path: impl Into<PathBuf>) {
    VERSO_RESOURCES_DIRECTORY
        .lock()
        .unwrap()
        .replace(path.into());
}

fn get_verso_resource_directory() -> Option<PathBuf> {
    VERSO_RESOURCES_DIRECTORY.lock().unwrap().clone()
}

/// Initialization script string to bootstrap the Verso invoke system in created webviews.
///
/// Note:
///
/// This initialization script is framework-agnostic and intended to be injected into Verso webviews
/// by your application at the appropriate time.
pub const INVOKE_SYSTEM_SCRIPTS: &str = include_str!("./invoke-system-initialization-script.js");

static DEV_TOOLS_PORT: Mutex<Option<u16>> = Mutex::new(None);

/// Sets the Verso devtools port to ues for the webviews, 0 for random port,
/// note this only affects webviews created after you set this
///
/// Since Verso doesn't have devtools built-in,
/// you need to use the one from Firefox from the `about:debugging` page,
/// this setting allows you to let verso open a port for it
pub fn set_verso_devtools_port(port: u16) {
    DEV_TOOLS_PORT.lock().unwrap().replace(port);
}

fn get_verso_devtools_port() -> Option<u16> {
    *DEV_TOOLS_PORT.lock().unwrap()
}

pub mod winit_api {
    use std::marker::PhantomData;

    /// Minimal, framework-agnostic types to hint winit usage without imposing an event loop policy.
    pub struct Api {
        _marker: PhantomData<fn() -> winit::event_loop::EventLoop<()>>,
    }

    impl Api {
        /// Construct the minimal API wrapper.
        pub fn new() -> Self {
            Self {
                _marker: PhantomData,
            }
        }
    }
}
