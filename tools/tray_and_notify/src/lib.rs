/*!
Minimal, feature-gated system tray and notifications API for Verso.

Goals
- Provide a small, dependency-light public surface with opt-in platform backends.
- Keep defaults safe: no-ops when a backend is not enabled or not supported.
- Allow apps to wire callbacks (tray click, notification action) without coupling.

Features (Cargo.toml)
- `tray`: enables high-level tray API (subfeatures: `tray-linux`, `tray-macos`, `tray-windows`)
- `notifications`: enables high-level notification API (subfeatures: `notify-linux`, `notify-macos`, `notify-windows`)
- `serde`: derive Serialize/Deserialize on menu spec and payload types
- `tracing`: enable tracing logs

Current backends
- Linux notifications (notify-rust) when `notify-linux` is enabled.
- Tray backends are stubs for now (no-ops), ready to be filled per platform.

This crate intentionally avoids heavy dependencies by default. Platform crates are optional and only
compiled when the target OS and matching feature are enabled.
*/

#![forbid(unsafe_code)]
#![deny(missing_docs)]

use std::fmt;

#[cfg(feature = "tracing")]
use tracing::{debug, info, warn};

/// Unified error type for this crate.
#[derive(Debug)]
pub enum ApiError {
    /// Operation is not supported on the current platform or with the current features.
    Unsupported(&'static str),
    /// Backend-specific error with a string payload.
    Backend(String),
}

impl fmt::Display for ApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ApiError::Unsupported(msg) => write!(f, "unsupported: {msg}"),
            ApiError::Backend(msg) => write!(f, "backend error: {msg}"),
        }
    }
}

impl std::error::Error for ApiError {}

type Result<T> = std::result::Result<T, ApiError>;

/// A single menu item for a tray menu.
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MenuItem {
    /// Application-defined identifier for click routing.
    pub id: String,
    /// Visible title/label (ignored for separators).
    pub title: String,
    /// Whether the item is enabled.
    pub enabled: bool,
    /// Whether the item is checked (for checkbox items).
    pub checked: bool,
    /// Optional keyboard shortcut descriptor (stringified).
    pub shortcut: Option<String>,
    /// Whether this item represents a separator (title, enabled, checked ignored).
    pub separator: bool,
    /// Optional submenu items.
    pub submenu: Option<Vec<MenuItem>>,
}

impl Default for MenuItem {
    fn default() -> Self {
        Self {
            id: String::new(),
            title: String::new(),
            enabled: true,
            checked: false,
            shortcut: None,
            separator: false,
            submenu: None,
        }
    }
}

/// A tray menu specification consisting of a flat or nested list of items.
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct MenuSpec {
    /// Top-level items. Submenus are supported via `MenuItem::submenu`.
    pub items: Vec<MenuItem>,
}

/// Callback invoked when a tray menu item is clicked.
/// Receives the `MenuItem::id` associated with the clicked item.
pub type TrayClickCallback = Box<dyn Fn(&str) + Send + Sync + 'static>;

/// Minimal tray API. Current implementation is a stub with a cross-platform public surface.
///
/// Behavior:
/// - `new` stores the icon path for future use (no-op backend may log).
/// - `set_menu` records the desired menu (no-op backend may log).
/// - `on_click` registers a callback stored inside the handle (no-op backend never triggers it).
///
/// When platform backends are implemented, these methods will bridge to the OS-specific tray APIs.
/// Until then, calls succeed (Ok(())) so apps can compile and integrate incrementally.
pub struct Tray {
    icon_path: Option<String>,
    menu: Option<MenuSpec>,
    on_click: Option<TrayClickCallback>,
    #[allow(dead_code)]
    platform: PlatformTray,
}

impl Tray {
    /// Create a new tray icon handle. Pass `None` for no icon.
    pub fn new(icon_path: Option<String>) -> Result<Self> {
        #[cfg(feature = "tracing")]
        info!("Tray::new (icon: {:?})", icon_path);

        let platform = PlatformTray::create()?;
        Ok(Self {
            icon_path,
            menu: None,
            on_click: None,
            platform,
        })
    }

    /// Set or replace the tray menu.
    pub fn set_menu(&mut self, menu: MenuSpec) -> Result<()> {
        #[cfg(feature = "tracing")]
        debug!("Tray::set_menu (items: {})", menu.items.len());
        self.menu = Some(menu);
        self.platform.apply_menu(self.menu.as_ref())?;
        Ok(())
    }

    /// Register a click callback for menu items (by item id).
    pub fn on_click(&mut self, cb: TrayClickCallback) -> Result<()> {
        #[cfg(feature = "tracing")]
        debug!("Tray::on_click (callback installed)");
        // Store locally for completeness, but move into the platform-global callback when a tray backend is active.
        self.on_click = Some(cb);
        #[cfg(any(
            all(target_os = "linux", feature = "tray-linux"),
            all(target_os = "macos", feature = "tray-macos"),
            all(target_os = "windows", feature = "tray-windows")
        ))]
        {
            let cb_box = self.on_click.take();
            set_tray_click_callback(cb_box);
        }
        Ok(())
    }

    /// Close/destroy the tray (best-effort, no-op on stub).
    pub fn close(mut self) -> Result<()> {
        #[cfg(feature = "tracing")]
        info!("Tray::close");
        #[cfg(any(
            all(target_os = "linux", feature = "tray-linux"),
            all(target_os = "macos", feature = "tray-macos"),
            all(target_os = "windows", feature = "tray-windows")
        ))]
        {
            // Clear global callback to avoid calling into dropped closures.
            set_tray_click_callback(None);
        }
        self.platform.destroy()
    }

    /// Internal: for test harnesses to simulate a click (no-op in release).
    #[cfg(test)]
    fn simulate_click(&self, id: &str) {
        if let Some(cb) = &self.on_click {
            cb(id);
        }
    }
}

/// Platform tray abstraction.
/// For now, all implementations are stubs (no-ops) that keep API surface stable.
///
/// When a specific platform backend is implemented under `tray-*` features, its
/// `create/apply_menu/destroy` methods should hook into the OS tray APIs.
struct PlatformTray {
    #[cfg(any(
        all(target_os = "linux", feature = "tray-linux"),
        all(target_os = "macos", feature = "tray-macos"),
        all(target_os = "windows", feature = "tray-windows")
    ))]
    tray: Option<tray_icon::TrayIcon>,
    #[cfg(any(
        all(target_os = "linux", feature = "tray-linux"),
        all(target_os = "macos", feature = "tray-macos"),
        all(target_os = "windows", feature = "tray-windows")
    ))]
    menu: Option<tray_icon::menu::Menu>,
}

#[cfg(any(
    all(target_os = "linux", feature = "tray-linux"),
    all(target_os = "macos", feature = "tray-macos"),
    all(target_os = "windows", feature = "tray-windows")
))]
static TRAY_CLICK_CB: std::sync::OnceLock<
    std::sync::Mutex<Option<Box<dyn Fn(&str) + Send + Sync + 'static>>>,
> = std::sync::OnceLock::new();

#[cfg(any(
    all(target_os = "linux", feature = "tray-linux"),
    all(target_os = "macos", feature = "tray-macos"),
    all(target_os = "windows", feature = "tray-windows")
))]
static TRAY_ID_MAP: std::sync::OnceLock<
    std::sync::Mutex<std::collections::HashMap<tray_icon::menu::MenuId, String>>,
> = std::sync::OnceLock::new();

#[cfg(any(
    all(target_os = "linux", feature = "tray-linux"),
    all(target_os = "macos", feature = "tray-macos"),
    all(target_os = "windows", feature = "tray-windows")
))]
static TRAY_EVENT_THREAD: std::sync::OnceLock<()> = std::sync::OnceLock::new();

#[cfg(any(
    all(target_os = "linux", feature = "tray-linux"),
    all(target_os = "macos", feature = "tray-macos"),
    all(target_os = "windows", feature = "tray-windows")
))]
fn set_tray_click_callback(cb: Option<Box<dyn Fn(&str) + Send + Sync + 'static>>) {
    let m = TRAY_CLICK_CB.get_or_init(|| std::sync::Mutex::new(None));
    *m.lock().unwrap() = cb;
}

impl PlatformTray {
    fn create() -> Result<Self> {
        #[cfg(any(
            all(target_os = "linux", feature = "tray-linux"),
            all(target_os = "macos", feature = "tray-macos"),
            all(target_os = "windows", feature = "tray-windows")
        ))]
        {
            // Attempt to create a minimal tray icon with a tooltip. If creation fails,
            // fall back to a no-op backend (tray = None).
            let tray = tray_icon::TrayIconBuilder::new()
                .with_tooltip("Verso")
                .build()
                .ok();
            return Ok(Self { tray, menu: None });
        }

        #[cfg(feature = "tracing")]
        warn!("PlatformTray::create: using no-op backend");
        Ok(Self {
            #[cfg(any(
                all(target_os = "linux", feature = "tray-linux"),
                all(target_os = "macos", feature = "tray-macos"),
                all(target_os = "windows", feature = "tray-windows")
            ))]
            tray: None,
            #[cfg(any(
                all(target_os = "linux", feature = "tray-linux"),
                all(target_os = "macos", feature = "tray-macos"),
                all(target_os = "windows", feature = "tray-windows")
            ))]
            menu: None,
        })
    }

    fn apply_menu(&mut self, menu: Option<&MenuSpec>) -> Result<()> {
        #[cfg(any(
            all(target_os = "linux", feature = "tray-linux"),
            all(target_os = "macos", feature = "tray-macos"),
            all(target_os = "windows", feature = "tray-windows")
        ))]
        {
            use std::collections::HashMap;
            use tray_icon::menu::{
                Menu, MenuEvent, MenuId, MenuItem as TMenuItem, PredefinedMenuItem, Submenu,
            };

            fn build_menu_recursive(
                spec_items: &[super::MenuItem],
                menu: &Menu,
                id_map: &mut HashMap<MenuId, String>,
            ) {
                for it in spec_items {
                    if it.separator {
                        let sep = PredefinedMenuItem::separator();
                        let _ = menu.append(&sep);
                        continue;
                    }
                    if let Some(children) = &it.submenu {
                        let sub = Submenu::new(&it.title, true);
                        let sub_menu = sub.clone().into_menu();
                        build_menu_recursive(children, &sub_menu, id_map);
                        let _ = menu.append(&sub);
                        continue;
                    }
                    let item = TMenuItem::new(&it.title, it.enabled, None);
                    id_map.insert(item.id(), it.id.clone());
                    let _ = menu.append(&item);
                }
            }

            let spec = if let Some(s) = menu {
                s
            } else {
                // Clear mapping if no menu provided.
                let m = TRAY_ID_MAP.get_or_init(|| std::sync::Mutex::new(HashMap::new()));
                m.lock().unwrap().clear();
                return Ok(());
            };

            // Build a new menu and id map from the spec.
            let menu_obj = Menu::new();
            let mut map: HashMap<MenuId, String> = HashMap::new();
            build_menu_recursive(&spec.items, &menu_obj, &mut map);

            // Swap global id map used by the event handler.
            let m = TRAY_ID_MAP.get_or_init(|| std::sync::Mutex::new(HashMap::new()));
            *m.lock().unwrap() = map;

            // Recreate tray icon with the new menu to ensure platform state is updated.
            if self.tray.take().is_some() {
                // Dropping old tray instance.
            }
            self.menu = Some(menu_obj.clone());
            self.tray = tray_icon::TrayIconBuilder::new()
                .with_tooltip("Verso")
                .with_menu(menu_obj)
                .build()
                .ok();

            // Spawn the event loop thread once to dispatch click events.
            TRAY_EVENT_THREAD.get_or_init(|| {
                std::thread::spawn(move || {
                    let rx = MenuEvent::receiver();
                    while let Ok(ev) = rx.recv() {
                        match ev {
                            MenuEvent::MenuItemClick(id) => {
                                let name_opt = {
                                    let mm = TRAY_ID_MAP.get().unwrap().lock().unwrap();
                                    mm.get(&id).cloned()
                                };
                                if let Some(name) = name_opt {
                                    if let Some(mcb) = TRAY_CLICK_CB.get() {
                                        if let Some(cb) = mcb.lock().unwrap().as_ref() {
                                            cb(&name);
                                        }
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                });
            });

            #[cfg(feature = "tracing")]
            debug!("PlatformTray::apply_menu: applied tray menu");
            return Ok(());
        }

        // Other platforms or features: best-effort no-op
        #[cfg(feature = "tracing")]
        debug!("PlatformTray::apply_menu: no-op");
        Ok(())
    }

    fn destroy(&mut self) -> Result<()> {
        #[cfg(any(
            all(target_os = "linux", feature = "tray-linux"),
            all(target_os = "macos", feature = "tray-macos"),
            all(target_os = "windows", feature = "tray-windows")
        ))]
        {
            // Drop the tray icon to remove it from the status area.
            self.tray = None;
            self.menu = None;
            // Clear global mappings/callbacks best-effort.
            if let Some(mm) = TRAY_ID_MAP.get() {
                mm.lock().unwrap().clear();
            }
        }
        #[cfg(feature = "tracing")]
        debug!("PlatformTray::destroy: done");
        Ok(())
    }
}

/// Callback invoked on notification action (e.g., button press).
/// Receives an action identifier string when available.
pub type NotificationActionCallback = Box<dyn Fn(&str) + Send + Sync + 'static>;

/// Minimal notification builder/handle.
pub struct Notification {
    title: String,
    body: String,
    icon_path: Option<String>,
    on_action: Option<NotificationActionCallback>,
}

impl Notification {
    /// Create a new notification builder with title and body.
    pub fn new(title: impl Into<String>, body: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            body: body.into(),
            icon_path: None,
            on_action: None,
        }
    }

    /// Set an optional icon path.
    pub fn icon_path(mut self, path: impl Into<String>) -> Self {
        self.icon_path = Some(path.into());
        self
    }

    /// Register an action callback (delivered when supported by the platform backend).
    pub fn on_action(mut self, cb: NotificationActionCallback) -> Self {
        self.on_action = Some(cb);
        self
    }

    /// Show the notification now. Returns Ok(()) on success or if unsupported (no-op).
    pub fn show(&self) -> Result<()> {
        #[cfg(all(target_os = "linux", feature = "notify-linux"))]
        {
            return linux::show_linux(self);
        }

        // Other platforms or features: best-effort no-op
        #[cfg(feature = "tracing")]
        warn!(
            "Notification::show: no backend available (title: {:?})",
            self.title
        );
        Ok(())
    }

    /// Convenience function to show a simple notification in one call.
    pub fn show_simple(
        title: impl Into<String>,
        body: impl Into<String>,
        icon_path: Option<String>,
    ) -> Result<()> {
        let mut n = Notification::new(title, body);
        if let Some(p) = icon_path {
            n = n.icon_path(p);
        }
        n.show()
    }
}

#[cfg(all(target_os = "linux", feature = "notify-linux"))]
mod linux {
    use super::{ApiError, Notification, Result};
    #[cfg(feature = "tracing")]
    use tracing::debug;

    pub fn show_linux(n: &Notification) -> Result<()> {
        // Use notify-rust if available.
        let mut notif = notify_rust::Notification::new();
        notif.summary(&n.title).body(&n.body);
        if let Some(icon) = &n.icon_path {
            notif.icon(icon);
        }
        #[cfg(feature = "tracing")]
        debug!("notify-rust: showing notification {:?}", n.title);
        notif
            .show()
            .map_err(|e| ApiError::Backend(format!("notify-rust error: {e}")))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn menu_spec_roundtrip_default() {
        let spec = MenuSpec {
            items: vec![
                MenuItem {
                    id: "open".into(),
                    title: "Open".into(),
                    enabled: true,
                    ..Default::default()
                },
                MenuItem {
                    id: "sep1".into(),
                    separator: true,
                    ..Default::default()
                },
                MenuItem {
                    id: "quit".into(),
                    title: "Quit".into(),
                    enabled: true,
                    ..Default::default()
                },
            ],
        };
        assert_eq!(spec.items.len(), 3);
    }

    #[test]
    fn tray_stub_lifecycle() {
        let mut tray = Tray::new(None).expect("create tray");
        tray.set_menu(MenuSpec::default()).expect("set menu");
        tray.on_click(Box::new(|_id| {})).expect("on_click");
        tray.close().expect("close");
    }

    #[test]
    fn notification_builder_compiles() {
        let _ = Notification::new("Title", "Body").show();
        let _ = Notification::show_simple("A", "B", None);
    }
}
