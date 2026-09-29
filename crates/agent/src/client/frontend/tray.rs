//! System tray icon and menu.

use muda::{Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem};
use tray_icon::{MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};

use crate::client::frontend::icon::tray_icon_image;

/// Action produced by polling tray/menu events.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    OpenDashboard,
    Quit,
}

/// Handles needed to keep the tray alive and to match menu item IDs.
pub struct Handles {
    /// Kept alive for as long as the tray icon should be shown; dropping it
    /// removes the icon.
    pub tray: Option<TrayIcon>,
    pub open_item_id: MenuId,
    pub quit_item_id: MenuId,
}

/// Build the tray icon and its right-click menu.
pub fn build(tooltip: &str) -> Handles {
    let open_item = MenuItem::new("Open Dashboard", true, None);
    let quit_item = MenuItem::new("Quit AuditReady", true, None);
    let open_item_id = open_item.id().clone();
    let quit_item_id = quit_item.id().clone();

    let menu = Menu::new();
    let _ = menu.append(&open_item);
    let _ = menu.append(&PredefinedMenuItem::separator());
    let _ = menu.append(&quit_item);

    let tray = TrayIconBuilder::new()
        .with_tooltip(tooltip)
        .with_icon(tray_icon_image())
        .with_menu(Box::new(menu))
        // Left-click opens the dashboard directly (handled as a TrayIconEvent
        // below) instead of showing the menu; right-click still shows the menu.
        // Linux ignores this and always shows the menu on click, but Linux
        // never reaches this module.
        .with_menu_on_left_click(false)
        .build()
        .map_err(|e| tracing::warn!("failed to create tray icon: {}", e))
        .ok();

    Handles {
        tray,
        open_item_id,
        quit_item_id,
    }
}

/// Update the tooltip shown when hovering the tray icon.
pub fn set_tooltip(tray: &TrayIcon, tooltip: &str) {
    let _ = tray.set_tooltip(Some(tooltip));
}

/// Non-blocking poll of menu and tray click events.
pub fn poll(open_item_id: &MenuId, quit_item_id: &MenuId) -> Option<Action> {
    if let Ok(event) = MenuEvent::receiver().try_recv() {
        if event.id == *open_item_id {
            return Some(Action::OpenDashboard);
        } else if event.id == *quit_item_id {
            return Some(Action::Quit);
        }
    }

    if let Ok(TrayIconEvent::Click {
        button: MouseButton::Left,
        button_state: MouseButtonState::Up,
        ..
    }) = TrayIconEvent::receiver().try_recv()
    {
        return Some(Action::OpenDashboard);
    }

    None
}
