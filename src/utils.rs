/*!
Utility placeholders.

This module previously contained conversions and helpers that depended on Tauri
types. Since this crate is now framework-agnostic and no longer integrates with
Tauri directly, those utilities have been removed.

Add framework-agnostic utilities here as the standalone API evolves.
*/

#[allow(dead_code)]
/// No-op placeholder to keep this module active until new utilities are added.
pub fn placeholder() {}
