/*!
This module is intentionally left empty.

Previously, this file implemented a Tauri `Runtime` wrapper over Verso/Tao.
In this standalone fork, all Tauri-specific integrations have been removed,
so the runtime layer no longer exists here.

If you need a framework-agnostic runtime abstraction in the future, you can
add a new implementation in this module that builds purely on top of `winit`
and `verso` without any Tauri dependencies.
*/
