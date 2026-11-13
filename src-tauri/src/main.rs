//! SuiCap app shell (plan §3): tray, global hotkeys, commands and windows. All capture
//! work lives in `suicap-core`; this crate only drives it.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod commands;
mod feedback;
mod power;
mod state;
mod tray;

use std::str::FromStr;

use suicap_core::config::{self, Settings};
use tauri::{AppHandle, Manager, RunEvent};
use tauri_plugin_autostart::MacosLauncher;
use tauri_plugin_global_shortcut::{GlobalShortcutExt, Shortcut, ShortcutState};

use crate::state::AppState;

/// The save length bound to `shortcut`, if any (`Some(None)` = whole buffer).
fn hotkey_secs(app: &AppHandle, shortcut: &Shortcut) -> Option<Option<u32>> {
    let settings = app.state::<AppState>().settings();
    settings
        .hotkeys
        .bindings()
        .into_iter()
        .find(|(_, key)| !key.is_empty() && Shortcut::from_str(key).is_ok_and(|s| s == *shortcut))
        .map(|(secs, _)| secs)
}

/// (Re)register the save shortcuts; failures (a key taken by another app) are shown
/// in the settings window.
pub fn register_hotkeys(app: &AppHandle) {
    let gs = app.global_shortcut();
    let _ = gs.unregister_all();
    let settings = app.state::<AppState>().settings();
    let mut errors = Vec::new();
    for (_, key) in settings.hotkeys.bindings() {
        if key.is_empty() {
            continue;
        }
        match Shortcut::from_str(key) {
            Ok(s) => {
                if let Err(e) = gs.register(s) {
                    errors.push(format!("{key}: {e}"));
                }
            }
            Err(e) => errors.push(format!("{key}: {e}")),
        }
    }
    app.state::<AppState>()
        .update_status(app, |st| st.hotkey_errors = errors);
}

fn main() {
    let autostarted = std::env::args().any(|a| a == "--autostart");
    let app = tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            tray::open_settings(app)
        }))
        .plugin(
            tauri_plugin_global_shortcut::Builder::new()
                .with_handler(|app, shortcut, event| {
                    if event.state == ShortcutState::Pressed
                        && let Some(secs) = hotkey_secs(app, shortcut)
                    {
                        app.state::<AppState>().save(app, secs);
                    }
                })
                .build(),
        )
        .plugin(tauri_plugin_autostart::init(
            MacosLauncher::LaunchAgent,
            Some(vec!["--autostart"]),
        ))
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_dialog::init())
        .invoke_handler(tauri::generate_handler![
            commands::status,
            commands::arm,
            commands::disarm,
            commands::save_clip,
            commands::config_get,
            commands::config_set,
            commands::default_clips_dir,
            commands::displays,
            commands::estimate_mb,
            commands::open_clips_folder,
            commands::reveal_last_clip,
            commands::pick_clips_dir,
            commands::open_settings,
            commands::menu_ready,
            commands::quit,
        ])
        .setup(move |app| {
            let handle = app.handle().clone();
            let path = config::settings_path()
                .map_or_else(|| app.path().app_config_dir().map(|d| d.join("settings.json")), Ok)?;
            let (settings, warning) = Settings::load(&path);
            let clips = app
                .path()
                .video_dir()
                .or_else(|_| app.path().home_dir())?
                .join("SuiCap");
            let (autostart, arm) = (settings.autostart, settings.arm_on_start);
            app.manage(AppState::new(settings, warning, path, clips));
            tray::create(&handle)?;
            power::watch(&handle);
            register_hotkeys(&handle);
            commands::apply_autostart(&handle, autostart);
            if arm {
                let h = handle.clone();
                std::thread::spawn(move || h.state::<AppState>().arm(&h));
            }
            if !autostarted {
                tray::open_settings(&handle);
            }
            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("failed to build the SuiCap app");

    app.run(|app, event| match event {
        // Closing the last window keeps the app in the tray.
        RunEvent::ExitRequested { code: None, api, .. } => api.prevent_exit(),
        // Quit: stop capture so saves in progress finish their files.
        RunEvent::Exit => app.state::<AppState>().disarm(app),
        _ => {}
    });
}
