//! Tauri commands (plan §6.3). Every argument is validated here; no command takes a
//! file path from the page (clip and folder actions use paths the app chose itself).

use std::{collections::HashMap, path::PathBuf, str::FromStr, sync::Mutex};

use serde::Serialize;
use suicap_core::{
    apps, capture, clips,
    config::{self, Settings},
    pipeline::VideoOptions,
};
use tauri::{AppHandle, Manager, State, WebviewWindow};
use tauri_plugin_autostart::ManagerExt as _;
use tauri_plugin_dialog::DialogExt;
use tauri_plugin_global_shortcut::Shortcut;

use crate::{
    state::{AppState, AudioLive, Status},
    tray, update,
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
pub fn record_start(app: AppHandle) -> CmdResult<()> {
    app.state::<AppState>().record_start(&app)
}

#[tauri::command]
pub fn record_stop(state: State<'_, AppState>) -> CmdResult<()> {
    state.record_stop()
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

/// Settings fields that change what is captured; editing any restarts capture. The on/off
/// switches of tracks the running session already has are left out: those mute live.
fn capture_key(s: &Settings, live: &AudioLive) -> String {
    let v = VideoOptions::from_settings(s);
    let mut audio = s.audio.clone();
    audio.tracks.mix |= live.mix;
    audio.mic |= live.mic;
    if live.mic {
        audio.mic_device = None; // switching microphones is live, like muting
    }
    audio.tracks.game |= live.game;
    audio.tracks.voice |= live.voice;
    audio.tracks.media |= live.media;
    // Moving, parking or releasing apps re-routes the running mixer live (no restart).
    audio.game.clear();
    audio.voice.clear();
    audio.media.clear();
    audio.ignored.clear();
    format!(
        "{:?}|{:?}|{}|{:?}|{:?}|{:?}|{:?}",
        v, audio, s.buffer_secs, s.container, s.clips_dir, s.display, s.cursor
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
    crate::badge::sync(&app);
    crate::tray::notify_menu_changed();
    // Track switches the session can honour take effect now, with no restart.
    state.apply_live(&settings.audio);
    let started = state.status().audio_live;
    if capture_key(&old, &started) != capture_key(&settings, &started) && state.is_armed() {
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

pub fn open_clips_dir(state: &AppState) -> CmdResult<()> {
    let dir = state.clips_dir();
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    explorer(&[dir.into_os_string()])
}

#[tauri::command]
pub fn open_clips_folder(state: State<'_, AppState>) -> CmdResult<()> {
    open_clips_dir(&state)
}

// ---- Clip library (plan §6.3). Every path is re-resolved inside the clips folder by
// `suicap_core::clips`; the page can only name files that folder holds.

#[tauri::command]
pub async fn clips_list(state: State<'_, AppState>) -> CmdResult<Vec<clips::ClipEntry>> {
    let dir = state.clips_dir();
    tauri::async_runtime::spawn_blocking(move || clips::list(&dir))
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn clip_thumbnail(state: State<'_, AppState>, path: String) -> CmdResult<Option<String>> {
    let dir = state.clips_dir();
    tauri::async_runtime::spawn_blocking(move || clips::thumbnail(&dir, &PathBuf::from(path), 256).ok())
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn clip_duration(state: State<'_, AppState>, path: String) -> CmdResult<Option<f64>> {
    let dir = state.clips_dir();
    tauri::async_runtime::spawn_blocking(move || clips::duration_secs(&dir, &PathBuf::from(path)))
        .await
        .map_err(|e| e.to_string())?
}

/// Lossless copy of a range as a new file beside the original; returns its path.
#[tauri::command]
pub async fn clip_trim(state: State<'_, AppState>, path: String, start: f64, end: f64) -> CmdResult<String> {
    let dir = state.clips_dir();
    tauri::async_runtime::spawn_blocking(move || {
        clips::trim(&dir, &PathBuf::from(path), start, end).map(|p| p.to_string_lossy().into_owned())
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Moves the file to the Recycle Bin, or deletes it for good when `permanent`.
#[tauri::command]
pub async fn clip_delete(state: State<'_, AppState>, path: String, permanent: bool) -> CmdResult<()> {
    let dir = state.clips_dir();
    tauri::async_runtime::spawn_blocking(move || clips::delete(&dir, &PathBuf::from(path), permanent))
        .await
        .map_err(|e| e.to_string())?
}

/// Open a clip in the default app.
#[tauri::command]
pub fn clip_open(state: State<'_, AppState>, path: String) -> CmdResult<()> {
    let real = clips::resolve(&state.clips_dir(), &PathBuf::from(path))?;
    explorer(&[real.into_os_string()])
}

/// Show a clip selected in Explorer.
#[tauri::command]
pub fn clip_reveal(state: State<'_, AppState>, path: String) -> CmdResult<()> {
    let real = clips::resolve(&state.clips_dir(), &PathBuf::from(path))?;
    let mut arg = std::ffi::OsString::from("/select,");
    arg.push(real.as_os_str());
    explorer(&[arg])
}

#[tauri::command]
pub fn screenshot(app: AppHandle) {
    std::thread::spawn(move || app.state::<AppState>().screenshot(&app));
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

#[derive(Serialize)]
pub struct AudioApp {
    pub exe: String,
    pub path: String,
    pub name: String,
    pub playing: bool,
}

/// Programs with an audio session right now (playing ones first), for dragging onto tracks.
#[tauri::command]
pub async fn audio_apps() -> Vec<AudioApp> {
    tauri::async_runtime::spawn_blocking(|| {
        apps::audio_apps()
            .into_iter()
            .map(|a| AudioApp {
                exe: a.exe,
                path: a.path,
                name: a.name,
                playing: a.playing,
            })
            .collect()
    })
    .await
    .unwrap_or_default()
}

/// Which of `exes` are on this machine (running or installed), lowercase. The page hides the
/// rest from the track lists; settings keep them, so they work again once installed.
#[tauri::command]
pub async fn present_apps(exes: Vec<String>) -> CmdResult<Vec<String>> {
    if exes.len() > 200 {
        return Err("too many programs".into());
    }
    tauri::async_runtime::spawn_blocking(move || apps::present(&exes))
        .await
        .map_err(|e| e.to_string())
}

/// Select a program's file in Explorer. The path is found here from the exe name, never taken
/// from the page.
#[tauri::command]
pub async fn reveal_app(exe: String) -> CmdResult<()> {
    let ok = (5..=64).contains(&exe.len())
        && exe.to_ascii_lowercase().ends_with(".exe")
        && !exe.chars().any(|c| c.is_control() || r#"<>:"/\|?*"#.contains(c));
    if !ok {
        return Err("not a program file name".into());
    }
    let path = tauri::async_runtime::spawn_blocking(move || apps::app_path(&exe))
        .await
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "could not find where that program is installed".to_string())?;
    let mut arg = std::ffi::OsString::from("/select,");
    arg.push(path.as_os_str());
    explorer(&[arg])
}

#[derive(Serialize)]
pub struct InputDevice {
    pub id: String,
    pub name: String,
    pub is_default: bool,
}

/// Active microphones, the system default first. The page polls this so unplugging or
/// plugging a headset shows up without reopening the window.
#[tauri::command]
pub async fn audio_inputs() -> Vec<InputDevice> {
    tauri::async_runtime::spawn_blocking(|| {
        apps::input_devices()
            .into_iter()
            .map(|d| InputDevice {
                id: d.id,
                name: d.name,
                is_default: d.is_default,
            })
            .collect()
    })
    .await
    .unwrap_or_default()
}

#[derive(Serialize)]
pub struct WindowEntry {
    pub pid: u32,
    pub exe: String,
    pub title: String,
    pub class: String,
    pub width: u32,
    pub height: u32,
}

/// Open windows that can be recorded, for the source picker.
#[tauri::command]
pub async fn capture_windows() -> Vec<WindowEntry> {
    tauri::async_runtime::spawn_blocking(|| {
        suicap_core::targets::list_windows()
            .into_iter()
            .map(|w| WindowEntry {
                pid: w.pid,
                exe: w.exe,
                title: w.title,
                class: w.class,
                width: w.width,
                height: w.height,
            })
            .collect()
    })
    .await
    .unwrap_or_default()
}

/// The shortcut: record the app in front, or go back to the whole display.
pub fn toggle_target_app(app: &AppHandle) {
    use suicap_core::config::SourceKind;
    let mut s = app.state::<AppState>().settings();
    let msg = if s.source.kind == SourceKind::App {
        s.source.kind = SourceKind::Display;
        "Recording the whole display".to_owned()
    } else if let Some(w) = suicap_core::targets::foreground_window() {
        s.source.kind = SourceKind::App;
        s.source.exe = w.exe.clone();
        s.source.title = String::new();
        format!("Recording only {}", w.exe)
    } else {
        crate::feedback::notify(
            app,
            "Record the app in front",
            "There is no app window in front to record.",
        );
        return;
    };
    match config_set(app.clone(), s) {
        Ok(_) => crate::feedback::notify(app, "Capture source changed", &msg),
        Err(e) => crate::feedback::notify(app, "Could not change the capture source", &e),
    }
}

/// The full-screen app in front right now (a game, usually), for the status bar.
#[tauri::command]
pub async fn foreground_game() -> Option<String> {
    tauri::async_runtime::spawn_blocking(suicap_core::audio::foreground_game_exe)
        .await
        .ok()
        .flatten()
}

static ICON_CACHE: Mutex<Option<HashMap<String, String>>> = Mutex::new(None);

/// Frees cached app icon data URLs when Settings closes.
pub fn clear_icon_cache() {
    if let Ok(mut g) = ICON_CACHE.lock() {
        *g = None;
    }
}

/// An app's icon as a PNG data URL; `None` when it cannot be found. Found icons are cached.
#[tauri::command]
pub async fn app_icon(exe: String) -> CmdResult<Option<String>> {
    let ok = (5..=64).contains(&exe.len())
        && exe.to_ascii_lowercase().ends_with(".exe")
        && !exe.chars().any(|c| c.is_control() || r#"<>:"/\|?*"#.contains(c));
    if !ok {
        return Err("not a program file name".into());
    }
    let key = exe.to_ascii_lowercase();
    if let Some(hit) = ICON_CACHE
        .lock()
        .ok()
        .and_then(|c| c.as_ref().and_then(|m| m.get(&key).cloned()))
    {
        return Ok(Some(hit));
    }
    let url = tauri::async_runtime::spawn_blocking(move || {
        apps::app_icon_png(&exe).map(|png| format!("data:image/png;base64,{}", apps::base64(&png)))
    })
    .await
    .map_err(|e| e.to_string())?;
    if let (Some(u), Ok(mut c)) = (&url, ICON_CACHE.lock()) {
        c.get_or_insert_with(HashMap::new).insert(key, u.clone());
    }
    Ok(url)
}

#[tauri::command]
pub fn open_settings(app: AppHandle) {
    tray::open_settings(&app);
}

/// Fixed destinations only: the page names one, never supplies a URL.
#[tauri::command]
pub fn open_link(kind: String) -> CmdResult<()> {
    let url = match kind.as_str() {
        "coffee" => update::COFFEE_URL.to_string(),
        "support" => format!(
            "mailto:{}?subject=SuiCap%20{}",
            update::SUPPORT_EMAIL,
            env!("CARGO_PKG_VERSION")
        ),
        "releases" => format!("https://github.com/{}/releases", update::REPO),
        _ => return Err("unknown link".into()),
    };
    explorer(&[url.into()])
}

#[tauri::command]
pub fn app_info() -> serde_json::Value {
    serde_json::json!({
        "version": env!("CARGO_PKG_VERSION"),
        "developer": update::DEVELOPER,
        "email": update::SUPPORT_EMAIL,
    })
}

#[tauri::command]
pub fn update_info() -> update::Info {
    update::info()
}

#[tauri::command]
pub fn update_check(app: AppHandle) {
    update::check_now(&app);
}

#[tauri::command]
pub fn update_download(app: AppHandle) {
    update::download(&app);
}

#[tauri::command]
pub fn update_install(app: AppHandle) {
    update::install(&app);
}

/// Where the chosen badge icon is kept: beside the settings file.
fn badge_icon_path(state: &AppState) -> CmdResult<PathBuf> {
    state
        .settings_path
        .parent()
        .map(|d| d.join("badge-icon.bin"))
        .ok_or_else(|| "no settings folder".to_string())
}

const MAX_ICON_BYTES: u64 = 1 << 20;

/// Copy a chosen image into the app folder after checking its size and real type.
fn store_badge_icon(from: &std::path::Path, to: &std::path::Path) -> CmdResult<()> {
    let len = std::fs::metadata(from).map_err(|e| e.to_string())?.len();
    if len > MAX_ICON_BYTES {
        return Err("that image is larger than 1 MB; pick a smaller one".into());
    }
    let bytes = std::fs::read(from).map_err(|e| e.to_string())?;
    if clips::image_mime(&bytes).is_none() {
        return Err("use a PNG, JPEG or WebP image".into());
    }
    let mut tmp = to.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    std::fs::write(&tmp, &bytes).map_err(|e| e.to_string())?;
    suicap_core::mux::replace_file(&tmp, to).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        e.to_string()
    })
}

/// Ask for an image to use as the badge icon; true when one was stored. The page never
/// supplies a path: the file comes from the native dialog.
#[tauri::command]
pub async fn pick_badge_icon(app: AppHandle) -> CmdResult<bool> {
    let dest = badge_icon_path(&app.state::<AppState>())?;
    let picked = app
        .dialog()
        .file()
        .add_filter("Images", &["png", "jpg", "jpeg", "webp"])
        .blocking_pick_file();
    let Some(file) = picked else { return Ok(false) };
    let path = file.into_path().map_err(|e| e.to_string())?;
    tauri::async_runtime::spawn_blocking(move || store_badge_icon(&path, &dest))
        .await
        .map_err(|e| e.to_string())??;
    let _ = tauri::Emitter::emit(&app, "badge_icon", ());
    Ok(true)
}

#[tauri::command]
pub fn remove_badge_icon(app: AppHandle) -> CmdResult<()> {
    let path = badge_icon_path(&app.state::<AppState>())?;
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.to_string()),
    }
    let _ = tauri::Emitter::emit(&app, "badge_icon", ());
    Ok(())
}

/// The chosen icon as a data URL, or `None` when there is none (or it is no longer a valid image).
#[tauri::command]
pub fn badge_icon_data(state: State<'_, AppState>) -> Option<String> {
    let bytes = std::fs::read(badge_icon_path(&state).ok()?).ok()?;
    let mime = clips::image_mime(&bytes)?;
    Some(format!("data:{mime};base64,{}", clips::base64(&bytes)))
}

/// The badge page measured itself (logical px): size the window to it and show it.
#[tauri::command]
pub fn badge_resize(app: AppHandle, window: WebviewWindow, width: f64, height: f64) -> CmdResult<()> {
    if window.label() != crate::badge::BADGE {
        return Err("not the badge window".into());
    }
    crate::badge::resized(&app, width, height);
    Ok(())
}

/// What this GPU can encode, so the page can grey out the rest. Probed once per display and kept.
#[tauri::command]
pub async fn codec_support(
    state: State<'_, AppState>,
    refresh: bool,
) -> CmdResult<suicap_core::pipeline::CodecSupport> {
    static CACHE: Mutex<Option<(Option<u32>, suicap_core::pipeline::CodecSupport)>> = Mutex::new(None);
    let display = state.settings().display;
    if !refresh
        && let Some((d, found)) = CACHE.lock().unwrap_or_else(|e| e.into_inner()).as_ref()
        && *d == display
    {
        return Ok(found.clone());
    }
    let found = tauri::async_runtime::spawn_blocking(move || suicap_core::pipeline::codec_support(display))
        .await
        .map_err(|e| e.to_string())??;
    *CACHE.lock().unwrap_or_else(|e| e.into_inner()) = Some((display, found.clone()));
    Ok(found)
}

#[tauri::command]
pub fn quit(app: AppHandle) {
    crate::tray::shutdown(&app);
    app.exit(0);
}
