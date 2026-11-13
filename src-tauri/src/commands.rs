//! Tauri commands (plan §6.3). Every argument is validated here; no command takes a
//! file path from the page (clip and folder actions use paths the app chose itself).

use std::{path::PathBuf, str::FromStr};

use serde::Serialize;
use suicap_core::{
    capture,
    config::{self, Settings},
    pipeline::VideoOptions,
};
use tauri::{AppHandle, Manager, State, WebviewWindow};
use tauri_plugin_autostart::ManagerExt as _;
use tauri_plugin_dialog::DialogExt;
use tauri_plugin_global_shortcut::Shortcut;

use crate::{
    state::{AppState, Status},
    tray,
};

type CmdResult<T> = Result<T, String>;

#[tauri::command]
pub fn status(state: State<'_, AppState>) -> Status {
    state.status()
}

#[tauri::command]
pub fn arm(app: AppHandle) {
    std::thread::spawn(move || app.state::<AppState>().arm(&app));
}

#[tauri::command]
pub fn disarm(app: AppHandle) {
    std::thread::spawn(move || app.state::<AppState>().disarm(&app));
}

/// `secs`: 1..=600, or `None` for the whole buffer.
#[tauri::command]
pub fn save_clip(app: AppHandle, secs: Option<u32>) -> CmdResult<()> {
    if secs.is_some_and(|s| !(1..=*config::BUFFER_SECS.end()).contains(&s)) {
        return Err("seconds must be 1 to 600".into());
    }
    app.state::<AppState>().save(&app, secs);
    Ok(())
}

#[tauri::command]
pub fn config_get(state: State<'_, AppState>) -> Settings {
    state.settings()
}

/// Folder the clips go to when none is chosen.
#[tauri::command]
pub fn default_clips_dir(state: State<'_, AppState>) -> String {
    state.default_clips_dir.to_string_lossy().into_owned()
}

/// Settings fields that change what is captured; editing any restarts capture.
fn capture_key(s: &Settings) -> String {
    let v = VideoOptions::from_settings(s);
    format!(
        "{:?}|{:?}|{}|{:?}|{:?}|{:?}|{:?}",
        v, s.audio, s.buffer_secs, s.container, s.clips_dir, s.display, s.cursor
    )
}

#[tauri::command]
pub fn config_set(app: AppHandle, settings: Settings) -> CmdResult<Status> {
    settings.validate().map_err(|e| e.to_string())?;
    for (_, key) in settings.hotkeys.bindings() {
        if !key.is_empty() {
            Shortcut::from_str(key).map_err(|e| format!("shortcut \"{key}\": {e}"))?;
        }
    }
    let state = app.state::<AppState>();
    let old = state.settings();
    settings.save(&state.settings_path).map_err(|e| e.to_string())?;
    state.set_settings(settings.clone());

    if old.hotkeys != settings.hotkeys {
        crate::register_hotkeys(&app);
    }
    if old.autostart != settings.autostart {
        apply_autostart(&app, settings.autostart);
    }
    if old.buffer_secs != settings.buffer_secs {
        state.update_status(&app, |st| st.buffer_secs = settings.buffer_secs);
    }
    let _ = tauri::Emitter::emit(&app, "settings", &settings);
    if capture_key(&old) != capture_key(&settings) && state.is_armed() {
        let app = app.clone();
        std::thread::spawn(move || {
            let st = app.state::<AppState>();
            st.disarm(&app);
            st.arm(&app);
        });
    }
    Ok(state.status())
}

pub fn apply_autostart(app: &AppHandle, on: bool) {
    let launcher = app.autolaunch();
    let result = if on { launcher.enable() } else { launcher.disable() };
    if let Err(e) = result {
        eprintln!("autostart: {e}");
    }
}

#[derive(Serialize)]
pub struct Display {
    pub number: u32,
    pub label: String,
    pub primary: bool,
}

/// Monitors on hardware adapters, for the display picker.
#[tauri::command]
pub fn displays() -> Vec<Display> {
    let mut list = Vec::new();
    for a in capture::adapters()
        .unwrap_or_default()
        .into_iter()
        .filter(|a| !a.software)
    {
        for o in capture::outputs(&a)
            .unwrap_or_default()
            .into_iter()
            .filter(|o| o.attached)
        {
            let Some(number) = o.device_name.trim_start_matches(r"\\.\DISPLAY").parse().ok() else {
                continue;
            };
            list.push(Display {
                number,
                label: format!("Display {number}: {}x{} @ {} Hz", o.width, o.height, o.refresh_hz),
                primary: o.is_primary(),
            });
        }
    }
    list.sort_by_key(|d| d.number);
    list
}

/// Buffer RAM upper bound for a settings draft (plan §4.3), assuming 1080p when the
/// preset uses the monitor size.
#[tauri::command]
pub fn estimate_mb(settings: Settings) -> u64 {
    let v = settings.video();
    let mut preset = VideoOptions::from_settings(&settings).preset;
    if preset.width == 0 {
        preset.width = 2560;
        preset.height = 1440;
    }
    let codec = v.codec.unwrap_or(config::Codec::Hevc);
    config::buffer_ram_estimate_mb(preset.max_bitrate_mbps(codec), settings.buffer_secs.min(600))
}

#[tauri::command]
pub fn open_clips_folder(state: State<'_, AppState>) -> CmdResult<()> {
    let dir = state.clips_dir();
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    explorer(&[dir.into_os_string()])
}

/// Select the last saved clip in Explorer.
#[tauri::command]
pub fn reveal_last_clip(state: State<'_, AppState>) -> CmdResult<()> {
    let Some(path) = state.status().last_clip.map(PathBuf::from) else {
        return Err("no clip saved yet".into());
    };
    if !path.starts_with(state.clips_dir()) || !path.is_file() {
        return Err("the last clip is no longer in the clips folder".into());
    }
    let mut arg = std::ffi::OsString::from("/select,");
    arg.push(path.as_os_str());
    explorer(&[arg])
}

fn explorer(args: &[std::ffi::OsString]) -> CmdResult<()> {
    let exe = std::env::var_os("WINDIR")
        .map(|w| PathBuf::from(w).join("explorer.exe"))
        .unwrap_or_else(|| PathBuf::from(r"C:\Windows\explorer.exe"));
    std::process::Command::new(exe)
        .args(args)
        .spawn()
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// Ask for a clips folder; `None` if cancelled.
#[tauri::command]
pub async fn pick_clips_dir(app: AppHandle) -> CmdResult<Option<String>> {
    let start = app.state::<AppState>().clips_dir();
    let picked = app.dialog().file().set_directory(start).blocking_pick_folder();
    match picked {
        None => Ok(None),
        Some(p) => p
            .into_path()
            .map(|p| Some(p.to_string_lossy().into_owned()))
            .map_err(|e| e.to_string()),
    }
}

#[tauri::command]
pub fn open_settings(app: AppHandle) {
    tray::open_settings(&app);
}

/// Sent by the tray menu page once it has measured itself.
#[tauri::command]
pub fn menu_ready(window: WebviewWindow, width: f64, height: f64) -> CmdResult<()> {
    if window.label() != tray::MENU {
        return Err("not the menu window".into());
    }
    let (w, h) = (width.clamp(120.0, 480.0), height.clamp(40.0, 800.0));
    tray::show_menu(&window, w, h).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn quit(app: AppHandle) {
    app.exit(0);
}
