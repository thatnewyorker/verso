#![allow(unused_imports, dead_code)]

/*
simple_window – minimal example using the lightweight window_manager builder
and demonstrating a Tauri-like invoke round-trip via the adapter.

This example intentionally keeps the runtime integration minimal:
- It uses the window_manager crate to build a VersoWindow handle with common options.
- It wires a no-op backend that just logs actions (show/hide/close/eval_script/send_ipc_event).
- It demonstrates how to use the tauri_ipc_adapter to invoke a "hello" command locally.

Notes:
- This example does not spin up a winit event loop or attach to versoview directly to keep
  the code compile- and run-friendly in any environment.
- To integrate with the real versoview runtime, use the `verso-standalone` crate:
  - Create a WinitRuntime.
  - Create a native window and retrieve NativeSurfaceHandles.
  - Bind a VersoWebviewHost to that window.
  - Map the WindowOptions (devtools_port/init scripts) into ControllerConfig on bind.
*/

use serde_json::json;
use tauri_ipc_adapter::{ChannelAdapter, CommandRegistry, InvokeRequest};
use window_manager::{VersoWindowOptions, WindowBuilder};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("Verso simple_window example");
    println!("Building window description via window_manager...");

    // Prepare a simple backend that logs actions instead of creating an OS window.
    let mut backend = window_manager::Backend::default();
    backend.show = Some(Box::new(|| {
        println!("[backend] show()");
    }));
    backend.hide = Some(Box::new(|| {
        println!("[backend] hide()");
    }));
    backend.close = Some(Box::new(|| {
        println!("[backend] close()");
    }));
    backend.eval_script = Some(Box::new(|src: &str| {
        println!("[backend] eval_script(len={}): {}", src.len(), src);
        Ok(())
    }));
    backend.send_ipc_event = Some(Box::new(|name: &str, payload: &serde_json::Value| {
        println!("[backend] send_ipc_event: {} payload={}", name, payload);
        Ok(())
    }));

    // Build a logical window + webview descriptor with a few common options.
    let mut win = WindowBuilder::new()
        .title("Verso Simple Window")
        .size(800, 600)
        .min_size(400, 300)
        .decorations(true)
        .resizable(true)
        .always_on_top(false)
        .initialization_script("console.log('[verso] init script 1');")
        .initialization_script("console.log('[verso] init script 2');")
        .devtools_port(Some(0)) // 0 → request a random available port (when supported by runtime)
        .on_title_changed(|new_title| {
            println!("[hook] title changed -> {}", new_title);
        })
        .on_focus(|| {
            println!("[hook] focus gained");
        })
        .on_close(|| {
            println!("[hook] window closing");
        })
        .build_with_backend(backend);

    // Drive a few operations on the handle. These log via the example backend above.
    println!("Showing window and setting title...");
    win.show()?;
    win.set_title("Verso Simple Window (running)");

    // Simulate loading about:blank by evaluating a script (placeholder).
    // In a real app, you would call into the controller: host.load("about:blank").
    println!("Simulating about:blank load via eval_script...");
    win.eval_script("document.title = 'about:blank';")?;

    // Send a small IPC event (name + JSON payload) to illustrate the API surface.
    win.send_ipc_event("hello_event", &json!({"msg": "hello from host"}))?;

    // Demonstrate a Tauri-like invoke call via the lightweight adapter.
    // Here we register a "hello" handler that simply echoes back the payload.
    println!("Invoking a 'hello' command via the tauri-like adapter...");
    let mut registry = CommandRegistry::new();
    registry.register("hello", Box::new(|payload| Ok(payload)));

    let adapter = ChannelAdapter::new();
    let req = InvokeRequest {
        command: "hello".to_string(),
        payload: json!({"name": "world"}),
        id: 42,
    };
    let resp = adapter.invoke(&registry, req);
    match (resp.ok, resp.err) {
        (Some(value), _) => println!(
            "invoke ok: in_reply_to={}, payload={}",
            resp.in_reply_to, value
        ),
        (_, Some(err)) => println!(
            "invoke error: in_reply_to={}, error={}",
            resp.in_reply_to, err
        ),
        _ => println!(
            "invoke returned neither ok nor err (in_reply_to={})",
            resp.in_reply_to
        ),
    }

    // Close the window (logs via backend + triggers on_close hook).
    println!("Closing window...");
    win.close()?;

    println!("Done.");
    Ok(())
}
