#![allow(unused_imports, unused_variables, dead_code)]

/*
Skeleton example for integrating verso-standalone with a winit-driven app.

Status:
- This example demonstrates the intended usage flow and public API surface.
- The runtime now implements window creation and the event loop using `winit`'s
  ApplicationHandler model.
- IMPORTANT threading rule: window creation must NOT happen on the event-loop (UI) thread.
  Call `create_window` from another thread after `run` has initialized the event-loop proxy,
  or trigger creation in response to an event from a different thread. Calling `create_window`
  on the event-loop thread will deadlock because it waits for a response from the loop.

Typical flow:
1) Initialize configuration for Verso paths/resources if needed.
2) Create a WinitRuntime instance.
3) Start the event loop with `WinitRuntime::run`.
4) From another thread (after the event loop starts), call `create_window(...)`.
5) Build NativeSurfaceHandles from the created window and bind a VersoWebviewHost.
6) Attach the host to the runtime, then register a basic controller event subscriber
   that forwards input and draw events to the mock controller (until the real runtime is enabled).
*/

use verso_standalone::{
    NativeSurfaceHandles, VersoWebviewHost, WinitRuntime, WinitWindowHandle, logical_to_physical,
    map_winit_key, map_winit_mouse_button,
};

use winit::{
    dpi::{LogicalSize, PhysicalSize},
    event::MouseButton,
    keyboard::{Key, NamedKey},
};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("verso-standalone winit_hello skeleton");
    println!(
        "Note: Window creation is available. Create windows from a non-UI thread after `run` starts."
    );

    // Optionally configure Verso (uncomment and set as needed):
    // verso_standalone::set_verso_path("/absolute/path/to/versoview");
    // verso_standalone::set_verso_resource_directory("/absolute/path/to/resources");
    // verso_standalone::set_verso_devtools_port(0); // 0 for random available port

    // Step 1: Create the runtime.
    let runtime = WinitRuntime::<()>::new();

    // Step 2: Window creation (threading rule)
    // Do NOT call `create_window` on the event-loop thread. Instead, spawn a worker thread
    // after `run` has started, or block until the event-loop proxy is initialized, e.g.:
    //
    // let rt = /* clone or otherwise share `runtime` here (requires your own sharing strategy) */;
    // std::thread::spawn(move || {
    //     // Wait until the event-loop proxy is ready (post_user_event returns Ok)
    //     loop {
    //         if rt.post_user_event(()).is_ok() {
    //             break;
    //         }
    //         std::thread::sleep(std::time::Duration::from_millis(10));
    //     }
    //     let window_handle = rt
    //         .create_window("Verso Hello", Some(LogicalSize::new(800.0, 600.0)))
    //         .expect("failed to create window");
    //     // After this, bind the Verso webview to the native surface (see Step 3).
    // });
    //
    // For now, show how mapping helpers would be used:
    let _mapped_mouse = map_winit_mouse_button(MouseButton::Left);
    let dummy_key = Key::Named(NamedKey::Enter);
    let _mapped_key = map_winit_key(&dummy_key);

    // Step 3: Intended native surface binding (pseudo-code)
    // After creating a real winit::window::Window, you’d extract raw handles:
    //
    // use raw_window_handle::{HasDisplayHandle, HasWindowHandle};
    // let raw_window = window.window_handle().unwrap().as_raw();
    // let raw_display = window.display_handle().ok().map(|d| d.as_raw());
    //
    // let surface_handles = NativeSurfaceHandles {
    //     window: raw_window,
    //     display: raw_display,
    // };
    //
    // let mut webview = VersoWebviewHost::new(window_handle);
    // webview.bind_native_surface(surface_handles)?;
    // webview.load("https://example.com")?;
    //
    // Step 4: Attach the host to the runtime so subscribers can forward input and draw events:
    //
    // runtime.attach_host(window_handle, webview)?;
    //
    // Step 5: Register a basic controller event subscriber (mock-only wiring shown here).
    // This subscriber forwards WindowEvent/DeviceEvent to the controller via the host
    // and triggers draws on RedrawRequested. The type is available via the crate’s
    // re-exports once exposed; until then, use the module path where it is defined.
    //
    // use verso_standalone::ControllerEventSubscriber; // if re-exported by the crate
    // // or, if not re-exported yet, use the internal module path once made public:
    // // use verso_standalone::winit_runtime::ControllerEventSubscriber;
    // runtime.add_subscriber(window_handle, ControllerEventSubscriber::new(window_handle))?;
    //
    // Step 6: Run the event loop (blocks). Create windows from a non-UI thread after this starts.
    // runtime.run(|_user_event: ()| {
    //     // Handle custom user events posted to the event loop (if any)
    // })?;

    // Configure a startup window; the runtime will create it on resume.
    eprintln!("winit_hello: configuring startup window (800x600) with title 'Verso Hello'");
    runtime.set_startup_window(
        winit::window::Window::default_attributes()
            .with_title("Verso Hello".to_string())
            .with_inner_size(winit::dpi::LogicalSize::new(800.0, 600.0)),
    );

    // Start the winit event loop (blocks until all windows are closed).
    eprintln!(
        "winit_hello: entering runtime.run() (XDG_SESSION_TYPE={:?}, WAYLAND_DISPLAY={:?})",
        std::env::var("XDG_SESSION_TYPE").ok(),
        std::env::var("WAYLAND_DISPLAY").ok()
    );
    runtime.run(|_user_event: ()| {
        // Handle custom user events posted to the event loop (if any)
    })?;
    Ok(())
}
