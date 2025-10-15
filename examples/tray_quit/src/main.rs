#![allow(clippy::needless_return)]

/*
A minimal system tray example with a single "Quit" menu item.

Notes:
- This example uses the `tray_and_notify` crate's high-level API.
- On platforms or builds without a tray backend enabled, calls succeed as no-ops.
- A small stdin thread lets you press Enter to exit even if tray clicks aren't wired.

To try Linux tray backend:
- Enable the `tray` and `tray-linux` features for `tray_and_notify` in your workspace/app.
*/

use std::io::{self, Read};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::thread;
use std::time::Duration;

use tray_and_notify::{MenuItem, MenuSpec, Tray};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("Starting tray_quit example...");
    println!("If the tray backend is available, a tray icon should appear with a Quit item.");
    println!("Tip: Press Enter in this terminal to quit at any time.");

    // Track quit signal from tray click or stdin.
    let quitting = Arc::new(AtomicBool::new(false));

    // Build tray and menu.
    let mut tray = Tray::new(None)?;
    let menu = MenuSpec {
        items: vec![MenuItem {
            id: "quit".into(),
            title: "Quit".into(),
            enabled: true,
            ..Default::default()
        }],
    };
    tray.set_menu(menu)?;

    // Hook menu clicks.
    {
        let quitting = quitting.clone();
        tray.on_click(Box::new(move |id| {
            if id == "quit" {
                println!("[tray] Quit clicked.");
                quitting.store(true, Ordering::SeqCst);
            } else {
                println!("[tray] Click: {}", id);
            }
        }))?;
    }

    // Fallback: allow pressing Enter to exit even if the tray backend is a no-op.
    let stdin_quit = quitting.clone();
    thread::spawn(move || {
        // Block until some input is available (Enter).
        let _ = io::stdin().read(&mut [0u8; 1]);
        stdin_quit.store(true, Ordering::SeqCst);
    });

    // Idle loop until we’re asked to quit.
    while !quitting.load(Ordering::SeqCst) {
        thread::sleep(Duration::from_millis(200));
    }

    // Best-effort teardown.
    tray.close()?;
    println!("Exiting tray_quit example.");
    Ok(())
}
