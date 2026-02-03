#[cfg_attr(mobile, tauri::mobile_entry_point)]
mod app;
mod util;

use std::sync::atomic::{AtomicUsize, Ordering};
use tauri::{Manager, WebviewUrl, WebviewWindowBuilder};
use tauri_plugin_window_state::Builder as WindowStatePlugin;
use tauri_plugin_window_state::StateFlags;

#[cfg(target_os = "macos")]
use std::time::Duration;

static WINDOW_COUNTER: AtomicUsize = AtomicUsize::new(1);

const WINDOW_SHOW_DELAY: u64 = 50;

use app::{
    invoke::{
        clear_cache_and_restart, download_file, download_file_by_binary, send_notification,
        update_theme_mode,
    },
    setup::{set_global_shortcut, set_system_tray},
    window::set_window,
};
use util::get_pake_config;

/// Extract a valid URL from arguments that matches the configured domain
fn extract_url_arg(args: &[String], config_url: &str) -> Option<String> {
    let allowed_host = config_url
        .strip_prefix("https://")
        .or_else(|| config_url.strip_prefix("http://"))
        .and_then(|s| s.split('/').next())
        .unwrap_or("");

    args.iter()
        .skip(1)
        .find(|arg| {
            if arg.starts_with("https://") || arg.starts_with("http://") {
                if let Some(host) = arg
                    .strip_prefix("https://")
                    .or_else(|| arg.strip_prefix("http://"))
                    .and_then(|s| s.split('/').next())
                {
                    return host == allowed_host;
                }
            }
            false
        })
        .cloned()
}

pub fn run_app() {
    #[cfg(target_os = "linux")]
    {
        if std::env::var("WEBKIT_DISABLE_DMABUF_RENDERER").is_err() {
            std::env::set_var("WEBKIT_DISABLE_DMABUF_RENDERER", "1");
        }
    }

    let (pake_config, tauri_config) = get_pake_config();
    let tauri_app = tauri::Builder::default();

    let show_system_tray = pake_config.show_system_tray();
    let hide_on_close = pake_config.windows[0].hide_on_close;
    let activation_shortcut = pake_config.windows[0].activation_shortcut.clone();
    let init_fullscreen = pake_config.windows[0].fullscreen;
    let start_to_tray = pake_config.windows[0].start_to_tray && show_system_tray; // Only valid when tray is enabled
    let multi_instance = pake_config.multi_instance;

    let window_state_plugin = WindowStatePlugin::default()
        .with_state_flags(if init_fullscreen {
            StateFlags::FULLSCREEN
        } else {
            // Prevent flickering on the first open.
            StateFlags::all() & !StateFlags::VISIBLE
        })
        .build();

    #[allow(deprecated)]
    let mut app_builder = tauri_app
        .plugin(window_state_plugin)
        .plugin(tauri_plugin_oauth::init())
        .plugin(tauri_plugin_http::init())
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_opener::init()); // Add this

    // Only add single instance plugin if multiple instances are not allowed
    if !multi_instance {
        let config_url_for_callback = pake_config.windows[0].url.clone();
        let window_width = pake_config.windows[0].width;
        let window_height = pake_config.windows[0].height;
        app_builder = app_builder.plugin(tauri_plugin_single_instance::init(move |app, args, _cwd| {
            // If URL argument provided, open in a new window
            if let Some(url) = extract_url_arg(&args, &config_url_for_callback) {
                let window_id = WINDOW_COUNTER.fetch_add(1, Ordering::SeqCst);
                let window_label = format!("pake-{}", window_id);
                if let Ok(new_window) = WebviewWindowBuilder::new(
                    app,
                    &window_label,
                    WebviewUrl::External(url.parse().unwrap()),
                )
                .title("")
                .inner_size(window_width, window_height)
                .build()
                {
                    let _ = new_window.show();
                    let _ = new_window.set_focus();
                }
            } else if let Some(window) = app.get_webview_window("pake") {
                // No URL, just show/focus existing window
                let _ = window.unminimize();
                let _ = window.show();
                let _ = window.set_focus();
            }
        }));
    }

    app_builder
        .invoke_handler(tauri::generate_handler![
            download_file,
            download_file_by_binary,
            send_notification,
            update_theme_mode,
            clear_cache_and_restart,
        ])
        .setup(move |app| {
            // --- Menu Construction Start ---
            #[cfg(target_os = "macos")]
            {
                let menu = app::menu::get_menu(app.app_handle())?;
                app.set_menu(menu)?;

                // Event Handling for Custom Menu Item
                app.on_menu_event(move |app_handle, event| {
                    app::menu::handle_menu_click(app_handle, event.id().as_ref());
                });
            }
            // --- Menu Construction End ---

            let window = set_window(app, &pake_config, &tauri_config);

            // Handle URL argument on initial launch
            let launch_args: Vec<String> = std::env::args().collect();
            let config_url_for_launch = pake_config.windows[0].url.clone();
            if let Some(url) = extract_url_arg(&launch_args, &config_url_for_launch) {
                let window_clone = window.clone();
                tauri::async_runtime::spawn(async move {
                    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
                    let script = format!("window.location.href = '{}'", url.replace('\'', "\\'"));
                    let _ = window_clone.eval(&script);
                });
            }

            set_system_tray(
                app.app_handle(),
                show_system_tray,
                &pake_config.system_tray_path,
                init_fullscreen,
            )
            .unwrap();
            set_global_shortcut(app.app_handle(), activation_shortcut, init_fullscreen).unwrap();

            // Show window after state restoration to prevent position flashing
            // Unless start_to_tray is enabled, then keep it hidden
            if !start_to_tray {
                let window_clone = window.clone();
                tauri::async_runtime::spawn(async move {
                    tokio::time::sleep(tokio::time::Duration::from_millis(WINDOW_SHOW_DELAY)).await;
                    window_clone.show().unwrap();

                    // Fixed: Linux fullscreen issue with virtual keyboard
                    #[cfg(target_os = "linux")]
                    {
                        if init_fullscreen {
                            window_clone.set_fullscreen(true).unwrap();
                            // Ensure webview maintains focus for input after fullscreen
                            let _ = window_clone.set_focus();
                        } else {
                            // Fix: Ubuntu 24.04/GNOME window buttons non-functional until resize (#1122)
                            // The window manager needs time to process the MapWindow event before
                            // accepting focus requests. Without this, decorations remain non-interactive.
                            tokio::time::sleep(tokio::time::Duration::from_millis(30)).await;
                            let _ = window_clone.set_focus();
                        }
                    }
                });
            }

            Ok(())
        })
        .on_window_event(move |_window, _event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = _event {
                if hide_on_close {
                    // Hide window when hide_on_close is enabled (regardless of tray status)
                    let window = _window.clone();
                    tauri::async_runtime::spawn(async move {
                        #[cfg(target_os = "macos")]
                        {
                            if window.is_fullscreen().unwrap_or(false) {
                                window.set_fullscreen(false).unwrap();
                                tokio::time::sleep(Duration::from_millis(900)).await;
                            }
                        }
                        #[cfg(target_os = "linux")]
                        {
                            if window.is_fullscreen().unwrap_or(false) {
                                window.set_fullscreen(false).unwrap();
                                // Restore focus after exiting fullscreen to fix input issues
                                let _ = window.set_focus();
                            }
                        }
                        // On macOS, directly hide without minimize to avoid duplicate Dock icons
                        #[cfg(not(target_os = "macos"))]
                        window.minimize().unwrap();
                        window.hide().unwrap();
                    });
                    api.prevent_close();
                } else {
                    // Exit app completely when hide_on_close is false
                    std::process::exit(0);
                }
            }
        })
        .build(tauri::generate_context!())
        .expect("error while building tauri application")
        .run(|_app, _event| {
            // Handle macOS dock icon click to reopen hidden window
            #[cfg(target_os = "macos")]
            if let tauri::RunEvent::Reopen {
                has_visible_windows,
                ..
            } = _event
            {
                if !has_visible_windows {
                    if let Some(window) = _app.get_webview_window("pake") {
                        let _ = window.show();
                        let _ = window.set_focus();
                    }
                }
            }
        });
}

pub fn run() {
    run_app()
}
