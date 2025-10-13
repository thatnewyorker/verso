# verso-standalone – Winit-based runtime helpers

verso-standalone is a standalone, framework-agnostic runtime scaffold around [Verso](https://github.com/tauri-apps/verso) using a native event loop provided by [winit](https://github.com/rust-windowing/winit). It removes any Tauri-specific integration and focuses on helping you:

- Configure and launch Verso-based webviews.
- Manage runtime paths (executable/resources) and developer tooling.
- Integrate cleanly with your own windowing/event loop built on `winit`.

If you previously used this project as a Tauri runtime, note that all Tauri-specific APIs and instructions have been removed in this fork.

---

## What is Verso?

[Verso](https://github.com/tauri-apps/verso) is a browser-like webview component backed by the [Servo](https://servo.org/) engine. Its APIs are intentionally lower-level than typical “webview” crates because Servo expects the embedder to drive much of the environment. Verso wraps the complexity and exposes a simpler API for creating a window+webview pair and interacting with it.

This crate provides light, framework-agnostic glue on top of Verso for desktop apps that run their own event loops via `winit`.

---

## Current status

- Tauri dependencies and examples have been removed.
- `winit` is the recommended windowing and event loop backend.
- The crate exports simple helpers for:
  - Setting the path to the Verso executable (the `versoview` controller).
  - Setting an optional resources directory for Verso.
  - Enabling Servo devtools via a TCP port (connect through Firefox’s `about:debugging`).
- A small `winit_api` module exists as a placeholder for future, higher-level integrations. You should currently plan on driving `winit` and `verso` directly in your application.
- A new `winit_runtime` module provides initial types (`WinitRuntime`, `WinitWindowHandle`, `VersoWebviewHost`) and mapping helpers. These are re-exported from the crate root for convenience.
- A minimal example is available under `examples/winit_hello` showing the intended flow; it's a scaffold until the runtime's window creation and event loop are fully implemented.

This project is evolving toward a clean, standalone layering over `verso` and `winit` with minimal opinions. Expect breaking changes as the API is refined.

---

## When to use this crate

Use this crate if you:

- Want to drive your application lifecycle with `winit` and integrate a Servo-based webview through Verso.
- Need a small set of convenience functions for locating/setting Verso’s external binary and resources in a consistent way.
- Don’t want a framework to manage your application lifecycle (you will write your own `winit` event loop, state management, etc.).

If you need high-level app scaffolding (menus, trays, plugins, bundling) supplied by a full framework, this crate intentionally does not provide those.

---

## Getting started

High-level steps to integrate in your app:

1. Add this crate as a dependency to your project.
2. Ensure the `versoview` executable is available at runtime.
   - Option A: Place it next to your app’s executable.
   - Option B: Configure an explicit path at startup (see “Runtime configuration helpers” below).
3. Optionally set a dedicated resources directory for Verso (e.g., assets, preload scripts).
4. Use `winit` to create and run your event loop.
5. Use the `verso` crate to build your webview(s), then interact with them as needed (navigation, scripting, window events).
6. For devtools, pick a port (or 0 for random) and connect with Firefox `about:debugging`.

Notes:
- This crate doesn’t currently expose window-building helpers; use the `verso` crate directly to construct windows/webviews.
- The `winit_api` module is a placeholder for future convenience layers and is not required for basic usage.
- See `examples/winit_hello` for a minimal skeleton example of the intended integration flow.

---

## Runtime configuration helpers

These helpers are provided to control how and where Verso is located and executed at runtime. Call them early in your app’s startup sequence (before creating any webviews):

- Set the Verso executable path:
  - If you don’t call this, the crate assumes the `versoview` binary is next to your app executable and uses that path.
- Set the Verso resources directory:
  - A path to assets used by Verso (e.g., content served to the webview).
- Enable devtools by port:
  - Use a fixed port for stable tooling, or `0` to request an available random port.
  - Connect from Firefox via `about:debugging` → “Connect” to localhost:port.

These APIs are framework-agnostic and do not depend on Tauri.

---

## Architecture overview

Your application owns the event loop and windowing:

- Event loop:
  - Use `winit::event_loop::EventLoop` and `EventLoop::run` to drive your app.
  - Handle input, window lifecycle, and redraw events in the `winit` event loop.
- Webview:
  - Use the `verso` crate to create a Verso `VersoviewController`, which manages a native window + embedded Servo instance.
- Communication and async work:
  - Spawn background tasks (e.g., with `std::thread` or `tokio`) and post messages back into your main thread as needed (e.g., using channels).
  - If you need to trigger UI actions from other threads, dispatch back to your main thread and interact with your Verso controller there.

This separation keeps your app free from any specific application framework and allows you to compose only the building blocks you need.

---

## Devtools

Verso doesn’t ship its own devtools UI. Instead:

- Configure a devtools port at startup (0 for random or a specific port).
- Open Firefox and navigate to `about:debugging`.
- Add a connection to `localhost:<port>` and inspect your webview.

This works across platforms currently supported by Verso.

---

## Platform support

- Desktop platforms (Linux, Windows, macOS) as supported by `verso` and `winit`.
- Mobile targets (Android/iOS) are not supported by this crate at this time.

---

## Migration notes (from Tauri integration)

If you are migrating from a previous Tauri-based setup:

- Remove all `tauri` and `tauri-*` crates from your app’s `Cargo.toml`.
- Drop any Tauri-specific build scripts and configuration.
- Introduce `winit` as your event loop and windowing backend.
- Use the `verso` crate directly to create the webview.
- Use the helpers in this crate to configure Verso’s executable and resources.
- Replace any Tauri-specific IPC or command handlers with your own message passing or webview bindings (e.g., postMessage-style messages, custom protocols, or direct function calls as appropriate for your app).

---

## Testing and CI

- Run the test suite (if present in your project) with:
  - `RUST_BACKTRACE=full cargo nextest run --workspace --all-targets`
- This crate itself contains minimal testable surface after the Tauri removal. As features are added, tests and docs will be expanded.

---

## Known limitations

- This crate currently provides limited convenience APIs. It intentionally leaves application structure (event loop, state, IPC) to your code.
- You must manage your own packaging/bundling and distribution pipeline.
- IPC and webview bindings are the responsibility of your application (e.g., define a messaging protocol between your UI and Rust).

---

## Roadmap

- Expand the `winit` integration with ergonomic helpers around:
  - Event loop proxies and cross-thread messaging patterns.
  - Window/webview orchestration (creation, teardown, multiple windows).
  - Common tasks (navigation, script evaluation, basic file dialogs via optional dependencies).
- Improve documentation and examples that illustrate real-world patterns with `winit` + `verso`.

---

## License

Dual-licensed under Apache-2.0 and MIT. See the `LICENSE.txt` file for details.

---

## Acknowledgements

- [Verso](https://github.com/tauri-apps/verso) and [Servo](https://servo.org/) for the webview technology.
- [winit](https://github.com/rust-windowing/winit) for the cross-platform window and event loop abstractions.
