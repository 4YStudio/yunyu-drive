mod bridge;
mod commands;
mod config;
mod deeplink;
mod error;
mod logging;
mod login;
mod paths;
mod providers;
mod share;
#[cfg(test)]
mod share_live;
mod state;
mod theme;
mod transfer;
mod tray;

use std::sync::Arc;

use tauri::{Emitter, Manager, WindowEvent};

use crate::config::AppPaths;
use crate::state::AppState;

/// The desktop's colour scheme, used by the "follow system" theme mode.
/// WebKitGTK often fails to expose `prefers-color-scheme` on Wayland/KDE.
#[tauri::command]
fn system_theme() -> &'static str {
    theme::detect()
}

/// Event emitted when a deep-linked share arrives while the app is running.
pub const EVENT_DEEPLINK: &str = "deeplink://open";
/// Asks the UI to switch screens (tray "传输列表").
pub const EVENT_NAVIGATE: &str = "app://navigate";

/// Returns (and clears) a link that arrived on the command line before the
/// frontend was ready to listen for events.
#[tauri::command]
async fn deeplink_take(
    state: tauri::State<'_, Arc<AppState>>,
) -> Result<Option<String>, crate::error::AppError> {
    Ok(state.pending_link.write().await.take())
}

/// First share link among `args`, normalised to `yunyu-drive://share?url=…`.
///
/// A launch can come from our own desktop entry, from the web client's
/// `qklink://` handler, or from a plain `pan.quark.cn/s/…` URL, so every
/// spelling is folded into one before it reaches the UI.
fn first_link(args: &[String]) -> Option<String> {
    args.iter().find_map(|arg| deeplink::normalize(arg))
}



pub fn run() {
    tauri::Builder::default()
        // Must be registered first: a second launch (e.g. from a share link)
        // forwards its arguments here and exits.
        .plugin(tauri_plugin_single_instance::init(|app, argv, _cwd| {
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.show();
                let _ = window.unminimize();
                let _ = window.set_focus();
            }
            if let Some(link) = first_link(&argv) {
                let state = app.state::<Arc<AppState>>().inner().clone();
                let handle = app.clone();
                tauri::async_runtime::spawn(async move {
                    *state.pending_link.write().await = Some(link.clone());
                    tray::notify(&handle, "收到分享链接", "已打开「分享链接」页面");
                    let _ = handle.emit(EVENT_DEEPLINK, link);
                });
            }
        }))
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_notification::init())
        .setup(|app| {
            let paths = AppPaths::resolve()?;
            // File logging: several provider flows fail in ways only the wire
            // trace explains, and a GUI app has no stderr to read.
            logging::init(&paths.log_dir);
            ylog!("app start, version {}", env!("CARGO_PKG_VERSION"));
            // The cloud139 crate writes its config to `./cloud139rc.toml` by
            // default (it caches the resolved personal-cloud host after the
            // first listing). Under `tauri dev` that file lives inside
            // `src-tauri/`, so the watcher saw it appear, rebuilt the app, the
            // new process wrote it again — an endless restart loop. Point the
            // crate at our own config dir instead: one file, outside the tree
            // the dev server watches.
            cloud139::config::set_config_override(paths.config_dir.join("yun139.toml"));
            // A link on our own command line (first launch) — handed to the
            // state at construction so nothing tries to block-lock it later.
            let args: Vec<String> = std::env::args().collect();
            let state = AppState::with_pending_link(paths, first_link(&args))?;

            // Tray icon: closing the window hides it there instead of quitting,
            // so a running transfer survives a stray click on the X.
            tray::install(app.handle())?;

            // The share page on pan.quark.cn looks for this loopback service to
            // launch us; without it 「打开客户端」 only offers a download.
            {
                let handle = app.handle().clone();
                let on_link = std::sync::Arc::new(move |link: String| {
                    let handle = handle.clone();
                    tauri::async_runtime::spawn(async move {
                        if let Some(window) = handle.get_webview_window("main") {
                            let _ = window.show();
                            let _ = window.unminimize();
                            let _ = window.set_focus();
                        }
                        let state = handle.state::<Arc<AppState>>().inner().clone();
                        *state.pending_link.write().await = Some(link.clone());
                        tray::notify(&handle, "收到分享链接", "已打开「分享链接」页面");
                        let _ = handle.emit(EVENT_DEEPLINK, link);
                    });
                });
                bridge::spawn(app.handle().clone(), on_link);
            }

            // Bring back the last session's tasks (unfinished ones as paused)
            // and keep the file refreshed while transfers run.
            let transfers = Arc::clone(&state.transfers);
            let handle = app.handle().clone();
            transfers.start_persister();
            app.manage(state);
            tauri::async_runtime::spawn(async move {
                transfers.restore(&handle).await;
            });

            // Dev aid: `QUARK_DEV_LOGIN=1` opens the web login window on boot so
            // the flow can be exercised without clicking through the UI first.
            if std::env::var("QUARK_DEV_LOGIN").is_ok() {
                let handle = app.handle().clone();
                tauri::async_runtime::spawn(async move {
                    tokio::time::sleep(std::time::Duration::from_millis(900)).await;
                    let state = handle.state::<Arc<AppState>>();
                    if let Err(err) = login::open_web_login(handle.clone(), state).await {
                        eprintln!("dev login failed: {err}");
                    }
                });
            }
            Ok(())
        })
        .on_window_event(|window, event| {
            if let WindowEvent::CloseRequested { api, .. } = event {
                // Only the main window may be hidden to the tray. Login windows
                // must actually be destroyed: the web-login poll loop treats
                // "window gone" as cancelled, and a merely hidden window keeps
                // the app waiting forever. Re-showing that hidden webview later
                // is also what looked like "内置浏览器连不上网络".
                if window.label() != "main" {
                    return;
                }
                if crate::state::is_exiting() {
                    return;
                }
                let keep_running = window
                    .app_handle()
                    .try_state::<Arc<AppState>>()
                    .map(|state| state.close_to_tray())
                    .unwrap_or(false);
                if keep_running {
                    // Transfers are process-local: hiding keeps them alive.
                    api.prevent_close();
                    let _ = window.hide();
                }
            }
        })
        .invoke_handler(tauri::generate_handler![
            commands::auth_states,
            commands::provider_profiles,
            commands::auth_status,
            commands::set_cookie,
            commands::clear_cookie,
            commands::validate_cookie,
            commands::list_dir,
            commands::resolve_path,
            commands::search,
            commands::make_dir,
            commands::rename_entry,
            commands::delete_entries,
            commands::transfer_entries,
            commands::download_entries,
            commands::upload_files,
            commands::transfers_list,
            commands::transfer_cancel,
            commands::transfer_pause,
            commands::transfer_remove,
            commands::transfer_clear,
            commands::transfer_retry,
            commands::aliyun_qr_start,
            commands::aliyun_qr_poll_with,
            commands::provider_login,
            commands::provider_logout,
            commands::provider_quota,
            commands::recycle_list,
            commands::recycle_restore,
            commands::recycle_purge,
            commands::recycle_clear,
            commands::settings_get,
            commands::settings_set,
            commands::app_info,
            commands::quit_app,
            system_theme,
            deeplink_take,
            share::account_info,
            share::share_open,
            share::share_list,
            share::share_save,
            share::share_download,
            login::open_web_login,
            login::open_web_login_139,
            login::open_web_login_baidu,
            login::close_web_login,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

#[allow(dead_code)]
fn assert_send_sync() {
    fn check<T: Send + Sync>() {}
    check::<Arc<AppState>>();
}
