//! SuiCap app shell (plan §3): tray, global hotkeys, commands and windows. All capture
//! work lives in `suicap-core`; this crate only drives it.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod autoadd;
mod badge;
mod commands;
mod feedback;
mod power;
mod state;
mod tray;
mod update;

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

fn is_target_key(app: &AppHandle, shortcut: &Shortcut) -> bool {
    let key = app.state::<AppState>().settings().hotkeys.target_app;
    !key.is_empty() && Shortcut::from_str(&key).is_ok_and(|s| s == *shortcut)
}

fn is_screenshot_key(app: &AppHandle, shortcut: &Shortcut) -> bool {
    let key = app.state::<AppState>().settings().hotkeys.screenshot;
    !key.is_empty() && Shortcut::from_str(&key).is_ok_and(|s| s == *shortcut)
}

fn is_record_toggle_key(app: &AppHandle, shortcut: &Shortcut) -> bool {
    let key = app.state::<AppState>().settings().hotkeys.record_toggle;
    !key.is_empty() && Shortcut::from_str(&key).is_ok_and(|s| s == *shortcut)
}

fn is_record_pause_key(app: &AppHandle, shortcut: &Shortcut) -> bool {
    let key = app.state::<AppState>().settings().hotkeys.record_pause;
    !key.is_empty() && Shortcut::from_str(&key).is_ok_and(|s| s == *shortcut)
}

fn is_arm_toggle_key(app: &AppHandle, shortcut: &Shortcut) -> bool {
    let key = app.state::<AppState>().settings().hotkeys.arm_toggle;
    !key.is_empty() && Shortcut::from_str(&key).is_ok_and(|s| s == *shortcut)
}

/// (Re)register the save shortcuts; failures (a key taken by another app) are shown
/// in the settings window.
pub fn register_hotkeys(app: &AppHandle) {
    let gs = app.global_shortcut();
    let _ = gs.unregister_all();
    let settings = app.state::<AppState>().settings();
    let mut errors = Vec::new();
    for key in settings.hotkeys.all() {
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

/// Release builds abort on panic and have no console, so the message would be lost: append it
/// (thread, location, message) to `%APPDATA%\SuiCap\crash.log` before the default hook runs.
fn install_crash_log() {
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        if let Some(path) = config::settings_path().map(|p| p.with_file_name("crash.log")) {
            use std::io::Write;
            let secs = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_secs());
            let thread = std::thread::current().name().unwrap_or("unnamed").to_owned();
            let bt = std::backtrace::Backtrace::force_capture();
            if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
                let _ = writeln!(
                    f,
                    "[unix {secs}] v{} thread '{thread}' {info}\n{bt}\n",
                    env!("CARGO_PKG_VERSION")
                );
            }
        }
        default(info);
    }));
}

fn main() {
    install_crash_log();
    let autostarted = std::env::args().any(|a| a == "--autostart");
    let app = tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            tray::open_settings(app)
        }))
        .plugin(
            tauri_plugin_global_shortcut::Builder::new()
                .with_handler(|app, shortcut, event| {
                    if event.state != ShortcutState::Pressed {
                        return;
                    }
                    if let Some(secs) = hotkey_secs(app, shortcut) {
                        app.state::<AppState>().save(app, secs);
                    } else if is_target_key(app, shortcut) {
                        let app = app.clone();
                        std::thread::spawn(move || commands::toggle_target_app(&app));
                    } else if is_screenshot_key(app, shortcut) {
                        let app = app.clone();
                        std::thread::spawn(move || app.state::<AppState>().screenshot(&app));
                    } else if is_record_toggle_key(app, shortcut) {
                        let app = app.clone();
                        std::thread::spawn(move || {
                            let _ = app.state::<AppState>().record_toggle(&app);
                        });
                    } else if is_record_pause_key(app, shortcut) {
                        let _ = app.state::<AppState>().record_pause();
                    } else if is_arm_toggle_key(app, shortcut) {
                        let app = app.clone();
                        std::thread::spawn(move || {
                            let st = app.state::<AppState>();
                            if st.is_armed() {
                                st.disarm(&app);
                            } else {
                                st.arm(&app);
                            }
                        });
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
            commands::record_start,
            commands::record_stop,
            commands::record_pause,
            commands::record_split,
            commands::config_get,
            commands::config_set,
            commands::default_clips_dir,
            commands::default_temp_dir,
            commands::displays,
            commands::estimate_mb,
            commands::open_clips_folder,
            commands::open_temp_folder,
            commands::video_preview,
            commands::close_video_preview,
            commands::reveal_last_clip,
            commands::pick_clips_dir,
            commands::pick_temp_dir,
            commands::audio_apps,
            commands::audio_inputs,
            commands::foreground_game,
            commands::capture_windows,
            commands::app_icon,
            commands::reveal_app,
            commands::present_apps,
            commands::open_settings,
            commands::quit,
            commands::codec_support,
            commands::badge_resize,
            commands::pick_badge_icon,
            commands::remove_badge_icon,
            commands::badge_icon_data,
            commands::clips_list,
            commands::clip_thumbnail,
            commands::clip_duration,
            commands::clip_trim,
            commands::clip_delete,
            commands::clip_open,
            commands::clip_reveal,
            commands::screenshot,
            commands::open_link,
            commands::app_info,
            commands::update_info,
            commands::update_check,
            commands::update_download,
            commands::update_install,
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
            let temp = app
                .path()
                .local_data_dir()
                .map(|d| d.join("SuiCap").join("Temp"))
                .unwrap_or_else(|_| std::env::temp_dir().join("SuiCap"));
            let (autostart, arm) = (settings.autostart, settings.arm_on_start);
            app.manage(AppState::new(settings, warning, path, clips, temp));
            let state = handle.state::<AppState>();
            suicap_core::mux::set_temp_dir(Some(state.temp_dir()));
            let removed = suicap_core::clips::cleanup_temp_files(&state.clips_dir())
                + suicap_core::clips::cleanup_temp_files(&state.temp_dir());
            if removed > 0 {
                eprintln!("cleaned up {removed} leftover temporary clip file(s)");
            }
            tray::create(&handle)?;
            power::watch(&handle);
            register_hotkeys(&handle);
            update::start(&handle);
            commands::apply_autostart(&handle, autostart);
            if arm {
                let h = handle.clone();
                std::thread::spawn(move || h.state::<AppState>().arm(&h));
            }
            autoadd::start(&handle);
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
        RunEvent::ExitRequested { code: Some(_), .. } => tray::shutdown(app),
        // Quit: close the overlay immediately and stop capture so saves in progress finish.
        RunEvent::Exit => {
            tray::shutdown(app);
            app.state::<AppState>().disarm(app);
        }
        _ => {}
    });
}
