/*!
Glue utilities to map `window_manager` builder options into Verso controller config
and (optionally) winit window attributes.

This module is intentionally tiny and feature-gated:
- When the `window_manager` feature is enabled on the `verso-standalone` crate,
  the helpers in this file are compiled and available to consumers.
- By keeping this glue here, we avoid a hard dependency from `window_manager`
  back onto `verso-standalone`, and vice versa, maintaining clean layering.

What this provides:
- `to_controller_config_from_options` – map `VersoWindowOptions` into a
  `ControllerConfig` (handshake config for `versoview`).
- `controller_config_with_base` – merge mapping over an existing `ControllerConfig`.
- `apply_window_options_to_attributes` – apply window options to `winit::window::WindowAttributes`.

Notes:
- Only a subset of window options map to controller config (init scripts, devtools port).
  Window geometry and chrome are purely native window concerns and are applied to
  `WindowAttributes`, not `ControllerConfig`.
*/

#![forbid(unsafe_code)]
#![deny(missing_docs)]

#[cfg(feature = "window_manager")]
use window_manager::VersoWindowOptions;

use crate::controller::ControllerConfig;

#[cfg(feature = "window_manager")]
use winit::window::WindowAttributes;

/// Convert `VersoWindowOptions` into a `ControllerConfig`.
///
/// Mapping:
/// - `initialization_scripts` → `ControllerConfig.init_scripts`
/// - `devtools_port` → `ControllerConfig.devtools_port`
///
/// All other `ControllerConfig` fields are left at their defaults; the caller can
/// adjust (e.g., `mode`, `resources_dir`, `user_agent`, etc.) after this mapping.
///
/// This function is only available when the `window_manager` feature is enabled.
#[cfg(feature = "window_manager")]
pub fn to_controller_config_from_options(opts: &VersoWindowOptions) -> ControllerConfig {
    let mut cfg = ControllerConfig::default();
    cfg.devtools_port = opts.devtools_port;
    cfg.init_scripts = opts.initialization_scripts.clone();
    cfg
}

/// Merge `VersoWindowOptions` into an existing `ControllerConfig`.
///
/// Semantics:
/// - If `opts.devtools_port` is `Some(_)`, it overwrites `base.devtools_port`.
/// - If `opts.initialization_scripts` is non-empty, append them to `base.init_scripts`.
/// - All other fields of `base` are preserved.
///
/// This is useful when you already have paths and controller mode configured and you
/// want to enrich them from a window builder.
///
/// This function is only available when the `window_manager` feature is enabled.
#[cfg(feature = "window_manager")]
pub fn controller_config_with_base(
    mut base: ControllerConfig,
    opts: &VersoWindowOptions,
) -> ControllerConfig {
    if let Some(port) = opts.devtools_port {
        base.devtools_port = Some(port);
    }
    if !opts.initialization_scripts.is_empty() {
        base.init_scripts
            .extend(opts.initialization_scripts.clone());
    }
    base
}

/// Apply `VersoWindowOptions` to a `winit::window::WindowAttributes` builder.
///
/// Mapping:
/// - `title` → `with_title`
/// - `size` → `with_inner_size`
/// - `min_size` → `with_min_inner_size`
/// - `max_size` → `with_max_inner_size`
/// - `decorations` → `with_decorations`
/// - `always_on_top` → `with_always_on_top`
/// - `resizable` → `with_resizable`
///
/// Returns the updated `WindowAttributes`. This is a pure transformation function
/// that does not create windows by itself.
///
/// This function is only available when the `window_manager` feature is enabled.
#[cfg(feature = "window_manager")]
pub fn apply_window_options_to_attributes(
    mut attrs: WindowAttributes,
    opts: &VersoWindowOptions,
) -> WindowAttributes {
    use winit::dpi::LogicalSize;

    // Title
    attrs = attrs.with_title(opts.title.clone());

    // Geometry and constraints (convert u32 to f64 logical sizes)
    if let Some(sz) = opts.size {
        attrs = attrs.with_inner_size(LogicalSize::new(sz.width as f64, sz.height as f64));
    }
    if let Some(min) = opts.min_size {
        attrs = attrs.with_min_inner_size(LogicalSize::new(min.width as f64, min.height as f64));
    }
    if let Some(max) = opts.max_size {
        attrs = attrs.with_max_inner_size(LogicalSize::new(max.width as f64, max.height as f64));
    }

    // Chrome and behaviors
    attrs = attrs
        .with_decorations(opts.decorations)
        .with_always_on_top(opts.always_on_top)
        .with_resizable(opts.resizable);

    attrs
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(feature = "window_manager")]
    #[test]
    fn map_to_controller_config_sets_expected_fields() {
        let opts = window_manager::WindowBuilder::new()
            .title("Test")
            .size(800, 600)
            .initialization_script("console.log('a');")
            .initialization_script("console.log('b');")
            .devtools_port(Some(0))
            .build()
            .options()
            .clone();

        let cfg = to_controller_config_from_options(&opts);
        assert_eq!(cfg.devtools_port, Some(0));
        assert_eq!(cfg.init_scripts.len(), 2);
    }

    #[cfg(feature = "window_manager")]
    #[test]
    fn merge_over_base_preserves_and_overrides_correctly() {
        let opts = window_manager::WindowBuilder::new()
            .initialization_script("s1")
            .initialization_script("s2")
            .devtools_port(Some(9222))
            .build()
            .options()
            .clone();

        let mut base = ControllerConfig::default();
        base.devtools_port = Some(0);
        base.init_scripts = vec!["base".into()];

        let merged = controller_config_with_base(base, &opts);
        assert_eq!(merged.devtools_port, Some(9222));
        assert_eq!(merged.init_scripts, vec!["base", "s1", "s2"]);
    }

    // This test only ensures the function compiles and applies a couple of flags.
    #[cfg(feature = "window_manager")]
    #[test]
    fn apply_window_options_to_attrs_sets_flags() {
        let opts = window_manager::WindowBuilder::new()
            .title("X")
            .decorations(false)
            .resizable(false)
            .always_on_top(true)
            .build()
            .options()
            .clone();

        let attrs = winit::window::Window::default_attributes();
        let _ = apply_window_options_to_attributes(attrs, &opts);
        // We cannot easily introspect WindowAttributes fields here; compile-time coverage is fine.
    }
}
