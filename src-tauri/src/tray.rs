//! Tray icon + desktop notifications.
//!
//! Closing the window hides to the tray instead of exiting: transfers are
//! process-local, so quitting on the close button used to kill a running
//! upload/download. Quitting now only happens through the tray menu (or
//! SIGTERM), and `state::EXITING` tells the window handler to let it through.
//!
//! The notification side is deliberately thin: [`notify`] is called for the
//! handful of events worth interrupting the user for (a transfer finished or
//! failed, a share link arrived) and respects the `notifications` setting.

use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::image::Image;
use tauri::{AppHandle, Manager};
use tauri_plugin_notification::NotificationExt;

use crate::state::AppState;

/// Icon embedded at compile time so the tray also works from a `cargo run`
/// build that has no `icons/` directory next to the binary.
const TRAY_ICON: &[u8] = include_bytes!("../icons/32x32.png");

pub fn install(app: &AppHandle) -> tauri::Result<()> {
    let open = MenuItem::with_id(app, "open", "打开云语聚合网盘", true, None::<&str>)?;
    let transfers = MenuItem::with_id(app, "transfers", "传输列表", true, None::<&str>)?;
    let sep = PredefinedMenuItem::separator(app)?;
    let quit = MenuItem::with_id(app, "quit", "退出", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&open, &transfers, &sep, &quit])?;

    let mut builder = TrayIconBuilder::with_id("main")
        .tooltip("云语聚合网盘")
        .menu(&menu)
        // Left click opens the window; the menu lives on right click.
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| match event.id().as_ref() {
            "open" | "transfers" => show_window(app, event.id().as_ref() == "transfers"),
            "quit" => {
                crate::state::request_exit();
                app.exit(0);
            }
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                show_window(tray.app_handle(), false);
            }
        });

    if let Ok(icon) = Image::from_bytes(TRAY_ICON) {
        builder = builder.icon(icon);
    }
    builder.build(app)?;
    Ok(())
}

/// Shows and focuses the main window, optionally on the transfers screen.
fn show_window(app: &AppHandle, transfers: bool) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.show();
        let _ = window.unminimize();
        let _ = window.set_focus();
    }
    if transfers {
        let _ = app.emit_to("main", crate::EVENT_NAVIGATE, "transfers");
    }
}

/// Fires a desktop notification when the user asked for them.
pub fn notify(app: &AppHandle, title: &str, body: &str) {
    // Called from tokio tasks as well as the main thread: never touch the
    // async settings lock here (that is what used to crash the app).
    let enabled = match app.try_state::<std::sync::Arc<AppState>>() {
        Some(state) => state.notifications_enabled(),
        None => return,
    };
    if !enabled {
        return;
    }
    let _ = app
        .notification()
        .builder()
        .title(title)
        .body(body)
        .show();
}

use tauri::Emitter;
