//! Native tray icon and menu.
//!
//! The tray is deliberately thin: it only reports Open/Exit actions. The shell owns window
//! visibility and shutdown so the same code path works on every platform.

use tray_icon::menu::{Menu, MenuEvent, MenuId, MenuItem};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder};

/// An action requested from the tray menu.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayAction {
    /// Show and focus the main window.
    Open,
    /// Quit the application.
    Exit,
}

/// A live tray icon. Dropping it removes the icon from the tray.
pub struct Tray {
    icon: TrayIcon,
    open_id: MenuId,
    exit_id: MenuId,
    visible: bool,
}

impl Tray {
    /// Creates the tray icon and its two-item menu.
    ///
    /// Returns `None` when the platform tray service is unavailable; the application then
    /// keeps its normal visible-window behaviour.
    #[must_use]
    pub fn new(open_label: &str, exit_label: &str) -> Option<Self> {
        let menu = Menu::new();
        let open = MenuItem::new(open_label, true, None);
        let exit = MenuItem::new(exit_label, true, None);
        menu.append(&open).ok()?;
        menu.append(&tray_icon::menu::PredefinedMenuItem::separator())
            .ok()?;
        menu.append(&exit).ok()?;

        let icon = TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            .with_tooltip("LuaTools")
            .with_icon(load_icon()?)
            .build()
            .ok()?;

        Some(Self {
            icon,
            open_id: open.id().clone(),
            exit_id: exit.id().clone(),
            visible: true,
        })
    }

    /// Returns the next tray menu action, if one is waiting.
    #[must_use]
    pub fn poll(&self) -> Option<TrayAction> {
        while let Ok(event) = MenuEvent::receiver().try_recv() {
            if event.id == self.open_id {
                return Some(TrayAction::Open);
            }
            if event.id == self.exit_id {
                return Some(TrayAction::Exit);
            }
        }
        None
    }

    /// Shows or hides the tray icon.
    pub fn set_visible(&mut self, visible: bool) {
        if self.visible == visible {
            return;
        }
        if self.icon.set_visible(visible).is_ok() {
            self.visible = visible;
        }
    }

    /// Whether the tray icon currently claims to be visible.
    #[must_use]
    pub const fn is_visible(&self) -> bool {
        self.visible
    }
}

fn load_icon() -> Option<Icon> {
    if let Ok(image) = image::load_from_memory(include_bytes!("../../../assets/icon.ico")) {
        let image = image.into_rgba8();
        let (width, height) = image.dimensions();
        return Icon::from_rgba(image.into_raw(), width, height).ok();
    }
    fallback_icon()
}

fn fallback_icon() -> Option<Icon> {
    const SIZE: u32 = 32;
    let mut pixels = vec![0_u8; (SIZE * SIZE * 4) as usize];
    for y in 0..SIZE {
        for x in 0..SIZE {
            let index = ((y * SIZE + x) * 4) as usize;
            let inside = (4..28).contains(&x) && (4..28).contains(&y);
            let (r, g, b) = if inside {
                (167, 139, 250)
            } else {
                (32, 32, 32)
            };
            pixels[index] = r;
            pixels[index + 1] = g;
            pixels[index + 2] = b;
            pixels[index + 3] = 255;
        }
    }
    Icon::from_rgba(pixels, SIZE, SIZE).ok()
}
