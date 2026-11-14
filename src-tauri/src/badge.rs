//! The "recording" badge: a small click-through window in one corner of the recorded
//! monitor, shown while recording runs (and, if the user wants, while replay is on). It is
//! excluded from capture (`WDA_EXCLUDEFROMCAPTURE`), so it never appears in a clip or a
//! screenshot. If Windows refuses the exclusion the badge is not shown at all rather than
//! recorded.
//!
//! The page measures its own content and reports it (`badge_resize`), so the window is
//! exactly as wide as the text, however the size and options are set. A new window stays
//! hidden until that first measurement arrives.

use std::sync::Mutex;

use suicap_core::config::BadgeCorner;
use tauri::{AppHandle, Manager, WebviewUrl, WebviewWindow, WebviewWindowBuilder};
use windows::Win32::{
    Foundation::HWND,
    UI::WindowsAndMessaging::{SetWindowDisplayAffinity, WDA_EXCLUDEFROMCAPTURE},
};

use crate::state::{AppState, Phase};

pub const BADGE: &str = "badge";
/// Room the page lays out in before it reports its real size (logical px).
const MEASURE_SIZE: (f64, f64) = (420.0, 120.0);

/// What the badge should look like: corner, monitor and edge distance. `None` = hidden.
type Want = Option<(BadgeCorner, Option<u32>, u32)>;

/// What was last asked for (cheap check on the caller's thread) and what is on screen.
static REQUESTED: Mutex<Want> = Mutex::new(None);
static REALIZED: Mutex<Want> = Mutex::new(None);
/// Logical size the page last reported; `None` until the current window has measured itself.
static SIZE: Mutex<Option<(f64, f64)>> = Mutex::new(None);

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn wanted(app: &AppHandle) -> Want {
    let state = app.state::<AppState>();
    let s = state.settings();
    let st = state.status();
    let live = st.recording || (s.badge_during_replay && st.phase == Phase::Armed);
    (s.recording_badge && live).then_some((s.badge_corner, s.display, s.badge_margin))
}

/// True when the badge is up and set to announce saved clips, so it says so itself and the
/// Windows notification would only repeat it.
pub fn announces_clips(app: &AppHandle) -> bool {
    wanted(app).is_some() && app.state::<AppState>().settings().badge_clip_flash
}

/// Show, move or hide the badge to match the settings and recording state. Cheap when
/// nothing changed (it runs on every status tick); the window work happens on its own thread.
pub fn sync(app: &AppHandle) {
    let want = wanted(app);
    {
        let mut requested = lock(&REQUESTED);
        if *requested == want {
            return;
        }
        *requested = want;
    }
    let app = app.clone();
    std::thread::spawn(move || {
        let mut realized = lock(&REALIZED);
        let want = wanted(&app); // newest state, in case several changes queued up
        if *realized == want {
            return;
        }
        *realized = match want {
            Some(w) => place(&app, w).then_some(w),
            None => {
                *lock(&SIZE) = None;
                if let Some(w) = app.get_webview_window(BADGE) {
                    let _ = w.close();
                }
                None
            }
        };
    });
}

/// The page reported its content size: size the window to it, put it in its corner, show it.
pub fn resized(app: &AppHandle, width: f64, height: f64) {
    let size = (width.clamp(24.0, 800.0), height.clamp(12.0, 300.0));
    *lock(&SIZE) = Some(size);
    let app = app.clone();
    std::thread::spawn(move || {
        let realized = *lock(&REALIZED);
        let Some(want) = realized.or(*lock(&REQUESTED)) else {
            return;
        };
        if let Some(window) = app.get_webview_window(BADGE)
            && position(&window, want, size).is_ok()
        {
            let _ = window.show();
        }
    });
}

/// Say why the badge is missing: a toast, since nothing else tells the user.
fn fail(app: &AppHandle, why: &str) {
    eprintln!("recording badge: {why}");
    crate::feedback::notify(app, "Recording badge unavailable", why);
}

/// Create the badge window, or move the existing one; false if it could not be hidden from capture.
fn place(app: &AppHandle, want: (BadgeCorner, Option<u32>, u32)) -> bool {
    if let Some(window) = app.get_webview_window(BADGE) {
        // Already measured: move it. Otherwise the pending measurement will place it.
        let size = *lock(&SIZE);
        return size.is_none_or(|size| position(&window, want, size).is_ok());
    }
    build(app).is_some()
}

fn build(app: &AppHandle) -> Option<WebviewWindow> {
    let window = WebviewWindowBuilder::new(app, BADGE, WebviewUrl::App("index.html".into()))
        .title("SuiCap recording")
        .inner_size(MEASURE_SIZE.0, MEASURE_SIZE.1)
        .position(-32000.0, -32000.0)
        .decorations(false)
        .transparent(true)
        .shadow(false)
        .resizable(false)
        .always_on_top(true)
        .skip_taskbar(true)
        .focusable(false)
        .focused(false)
        .visible(false)
        .build()
        .map_err(|e| fail(app, &format!("could not create its window ({e})")))
        .ok()?;
    // Hidden from capture before it is ever shown; no exclusion, no badge.
    let excluded = window.hwnd().map_err(|e| e.to_string()).and_then(|h| {
        // SAFETY: a valid top-level window handle owned by this process.
        unsafe { SetWindowDisplayAffinity(HWND(h.0), WDA_EXCLUDEFROMCAPTURE) }.map_err(|e| e.to_string())
    });
    if let Err(e) = excluded {
        fail(
            app,
            &format!("Windows would not hide it from capture ({e}), so it is not shown"),
        );
        let _ = window.close();
        return None;
    }
    let _ = window.set_ignore_cursor_events(true); // clicks pass through to the game
    Some(window)
}

fn position(
    window: &WebviewWindow,
    (corner, display, margin): (BadgeCorner, Option<u32>, u32),
    (lw, lh): (f64, f64),
) -> tauri::Result<()> {
    let monitors = window.available_monitors()?;
    let by_number = display.and_then(|n| {
        let name = format!(r"\\.\DISPLAY{n}");
        monitors
            .iter()
            .find(|m| m.name().is_some_and(|x| x.eq_ignore_ascii_case(&name)))
            .cloned()
    });
    let monitor = match by_number {
        Some(m) => m,
        None => window.primary_monitor()?.ok_or(tauri::Error::WindowNotFound)?,
    };
    let scale = monitor.scale_factor();
    let (w, h) = ((lw * scale).ceil() as i32, (lh * scale).ceil() as i32);
    let margin = (f64::from(margin) * scale).round() as i32;
    let (mp, ms) = (monitor.position(), monitor.size());
    let (right, bottom) = (
        mp.x + ms.width as i32 - w - margin,
        mp.y + ms.height as i32 - h - margin,
    );
    let (left, top) = (mp.x + margin, mp.y + margin);
    let (x, y) = match corner {
        BadgeCorner::TopLeft => (left, top),
        BadgeCorner::TopRight => (right, top),
        BadgeCorner::BottomLeft => (left, bottom),
        BadgeCorner::BottomRight => (right, bottom),
    };
    window.set_size(tauri::PhysicalSize::new(w as u32, h as u32))?;
    window.set_position(tauri::PhysicalPosition::new(x, y))
}
