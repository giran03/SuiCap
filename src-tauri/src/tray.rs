//! Tray icon, native Win32 layered tray menu, and the Settings window.
//!
//! RAM budget (plan §3): the Settings window is created on demand and destroyed on close so
//! WebView2 stays unloaded while the app sits in the tray. The tray right-click menu is a
//! native Win32 layered window (`UpdateLayeredWindow`) synced to the app's theme tokens,
//! so right-clicking the tray icon opens instantaneously (< 2 ms) without starting WebView2.

use std::{
    ffi::c_void,
    sync::{
        Mutex, OnceLock,
        atomic::{AtomicBool, AtomicIsize, Ordering},
    },
    time::{Duration, Instant},
};

use suicap_core::config::Theme;
use tauri::{
    AppHandle, Manager, PhysicalPosition, Rect, WebviewUrl, WebviewWindowBuilder, WindowEvent,
    image::Image,
    tray::{MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent},
};
use windows::{
    Win32::{
        Foundation::{COLORREF, HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, SIZE, WPARAM},
        Graphics::Gdi::{
            AC_SRC_ALPHA, AC_SRC_OVER, BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BLENDFUNCTION, CLEARTYPE_QUALITY,
            CLIP_DEFAULT_PRECIS, CreateCompatibleDC, CreateDIBSection, CreateFontW, DEFAULT_CHARSET, DEFAULT_PITCH,
            DIB_RGB_COLORS, DT_LEFT, DT_NOPREFIX, DT_RIGHT, DT_SINGLELINE, DT_VCENTER, DeleteDC, DeleteObject,
            DrawTextW, FF_SWISS, GdiFlush, HBITMAP, HDC, HFONT, HGDIOBJ, OUT_DEFAULT_PRECIS, SelectObject, SetBkMode,
            SetTextColor, TRANSPARENT,
        },
        System::Registry::{HKEY_CURRENT_USER, RRF_RT_REG_DWORD, RegGetValueW},
        UI::{
            Input::KeyboardAndMouse::{
                GetAsyncKeyState, TME_LEAVE, TRACKMOUSEEVENT, TrackMouseEvent, VK_DOWN, VK_ESCAPE, VK_LBUTTON,
                VK_RBUTTON, VK_RETURN, VK_SPACE, VK_UP,
            },
            WindowsAndMessaging::{
                CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GWLP_USERDATA, GetCursorPos,
                GetForegroundWindow, GetMessageW, GetWindowLongPtrW, GetWindowRect, HTCLIENT, IDC_ARROW, KillTimer,
                LoadCursorW, MSG, PostMessageW, PostQuitMessage, RegisterClassExW, SPI_GETCLIENTAREAANIMATION, SW_SHOW,
                SetCursor, SetForegroundWindow, SetTimer, SetWindowLongPtrW, ShowWindow, SystemParametersInfoW,
                TranslateMessage, ULW_ALPHA, UpdateLayeredWindow, WA_INACTIVE, WM_ACTIVATE, WM_APP, WM_CLOSE,
                WM_DESTROY, WM_KEYDOWN, WM_KILLFOCUS, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSEMOVE, WM_NCHITTEST,
                WM_SETCURSOR, WM_SETTINGCHANGE, WM_TIMER, WNDCLASSEXW, WS_EX_LAYERED, WS_EX_TOOLWINDOW, WS_EX_TOPMOST,
                WS_POPUP,
            },
        },
    },
    core::w,
};

use crate::state::{AppState, Phase, Status};

pub const SETTINGS: &str = "settings";
/// Disables unused Chromium background features in WebView2 to trim per-webview RAM.
pub const WEBVIEW_TRIM_ARGS: &str = "--disable-features=Translate,msImplicitSignin,AutofillServerCommunication,MediaRouter,DialMediaRouteProvider,OptimizationHints";
const TRAY_ID: &str = "main";
/// Gap between the menu and the tray icon / taskbar.
const GAP: i32 = 6;
const WM_APP_REFRESH: u32 = WM_APP + 1;
/// Not re-exported without `Win32_UI_Controls`; fixed Win32 value from `WinUser.h`.
const WM_MOUSELEAVE: u32 = 0x02A3;
const TIMER_ID: usize = 1;

/// Tray icon rectangle and cursor of a right-click, in physical pixels.
#[derive(Debug, Clone, Copy)]
struct Anchor {
    icon: Px,
    cursor: (i32, i32),
}

/// When the menu last closed on blur: a right-click on the icon while it is open
/// first blurs it, and that click must not reopen it.
static LAST_MENU_CLOSE: Mutex<Option<Instant>> = Mutex::new(None);
static SHOWN_PHASE: Mutex<Option<(Phase, String)>> = Mutex::new(None);
static MENU_HWND: AtomicIsize = AtomicIsize::new(0);
static MENU_OPEN: AtomicBool = AtomicBool::new(false);

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// A physical-pixel rectangle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Px {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

impl Px {
    fn right(&self) -> i32 {
        self.x + self.w
    }
    fn bottom(&self) -> i32 {
        self.y + self.h
    }
}

/// Where to put a `size` menu for a tray `icon`: beside the icon, away from the
/// taskbar edge, inside the work area, never over the icon.
pub fn place_menu(icon: Px, monitor: Px, work: Px, size: (i32, i32)) -> (i32, i32) {
    let (w, h) = size;
    // Taskbar edge: the side where the work area is short of the monitor; with an
    // auto-hide taskbar there is none, so use the monitor edge nearest the icon.
    let cx = icon.x + icon.w / 2;
    let cy = icon.y + icon.h / 2;
    let edge = if work.bottom() < monitor.bottom() {
        'b'
    } else if work.y > monitor.y {
        't'
    } else if work.x > monitor.x {
        'l'
    } else if work.right() < monitor.right() {
        'r'
    } else {
        let d = [
            (monitor.bottom() - cy, 'b'),
            (cy - monitor.y, 't'),
            (cx - monitor.x, 'l'),
            (monitor.right() - cx, 'r'),
        ];
        d.iter().min_by_key(|(dist, _)| *dist).map_or('b', |(_, e)| *e)
    };
    let clamp = |v: i32, lo: i32, hi: i32| v.min(hi - 1).max(lo);
    let fit_x = |x: i32| clamp(x, work.x, work.right() - w + 1);
    let fit_y = |y: i32| clamp(y, work.y, work.bottom() - h + 1);
    match edge {
        't' => (fit_x(cx - w / 2), (icon.bottom() + GAP).max(work.y + GAP)),
        'l' => ((icon.right() + GAP).max(work.x + GAP), fit_y(icon.y)),
        'r' => ((icon.x - GAP - w).min(work.right() - GAP - w), fit_y(icon.y)),
        _ => (fit_x(cx - w / 2), (icon.y - GAP - h).min(work.bottom() - GAP - h)),
    }
}

/// The logo's small-size cut on the 256 grid: two slices and the tip (the last entry).
/// Each polygon is convex, listed in order.
const GLYPH: [&[(f32, f32)]; 3] = [
    &[(48.3, 36.0), (96.3, 63.7), (96.3, 192.3), (48.3, 220.0)],
    &[(110.3, 71.8), (158.3, 99.5), (158.3, 156.5), (110.3, 184.2)],
    &[(172.3, 107.4), (207.7, 128.0), (172.3, 148.6)],
];

fn inside(poly: &[(f32, f32)], x: f32, y: f32) -> bool {
    let mut sign = 0.0f32;
    for i in 0..poly.len() {
        let (ax, ay) = poly[i];
        let (bx, by) = poly[(i + 1) % poly.len()];
        let cross = (bx - ax) * (y - ay) - (by - ay) * (x - ax);
        if cross != 0.0 {
            if sign != 0.0 && cross.signum() != sign.signum() {
                return false;
            }
            sign = cross;
        }
    }
    true
}

/// A 32x32 tray glyph: the logo's slices, with the play tip showing the state (red when
/// armed, grey while starting, amber on error; all grey when off).
fn icon_image(phase: Phase) -> Image<'static> {
    const N: usize = 32;
    const SS: usize = 4; // samples per pixel side
    let (body, tip): ([u8; 3], [u8; 3]) = match phase {
        Phase::Armed => ([240, 240, 240], [239, 68, 68]),
        Phase::Starting | Phase::Recovering => ([240, 240, 240], [160, 160, 160]),
        Phase::Error => ([245, 158, 11], [245, 158, 11]),
        Phase::Idle => ([170, 170, 170], [170, 170, 170]),
    };
    let k = 256.0 / N as f32;
    let mut rgba = vec![0u8; N * N * 4];
    for y in 0..N {
        for x in 0..N {
            let mut hits = [0usize; 3];
            for sy in 0..SS {
                for sx in 0..SS {
                    let px = (x as f32 + (sx as f32 + 0.5) / SS as f32) * k;
                    let py = (y as f32 + (sy as f32 + 0.5) / SS as f32) * k;
                    for (h, poly) in hits.iter_mut().zip(GLYPH) {
                        *h += usize::from(inside(poly, px, py));
                    }
                }
            }
            let total = (SS * SS) as f32;
            let tip_a = hits[2] as f32 / total;
            let body_a = (hits[0] + hits[1]) as f32 / total;
            let (col, a) = if tip_a > 0.0 { (tip, tip_a) } else { (body, body_a) };
            let i = (y * N + x) * 4;
            rgba[i..i + 3].copy_from_slice(&col);
            rgba[i + 3] = (a * 255.0) as u8;
        }
    }
    Image::new_owned(rgba, N as u32, N as u32)
}

fn tooltip(st: &Status) -> String {
    match st.phase {
        Phase::Armed => format!("SuiCap: replay on ({} s)", st.buffer_secs),
        Phase::Starting => "SuiCap: starting".into(),
        Phase::Recovering => "SuiCap: restarting capture".into(),
        Phase::Idle => "SuiCap: replay off".into(),
        Phase::Error => "SuiCap: capture stopped (open for details)".into(),
    }
}

/// Take everything down at once: windows, badge and the tray icon go first (so nothing lingers
/// on screen while capture winds down), and a watchdog ends the process if the orderly exit
/// stalls, so no SuiCap process is left behind. Safe to call more than once.
pub fn shutdown(app: &AppHandle) {
    static STARTED: std::sync::Once = std::sync::Once::new();
    crate::badge::close_now(app);
    let _ = app.remove_tray_by_id(TRAY_ID);
    STARTED.call_once(|| {
        std::thread::spawn(|| {
            std::thread::sleep(std::time::Duration::from_secs(4));
            std::process::exit(0);
        });
    });
}

pub fn create(app: &AppHandle) -> tauri::Result<TrayIcon> {
    TrayIconBuilder::with_id(TRAY_ID)
        .icon(icon_image(Phase::Idle))
        .tooltip("SuiCap")
        .show_menu_on_left_click(false)
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click {
                button,
                button_state: MouseButtonState::Up,
                rect,
                position,
                ..
            } = event
            {
                let app = tray.app_handle();
                match button {
                    MouseButton::Left => open_settings(app),
                    MouseButton::Right => open_menu(app, rect, position),
                    _ => {}
                }
            }
        })
        .build(app)
}

/// Notify an open native tray menu that status, settings, or theme changed.
pub fn notify_menu_changed() {
    let raw = MENU_HWND.load(Ordering::Acquire);
    if raw != 0 {
        // SAFETY: `raw` is a live HWND cleared synchronously in `WM_DESTROY`.
        unsafe {
            let _ = PostMessageW(Some(HWND(raw as *mut c_void)), WM_APP_REFRESH, WPARAM(0), LPARAM(0));
        }
    }
}

/// Update icon and tooltip when the phase changes (status ticks every second).
pub fn refresh(app: &AppHandle, st: &Status) {
    notify_menu_changed();
    let tip = tooltip(st);
    let mut shown = lock(&SHOWN_PHASE);
    if shown.as_ref().is_some_and(|(p, t)| *p == st.phase && *t == tip) {
        return;
    }
    if let Some(tray) = app.tray_by_id(TRAY_ID) {
        let _ = tray.set_icon(Some(icon_image(st.phase)));
        let _ = tray.set_tooltip(Some(&tip));
    }
    *shown = Some((st.phase, tip));
}

pub fn open_settings(app: &AppHandle) {
    suicap_core::apps::warm_up(); // the audio page hides apps that are not installed
    if let Some(w) = app.get_webview_window(SETTINGS) {
        let _ = w.unminimize();
        let _ = w.show();
        let _ = w.set_focus();
        return;
    }
    // Built off the event-loop thread: with no webview open, WebView2 has to start up and
    // needs the loop free to do it (building inline from a tray event stalls).
    let app = app.clone();
    std::thread::spawn(move || {
        let built = WebviewWindowBuilder::new(&app, SETTINGS, WebviewUrl::App("index.html".into()))
            .title("SuiCap")
            .additional_browser_args(WEBVIEW_TRIM_ARGS)
            .inner_size(1280.0, 780.0)
            .min_inner_size(1280.0, 640.0)
            .decorations(false) // the page draws its own title bar (ui/TitleBar.svelte)
            .disable_drag_drop_handler() // the audio page uses HTML5 drag and drop
            .center()
            .visible(false) // the page shows it once styled, so no white flash
            .build();
        match built {
            Ok(w) => {
                w.on_window_event(|event| {
                    if matches!(event, WindowEvent::Destroyed) {
                        suicap_core::apps::clear_caches();
                        crate::commands::clear_icon_cache();
                    }
                });
            }
            Err(e) => eprintln!("settings window: {e}"),
        }
    });
}

const LOGICAL_W: f32 = 252.0;
const LOGICAL_H: f32 = 374.0;
const LOGICAL_PAD: f32 = 6.0;
const LOGICAL_HEAD_H: f32 = 32.0;
const LOGICAL_SEP_H: f32 = 11.0;
const LOGICAL_ITEM_H: f32 = 33.0;
const LOGICAL_CARD_R: f32 = 14.0;
const LOGICAL_ITEM_R: f32 = 9.0;
const ITEM_COUNT: usize = 9;

/// Palette tokens matching `ui/theme.css` (`[R, G, B]`).
#[derive(Debug, Clone, Copy, PartialEq)]
struct Palette {
    surface_solid: [u8; 3],
    line: [u8; 3],
    line_strong: [u8; 3],
    surface_3: [u8; 3],
    text: [u8; 3],
    text_2: [u8; 3],
    text_3: [u8; 3],
    accent: [u8; 3],
    accent_soft_alpha: f32,
    warn: [u8; 3],
    rec: [u8; 3],
    rec_soft: [u8; 3],
}

fn palette_for(light: bool) -> Palette {
    if light {
        Palette {
            surface_solid: [255, 255, 255],
            line: [236, 236, 237],
            line_strong: [217, 217, 219],
            surface_3: [233, 234, 235],
            text: [20, 22, 26],
            text_2: [105, 106, 109],
            text_3: [156, 157, 159],
            accent: [27, 143, 240],
            accent_soft_alpha: 0.14,
            warn: [180, 83, 9],
            rec: [229, 54, 43],
            rec_soft: [252, 231, 230],
        }
    } else {
        Palette {
            surface_solid: [28, 29, 34],
            line: [44, 45, 49],
            line_strong: [60, 61, 65],
            surface_3: [51, 52, 56],
            text: [255, 255, 255],
            text_2: [169, 169, 171],
            text_3: [119, 119, 122],
            accent: [59, 167, 255],
            accent_soft_alpha: 0.16,
            warn: [255, 179, 64],
            rec: [255, 69, 58],
            rec_soft: [69, 36, 38],
        }
    }
}

fn system_prefers_light() -> bool {
    let mut val: u32 = 0;
    let mut size: u32 = std::mem::size_of::<u32>() as u32;
    // SAFETY: reads a DWORD from HKCU Personalize; outputs point to stack variables with exact byte size.
    let err = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            w!(r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize"),
            w!("AppsUseLightTheme"),
            RRF_RT_REG_DWORD,
            None,
            Some((&mut val as *mut u32).cast::<c_void>()),
            Some(&mut size),
        )
    };
    err.is_ok() && val != 0
}

fn resolve_palette(theme: Theme) -> Palette {
    let light = match theme {
        Theme::Light => true,
        Theme::Dark => false,
        Theme::System => system_prefers_light(),
    };
    palette_for(light)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MenuAction {
    Save(Option<u32>),
    RecordStart,
    RecordStop,
    Arm,
    Disarm,
    OpenClips,
    OpenSettings,
    Quit,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct MenuItem {
    label: &'static str,
    hint: Option<String>,
    action: MenuAction,
    danger: bool,
    disabled: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct MenuModel {
    phase: Phase,
    headline: String,
    items: [MenuItem; ITEM_COUNT],
}

fn format_duration(secs: f64) -> String {
    let total = secs.max(0.0).floor() as u64;
    let mins = total / 60;
    let rem = total % 60;
    format!("{mins}:{rem:02}")
}

fn build_menu_model(st: &Status) -> MenuModel {
    let armed = st.phase == Phase::Armed;
    let busy = matches!(st.phase, Phase::Starting | Phase::Recovering);
    let recording = armed && st.recording;

    let headline = match st.phase {
        Phase::Armed if st.recording => format!("Recording \u{00b7} {}", format_duration(st.recording_secs)),
        Phase::Armed => format!(
            "Replay on \u{00b7} {:.0} of {} s",
            st.buffered_secs.min(f64::from(st.buffer_secs)),
            st.buffer_secs
        ),
        Phase::Starting => "Starting capture\u{2026}".into(),
        Phase::Recovering => "Restarting capture\u{2026}".into(),
        Phase::Error => "Capture stopped".into(),
        Phase::Idle => "Replay off".into(),
    };

    let item = |label, action, disabled| MenuItem {
        label,
        hint: None,
        action,
        danger: false,
        disabled,
    };

    let record_item = if recording {
        MenuItem {
            label: "Stop recording",
            hint: Some(format_duration(st.recording_secs)),
            action: MenuAction::RecordStop,
            danger: true,
            disabled: false,
        }
    } else {
        item("Start recording", MenuAction::RecordStart, !armed)
    };

    let toggle_item = if armed || busy {
        item("Turn replay off", MenuAction::Disarm, false)
    } else {
        item("Turn replay on", MenuAction::Arm, false)
    };

    MenuModel {
        phase: st.phase,
        headline,
        items: [
            item("Save last 15 s", MenuAction::Save(Some(15)), !armed),
            item("Save last 30 s", MenuAction::Save(Some(30)), !armed),
            item("Save last 60 s", MenuAction::Save(Some(60)), !armed),
            MenuItem {
                label: "Save whole buffer",
                hint: Some(format!("{} s", st.buffer_secs)),
                action: MenuAction::Save(None),
                danger: false,
                disabled: !armed,
            },
            record_item,
            toggle_item,
            item("Open clips folder", MenuAction::OpenClips, false),
            item("Settings", MenuAction::OpenSettings, false),
            MenuItem {
                label: "Quit SuiCap",
                hint: None,
                action: MenuAction::Quit,
                danger: true,
                disabled: false,
            },
        ],
    }
}

#[derive(Debug, Clone, Copy)]
struct MenuLayout {
    w: i32,
    h: i32,
    scale: f32,
    dot_cx: f32,
    dot_cy: f32,
    head_rect: RECT,
    sep_rects: [RECT; 3],
    item_rects: [RECT; ITEM_COUNT],
}

fn compute_layout(scale: f32) -> MenuLayout {
    let px = |logical: f32| -> i32 { (logical * scale).round() as i32 };
    let w = px(LOGICAL_W).max(120);
    let h = px(LOGICAL_H).max(120);
    let pad = LOGICAL_PAD;
    let item_left = px(pad);
    let item_right = w - px(pad);

    let head_top = pad;
    let head_rect = RECT {
        left: px(pad + 10.0 + 8.0 + 9.0),
        top: px(head_top),
        right: px(LOGICAL_W - pad - 10.0),
        bottom: px(head_top + LOGICAL_HEAD_H),
    };
    let dot_cx = (pad + 10.0 + 4.0) * scale;
    let dot_cy = (head_top + LOGICAL_HEAD_H * 0.5) * scale;

    let sep_thickness = (scale.round() as i32).max(1);
    let sep_left = px(pad + 8.0);
    let sep_right = w - px(pad + 8.0);

    let mut sep_rects = [RECT::default(); 3];
    let mut item_rects = [RECT::default(); ITEM_COUNT];
    let group_counts = [5usize, 3usize, 1usize];
    let mut y = head_top + LOGICAL_HEAD_H;
    let mut idx = 0usize;

    for (g, count) in group_counts.iter().enumerate() {
        let sep_top = px(y + (LOGICAL_SEP_H - 1.0) * 0.5);
        sep_rects[g] = RECT {
            left: sep_left,
            top: sep_top,
            right: sep_right,
            bottom: sep_top + sep_thickness,
        };
        y += LOGICAL_SEP_H;
        for _ in 0..*count {
            item_rects[idx] = RECT {
                left: item_left,
                top: px(y),
                right: item_right,
                bottom: px(y + LOGICAL_ITEM_H),
            };
            y += LOGICAL_ITEM_H;
            idx += 1;
        }
    }

    MenuLayout {
        w,
        h,
        scale,
        dot_cx,
        dot_cy,
        head_rect,
        sep_rects,
        item_rects,
    }
}

fn hit_item(layout: &MenuLayout, x: i32, y: i32) -> Option<usize> {
    layout
        .item_rects
        .iter()
        .position(|r| x >= r.left && x < r.right && y >= r.top && y < r.bottom)
}

fn step_index(items: &[MenuItem], current: isize, dir: isize) -> isize {
    let n = items.len() as isize;
    if n == 0 {
        return -1;
    }
    let mut i = if current < 0 && dir < 0 { 0 } else { current };
    for _ in 0..n {
        i = (i + dir).rem_euclid(n);
        if !items[i as usize].disabled {
            return i;
        }
    }
    -1
}

const fn colorref(rgb: [u8; 3]) -> COLORREF {
    COLORREF((rgb[0] as u32) | ((rgb[1] as u32) << 8) | ((rgb[2] as u32) << 16))
}

fn blend_rgb(bg: [u8; 3], fg: [u8; 3], alpha: f32) -> [u8; 3] {
    let inv = 1.0 - alpha;
    [
        (f32::from(bg[0]) * inv + f32::from(fg[0]) * alpha).round() as u8,
        (f32::from(bg[1]) * inv + f32::from(fg[1]) * alpha).round() as u8,
        (f32::from(bg[2]) * inv + f32::from(fg[2]) * alpha).round() as u8,
    ]
}

fn blend_pixel(dst: &mut [u8], col: [u8; 3], cov: f32) {
    let a = cov.clamp(0.0, 1.0);
    if a <= 0.0 {
        return;
    }
    let inv = 1.0 - a;
    dst[0] = (f32::from(dst[0]) * inv + f32::from(col[2]) * a).round() as u8;
    dst[1] = (f32::from(dst[1]) * inv + f32::from(col[1]) * a).round() as u8;
    dst[2] = (f32::from(dst[2]) * inv + f32::from(col[0]) * a).round() as u8;
    dst[3] = 255;
}

fn rounded_box_sd(px: f32, py: f32, cx: f32, cy: f32, hw: f32, hh: f32, r: f32) -> f32 {
    let qx = (px - cx).abs() - (hw - r);
    let qy = (py - cy).abs() - (hh - r);
    qx.max(0.0).hypot(qy.max(0.0)) + qx.min(0.0).max(qy.min(0.0)) - r
}

fn fill_rounded_rect(bits: &mut [u8], w: i32, h: i32, r: RECT, radius: f32, col: [u8; 3]) {
    let x0 = r.left.max(0);
    let y0 = r.top.max(0);
    let x1 = r.right.min(w);
    let y1 = r.bottom.min(h);
    if x1 <= x0 || y1 <= y0 {
        return;
    }
    let cx = (r.left + r.right) as f32 * 0.5;
    let cy = (r.top + r.bottom) as f32 * 0.5;
    let hw = (r.right - r.left) as f32 * 0.5;
    let hh = (r.bottom - r.top) as f32 * 0.5;
    let rad = radius.min(hw).min(hh);
    for y in y0..y1 {
        let py = y as f32 + 0.5;
        let row = (y * w) as usize;
        for x in x0..x1 {
            let d = rounded_box_sd(x as f32 + 0.5, py, cx, cy, hw, hh, rad);
            let cov = (0.5 - d).clamp(0.0, 1.0);
            if cov > 0.0 {
                let idx = (row + x as usize) * 4;
                blend_pixel(&mut bits[idx..idx + 4], col, cov);
            }
        }
    }
}

fn fill_circle(bits: &mut [u8], w: i32, h: i32, center: (f32, f32), radius: f32, col: [u8; 3], opacity: f32) {
    if opacity <= 0.0 || radius <= 0.0 {
        return;
    }
    let (cx, cy) = center;
    let x0 = (cx - radius - 1.0).floor().max(0.0) as i32;
    let y0 = (cy - radius - 1.0).floor().max(0.0) as i32;
    let x1 = (cx + radius + 1.0).ceil().min(w as f32) as i32;
    let y1 = (cy + radius + 1.0).ceil().min(h as f32) as i32;
    for y in y0..y1 {
        let dy = (y as f32 + 0.5) - cy;
        let row = (y * w) as usize;
        for x in x0..x1 {
            let dx = (x as f32 + 0.5) - cx;
            let d = dx.hypot(dy) - radius;
            let cov = (0.5 - d).clamp(0.0, 1.0) * opacity;
            if cov > 0.0 {
                let idx = (row + x as usize) * 4;
                blend_pixel(&mut bits[idx..idx + 4], col, cov);
            }
        }
    }
}

fn apply_card_mask_and_border(bits: &mut [u8], w: i32, h: i32, radius: f32, bw: f32, border_col: [u8; 3]) {
    let hw = w as f32 * 0.5;
    let hh = h as f32 * 0.5;
    let rad = radius.min(hw).min(hh);
    let edge_threshold = -bw - 0.5;
    for y in 0..h {
        let py = y as f32 + 0.5;
        let row = (y * w) as usize;
        for x in 0..w {
            let d = rounded_box_sd(x as f32 + 0.5, py, hw, hh, hw, hh, rad);
            let idx = (row + x as usize) * 4;
            if d <= edge_threshold {
                // Restore full opacity while keeping GDI ClearType subpixel RGB intact.
                bits[idx + 3] = 255;
            } else {
                let cov_outer = (0.5 - d).clamp(0.0, 1.0);
                if cov_outer <= 0.0 {
                    bits[idx..idx + 4].fill(0);
                } else {
                    let cov_inner = (d + bw + 0.5).clamp(0.0, 1.0);
                    let inv_b = 1.0 - cov_inner;
                    let b = f32::from(bits[idx]) * inv_b + f32::from(border_col[2]) * cov_inner;
                    let g = f32::from(bits[idx + 1]) * inv_b + f32::from(border_col[1]) * cov_inner;
                    let r = f32::from(bits[idx + 2]) * inv_b + f32::from(border_col[0]) * cov_inner;
                    bits[idx] = (b * cov_outer).round() as u8;
                    bits[idx + 1] = (g * cov_outer).round() as u8;
                    bits[idx + 2] = (r * cov_outer).round() as u8;
                    bits[idx + 3] = (cov_outer * 255.0).round() as u8;
                }
            }
        }
    }
}

struct LayeredCanvas {
    w: i32,
    h: i32,
    mem_dc: HDC,
    bmp: HBITMAP,
    old_obj: HGDIOBJ,
    bits: *mut u8,
    font_head: HFONT,
    font_item: HFONT,
    font_hint: HFONT,
}

fn create_ui_font(logical_px: f32, scale: f32, weight: i32) -> HFONT {
    let height = -((logical_px * scale).round() as i32);
    // SAFETY: static face name literal + valid GDI font attributes.
    unsafe {
        CreateFontW(
            height,
            0,
            0,
            0,
            weight,
            0,
            0,
            0,
            DEFAULT_CHARSET,
            OUT_DEFAULT_PRECIS,
            CLIP_DEFAULT_PRECIS,
            CLEARTYPE_QUALITY,
            (DEFAULT_PITCH.0 | FF_SWISS.0) as u32,
            w!("Segoe UI Variable Text"),
        )
    }
}

impl LayeredCanvas {
    fn new(w: i32, h: i32, scale: f32) -> Option<Self> {
        let bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w,
                biHeight: -h, // top-down DIB
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits: *mut c_void = std::ptr::null_mut();
        // SAFETY: creates an offscreen memory DC + 32-bit DIB section owned by `Self`.
        unsafe {
            let mem_dc = CreateCompatibleDC(None);
            if mem_dc.is_invalid() {
                return None;
            }
            let bmp = match CreateDIBSection(Some(mem_dc), &bmi, DIB_RGB_COLORS, &mut bits, None, 0) {
                Ok(b) if !bits.is_null() => b,
                _ => {
                    let _ = DeleteDC(mem_dc);
                    return None;
                }
            };
            let old_obj = SelectObject(mem_dc, HGDIOBJ(bmp.0));
            let _ = SetBkMode(mem_dc, TRANSPARENT);
            Some(Self {
                w,
                h,
                mem_dc,
                bmp,
                old_obj,
                bits: bits.cast::<u8>(),
                font_head: create_ui_font(12.0, scale, 550),
                font_item: create_ui_font(13.0, scale, 500),
                font_hint: create_ui_font(12.0, scale, 400),
            })
        }
    }

    fn bits_mut(&mut self) -> &mut [u8] {
        let len = (self.w as usize) * (self.h as usize) * 4;
        // SAFETY: `self.bits` points to `w * h * 4` bytes valid for the lifetime of `self.bmp`.
        unsafe { std::slice::from_raw_parts_mut(self.bits, len) }
    }

    fn draw_text(&self, font: HFONT, col: [u8; 3], text: &str, rect: RECT, right_align: bool) {
        let mut wide: Vec<u16> = text.encode_utf16().collect();
        if wide.is_empty() {
            return;
        }
        let mut r = rect;
        let align = if right_align { DT_RIGHT } else { DT_LEFT };
        // SAFETY: `self.mem_dc` has a valid selected DIB section; `wide` and `r` remain valid during call.
        unsafe {
            let prev = SelectObject(self.mem_dc, HGDIOBJ(font.0));
            SetTextColor(self.mem_dc, colorref(col));
            DrawTextW(
                self.mem_dc,
                &mut wide,
                &mut r,
                DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX | align,
            );
            SelectObject(self.mem_dc, prev);
        }
    }
}

impl Drop for LayeredCanvas {
    fn drop(&mut self) {
        // SAFETY: releases all GDI handles allocated in `LayeredCanvas::new`.
        unsafe {
            SelectObject(self.mem_dc, self.old_obj);
            let _ = DeleteObject(HGDIOBJ(self.font_head.0));
            let _ = DeleteObject(HGDIOBJ(self.font_item.0));
            let _ = DeleteObject(HGDIOBJ(self.font_hint.0));
            let _ = DeleteObject(HGDIOBJ(self.bmp.0));
            let _ = DeleteDC(self.mem_dc);
        }
    }
}

struct MenuWindowState {
    app: AppHandle,
    hwnd: HWND,
    pos: (i32, i32),
    layout: MenuLayout,
    canvas: LayeredCanvas,
    palette: Palette,
    model: MenuModel,
    active: isize,
    pressed: bool,
    mouse_tracked: bool,
    saw_active: bool,
    was_l_down: bool,
    was_r_down: bool,
    reduced_motion: bool,
    open_at: Instant,
    open_done: bool,
    chosen: Option<MenuAction>,
}

fn reduced_motion_enabled() -> bool {
    let mut anim: i32 = 1;
    // SAFETY: queries `SPI_GETCLIENTAREAANIMATION` into an `i32` (`BOOL`).
    let ok = unsafe {
        SystemParametersInfoW(
            SPI_GETCLIENTAREAANIMATION,
            0,
            Some((&mut anim as *mut i32).cast::<c_void>()),
            Default::default(),
        )
    };
    ok.is_ok() && anim == 0
}

fn refresh_snapshot(st: &mut MenuWindowState) {
    let app_state = st.app.state::<AppState>();
    let settings = app_state.settings();
    let status = app_state.status();
    st.palette = resolve_palette(settings.theme);
    st.model = build_menu_model(&status);
    if st.active >= 0 && st.model.items[st.active as usize].disabled {
        st.active = -1;
        st.pressed = false;
    }
}

fn render_and_present(st: &mut MenuWindowState) {
    let p = st.palette;
    let layout = st.layout;
    let scale = layout.scale;
    let (w, h) = (layout.w, layout.h);

    let open_alpha: u8 = if st.reduced_motion || st.open_done {
        255
    } else {
        let t = (st.open_at.elapsed().as_secs_f32() / 0.14).clamp(0.0, 1.0);
        if t >= 1.0 {
            st.open_done = true;
            255
        } else {
            let eased = 1.0 - (1.0 - t).powi(3);
            (eased * 255.0).round().clamp(0.0, 255.0) as u8
        }
    };

    let bg_px = [p.surface_solid[2], p.surface_solid[1], p.surface_solid[0], 255u8];
    {
        let bits = st.canvas.bits_mut();
        for px in bits.chunks_exact_mut(4) {
            px.copy_from_slice(&bg_px);
        }

        // Status dot & pulse halo
        let dot_r = 4.0 * scale;
        let dot_center = (layout.dot_cx, layout.dot_cy);
        match st.model.phase {
            Phase::Armed => {
                if !st.reduced_motion {
                    let phase = (st.open_at.elapsed().as_secs_f32() % 2.4) / 2.4;
                    if phase < 0.7 {
                        let t = phase / 0.7;
                        let eased = 1.0 - (1.0 - t).powi(2);
                        let halo_r = dot_r + 6.0 * scale * eased;
                        let halo_a = p.accent_soft_alpha * (1.0 - eased);
                        fill_circle(bits, w, h, dot_center, halo_r, p.accent, halo_a);
                    }
                }
                fill_circle(bits, w, h, dot_center, dot_r, p.accent, 1.0);
            }
            Phase::Error => fill_circle(bits, w, h, dot_center, dot_r, p.warn, 1.0),
            _ => fill_circle(bits, w, h, dot_center, dot_r, p.text_3, 1.0),
        }

        // Group dividers
        for sep in &layout.sep_rects {
            for y in sep.top.max(0)..sep.bottom.min(h) {
                let row = (y * w) as usize;
                for x in sep.left.max(0)..sep.right.min(w) {
                    let idx = (row + x as usize) * 4;
                    blend_pixel(&mut bits[idx..idx + 4], p.line, 1.0);
                }
            }
        }

        // Active / pressed item highlight pill
        let mut active_pill_bg: Option<[u8; 3]> = None;
        if st.active >= 0 {
            let idx = st.active as usize;
            let item = &st.model.items[idx];
            if !item.disabled {
                let mut r = layout.item_rects[idx];
                if st.pressed {
                    let inset = (1.5 * scale).round() as i32;
                    r.left += inset;
                    r.right -= inset;
                    r.top += (0.5 * scale).round() as i32;
                    r.bottom -= (0.5 * scale).round() as i32;
                }
                let col = if item.danger {
                    p.rec_soft
                } else if st.pressed {
                    p.line_strong
                } else {
                    p.surface_3
                };
                active_pill_bg = Some(col);
                fill_rounded_rect(bits, w, h, r, LOGICAL_ITEM_R * scale, col);
            }
        }

        // Text pass (GDI ClearType)
        let pad_x = (10.0 * scale).round() as i32;
        st.canvas.draw_text(
            st.canvas.font_head,
            p.text_2,
            &st.model.headline,
            layout.head_rect,
            false,
        );

        for (i, item) in st.model.items.iter().enumerate() {
            let r = RECT {
                left: layout.item_rects[i].left + pad_x,
                top: layout.item_rects[i].top,
                right: layout.item_rects[i].right - pad_x,
                bottom: layout.item_rects[i].bottom,
            };
            let label_col = if item.disabled {
                p.text_3
            } else if item.danger {
                p.rec
            } else {
                p.text
            };
            st.canvas
                .draw_text(st.canvas.font_item, label_col, item.label, r, false);
            if let Some(hint) = &item.hint {
                let hint_col = if st.active == i as isize {
                    let bg = active_pill_bg.unwrap_or(p.surface_solid);
                    let fg = if p.surface_solid[0] > 128 {
                        [20, 22, 26]
                    } else {
                        [255, 255, 255]
                    };
                    let a = if p.surface_solid[0] > 128 { 0.42 } else { 0.40 };
                    blend_rgb(bg, fg, a)
                } else {
                    p.text_3
                };
                st.canvas.draw_text(st.canvas.font_hint, hint_col, hint, r, true);
            }
        }
    }

    // SAFETY: flushes GDI text writes before inspecting pixels for outer rounded corners/border.
    unsafe {
        let _ = GdiFlush();
    }

    let bits = st.canvas.bits_mut();
    apply_card_mask_and_border(bits, w, h, LOGICAL_CARD_R * scale, scale.max(1.0), p.line_strong);

    let pt_dst = POINT {
        x: st.pos.0,
        y: st.pos.1,
    };
    let sz = SIZE { cx: w, cy: h };
    let pt_src = POINT { x: 0, y: 0 };
    let blend = BLENDFUNCTION {
        BlendOp: AC_SRC_OVER as u8,
        BlendFlags: 0,
        SourceConstantAlpha: open_alpha,
        AlphaFormat: AC_SRC_ALPHA as u8,
    };
    // SAFETY: `st.hwnd` and `st.canvas.mem_dc` are valid on this owning thread.
    unsafe {
        let _ = UpdateLayeredWindow(
            st.hwnd,
            None,
            Some(&pt_dst),
            Some(&sz),
            Some(st.canvas.mem_dc),
            Some(&pt_src),
            COLORREF(0),
            Some(&blend),
            ULW_ALPHA,
        );
    }
}

fn dismiss_on_blur(hwnd: HWND) {
    *lock(&LAST_MENU_CLOSE) = Some(Instant::now());
    // SAFETY: `hwnd` is owned by the calling menu thread.
    unsafe {
        let _ = DestroyWindow(hwnd);
    }
}

fn menu_class_name() -> Option<()> {
    static REGISTERED: OnceLock<bool> = OnceLock::new();
    let ok = *REGISTERED.get_or_init(|| {
        // SAFETY: registers a process-local window class once with our WndProc and arrow cursor.
        unsafe {
            let hcur = LoadCursorW(None, IDC_ARROW).unwrap_or_default();
            let wc = WNDCLASSEXW {
                cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
                lpfnWndProc: Some(menu_wnd_proc),
                hCursor: hcur,
                lpszClassName: w!("SuiCapTrayMenu"),
                ..Default::default()
            };
            RegisterClassExW(&wc) != 0
        }
    });
    ok.then_some(())
}

unsafe extern "system" fn menu_wnd_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if msg == WM_NCHITTEST {
        return LRESULT(HTCLIENT as isize);
    }
    if msg == WM_SETCURSOR {
        // SAFETY: loads the standard arrow cursor handle (shared system cursor).
        unsafe {
            if let Ok(c) = LoadCursorW(None, IDC_ARROW) {
                SetCursor(Some(c));
            }
        }
        return LRESULT(1);
    }

    // SAFETY: `GWLP_USERDATA` points to a heap `MenuWindowState` valid until `WM_DESTROY` finishes.
    let ptr = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *mut MenuWindowState;
    if ptr.is_null() {
        // SAFETY: forward unhandled messages before state is attached or after `WM_DESTROY`.
        return unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) };
    }
    // SAFETY: only accessed from this owning window's message loop thread cleanly per message.
    let st = unsafe { &mut *ptr };

    match msg {
        WM_MOUSEMOVE => {
            if !st.mouse_tracked {
                let mut tme = TRACKMOUSEEVENT {
                    cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                    dwFlags: TME_LEAVE,
                    hwndTrack: hwnd,
                    dwHoverTime: 0,
                };
                // SAFETY: `tme` describes `hwnd`.
                st.mouse_tracked = unsafe { TrackMouseEvent(&mut tme).is_ok() };
            }
            let x = (lparam.0 & 0xFFFF) as i16 as i32;
            let y = ((lparam.0 >> 16) & 0xFFFF) as i16 as i32;
            let next = hit_item(&st.layout, x, y)
                .filter(|&i| !st.model.items[i].disabled)
                .map_or(-1, |i| i as isize);
            if next != st.active {
                st.active = next;
                render_and_present(st);
            }
            LRESULT(0)
        }
        WM_MOUSELEAVE => {
            st.mouse_tracked = false;
            if st.active != -1 || st.pressed {
                st.active = -1;
                st.pressed = false;
                render_and_present(st);
            }
            LRESULT(0)
        }
        WM_LBUTTONDOWN => {
            let x = (lparam.0 & 0xFFFF) as i16 as i32;
            let y = ((lparam.0 >> 16) & 0xFFFF) as i16 as i32;
            if let Some(i) = hit_item(&st.layout, x, y).filter(|&i| !st.model.items[i].disabled) {
                st.active = i as isize;
                st.pressed = true;
                render_and_present(st);
            }
            LRESULT(0)
        }
        WM_LBUTTONUP => {
            let x = (lparam.0 & 0xFFFF) as i16 as i32;
            let y = ((lparam.0 >> 16) & 0xFFFF) as i16 as i32;
            let was_pressed = st.pressed;
            st.pressed = false;
            if was_pressed
                && let Some(i) = hit_item(&st.layout, x, y)
                && i as isize == st.active
                && !st.model.items[i].disabled
            {
                st.chosen = Some(st.model.items[i].action);
                // SAFETY: closes the menu window cleanly after selecting an action.
                unsafe {
                    let _ = DestroyWindow(hwnd);
                }
            } else if was_pressed {
                render_and_present(st);
            }
            LRESULT(0)
        }
        WM_KEYDOWN => {
            let key = wparam.0 as u16;
            if key == VK_ESCAPE.0 {
                // SAFETY: closes the menu window cleanly on Escape.
                unsafe {
                    let _ = DestroyWindow(hwnd);
                }
            } else if key == VK_DOWN.0 {
                st.active = step_index(&st.model.items, st.active, 1);
                render_and_present(st);
            } else if key == VK_UP.0 {
                st.active = step_index(&st.model.items, st.active, -1);
                render_and_present(st);
            } else if (key == VK_RETURN.0 || key == VK_SPACE.0) && st.active >= 0 {
                let item = &st.model.items[st.active as usize];
                if !item.disabled {
                    st.chosen = Some(item.action);
                    // SAFETY: closes the menu window cleanly after activating an item.
                    unsafe {
                        let _ = DestroyWindow(hwnd);
                    }
                }
            }
            LRESULT(0)
        }
        WM_ACTIVATE => {
            let active = (wparam.0 & 0xFFFF) as u32 != WA_INACTIVE;
            if active {
                st.saw_active = true;
            } else if st.saw_active || st.open_at.elapsed() >= Duration::from_millis(80) {
                dismiss_on_blur(hwnd);
            }
            LRESULT(0)
        }
        WM_KILLFOCUS => {
            if st.saw_active || st.open_at.elapsed() >= Duration::from_millis(80) {
                dismiss_on_blur(hwnd);
            }
            LRESULT(0)
        }
        WM_SETTINGCHANGE | WM_APP_REFRESH => {
            refresh_snapshot(st);
            render_and_present(st);
            LRESULT(0)
        }
        WM_TIMER => {
            // Check outside mouse clicks and foreground loss (covers Win11 tray overflow flyout).
            if st.open_at.elapsed() >= Duration::from_millis(60) {
                // SAFETY: queries global mouse button state, cursor position, and window rect.
                let (l_down, r_down, pt, rc) = unsafe {
                    let l = (GetAsyncKeyState(i32::from(VK_LBUTTON.0)) as u16 & 0x8000) != 0;
                    let r = (GetAsyncKeyState(i32::from(VK_RBUTTON.0)) as u16 & 0x8000) != 0;
                    let mut pt = POINT::default();
                    let _ = GetCursorPos(&mut pt);
                    let mut rc = RECT::default();
                    let _ = GetWindowRect(hwnd, &mut rc);
                    (l, r, pt, rc)
                };
                let clicked = (l_down && !st.was_l_down) || (r_down && !st.was_r_down);
                st.was_l_down = l_down;
                st.was_r_down = r_down;
                let inside_win = pt.x >= rc.left && pt.x < rc.right && pt.y >= rc.top && pt.y < rc.bottom;
                if clicked && !inside_win {
                    dismiss_on_blur(hwnd);
                    return LRESULT(0);
                }
            }
            // SAFETY: checks current foreground window handle.
            let fg = unsafe { GetForegroundWindow() };
            if (st.saw_active || st.open_at.elapsed() >= Duration::from_millis(180)) && fg != hwnd {
                dismiss_on_blur(hwnd);
                return LRESULT(0);
            }
            let fading = !st.reduced_motion && !st.open_done;
            let pulsing = !st.reduced_motion && st.model.phase == Phase::Armed;
            if fading || pulsing {
                render_and_present(st);
            }
            LRESULT(0)
        }
        WM_CLOSE => {
            // SAFETY: closes the window cleanly when toggled off from the tray icon.
            unsafe {
                let _ = DestroyWindow(hwnd);
            }
            LRESULT(0)
        }
        WM_DESTROY => {
            MENU_HWND.store(0, Ordering::Release);
            // SAFETY: clears user data pointer, kills timer, and exits the menu message loop.
            unsafe {
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                let _ = KillTimer(Some(hwnd), TIMER_ID);
                PostQuitMessage(0);
            }
            LRESULT(0)
        }
        // SAFETY: standard Win32 default handling for remaining messages.
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

fn execute_action(app: &AppHandle, action: MenuAction) {
    match action {
        MenuAction::Save(secs) => app.state::<AppState>().save(app, secs),
        MenuAction::RecordStart => {
            let _ = app.state::<AppState>().record_start(app);
        }
        MenuAction::RecordStop => {
            let _ = app.state::<AppState>().record_stop();
        }
        MenuAction::Arm => app.state::<AppState>().arm(app),
        MenuAction::Disarm => app.state::<AppState>().disarm(app),
        MenuAction::OpenClips => {
            if let Err(e) = crate::commands::open_clips_dir(&app.state::<AppState>()) {
                crate::feedback::notify(app, "Could not open clips folder", &e);
            }
        }
        MenuAction::OpenSettings => open_settings(app),
        MenuAction::Quit => {
            shutdown(app);
            app.exit(0);
        }
    }
}

struct MenuOpenGuard;
impl Drop for MenuOpenGuard {
    fn drop(&mut self) {
        MENU_HWND.store(0, Ordering::Release);
        MENU_OPEN.store(false, Ordering::Release);
    }
}

fn monitor_at_point(app: &AppHandle, cx: i32, cy: i32) -> tauri::Result<tauri::Monitor> {
    let monitors = app.available_monitors()?;
    let hit = monitors
        .iter()
        .find(|m| {
            let p = m.position();
            let s = m.size();
            cx >= p.x && cx < p.x + s.width as i32 && cy >= p.y && cy < p.y + s.height as i32
        })
        .cloned()
        .or_else(|| {
            monitors.into_iter().min_by_key(|m| {
                let p = m.position();
                let s = m.size();
                let nx = cx.clamp(p.x, p.x + s.width as i32);
                let ny = cy.clamp(p.y, p.y + s.height as i32);
                i64::from(cx - nx).pow(2) + i64::from(cy - ny).pow(2)
            })
        });
    hit.map_or_else(|| app.primary_monitor()?.ok_or(tauri::Error::WindowNotFound), Ok)
}

fn open_menu(app: &AppHandle, rect: Rect, cursor: PhysicalPosition<f64>) {
    let raw = MENU_HWND.load(Ordering::Acquire);
    if raw != 0 {
        // SAFETY: `raw` is a live HWND cleared in `WM_DESTROY`.
        unsafe {
            let _ = PostMessageW(Some(HWND(raw as *mut c_void)), WM_CLOSE, WPARAM(0), LPARAM(0));
        }
        return;
    }
    if lock(&LAST_MENU_CLOSE).is_some_and(|t| t.elapsed() < Duration::from_millis(250)) {
        return; // this click closed it via blur
    }
    if MENU_OPEN
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return;
    }
    let pos = rect.position.to_physical::<f64>(1.0);
    let size = rect.size.to_physical::<f64>(1.0);
    let anchor = Anchor {
        icon: Px {
            x: pos.x as i32,
            y: pos.y as i32,
            w: size.width as i32,
            h: size.height as i32,
        },
        cursor: (cursor.x as i32, cursor.y as i32),
    };
    let app = app.clone();
    std::thread::spawn(move || {
        let guard = MenuOpenGuard;
        let chosen = run_native_menu(&app, anchor);
        drop(guard);
        if let Some(action) = chosen {
            execute_action(&app, action);
        }
    });
}

fn run_native_menu(app: &AppHandle, anchor: Anchor) -> Option<MenuAction> {
    menu_class_name()?;
    let (cx, cy) = anchor.cursor;
    let monitor = monitor_at_point(app, cx, cy).ok()?;
    let scale = monitor.scale_factor() as f32;
    let m = Px {
        x: monitor.position().x,
        y: monitor.position().y,
        w: monitor.size().width as i32,
        h: monitor.size().height as i32,
    };
    let wa = monitor.work_area();
    let work = Px {
        x: wa.position.x,
        y: wa.position.y,
        w: wa.size.width as i32,
        h: wa.size.height as i32,
    };
    let icon = if anchor.icon.w > 0 && anchor.icon.h > 0 {
        anchor.icon
    } else {
        Px {
            x: cx,
            y: cy,
            w: 1,
            h: 1,
        }
    };
    let layout = compute_layout(scale);
    let pos = place_menu(icon, m, work, (layout.w, layout.h));
    let canvas = LayeredCanvas::new(layout.w, layout.h, scale)?;

    let app_state = app.state::<AppState>();
    let settings = app_state.settings();
    let status = app_state.status();

    let mut boxed = Box::new(MenuWindowState {
        app: app.clone(),
        hwnd: HWND::default(),
        pos,
        layout,
        canvas,
        palette: resolve_palette(settings.theme),
        model: build_menu_model(&status),
        active: -1,
        pressed: false,
        mouse_tracked: false,
        saw_active: false,
        was_l_down: false,
        was_r_down: false,
        reduced_motion: reduced_motion_enabled(),
        open_at: Instant::now(),
        open_done: false,
        chosen: None,
    });

    // SAFETY: creates a hidden layered popup window owned by this thread.
    let hwnd = unsafe {
        CreateWindowExW(
            WS_EX_LAYERED | WS_EX_TOOLWINDOW | WS_EX_TOPMOST,
            w!("SuiCapTrayMenu"),
            w!("SuiCap menu"),
            WS_POPUP,
            pos.0,
            pos.1,
            layout.w,
            layout.h,
            None,
            None,
            None::<HINSTANCE>,
            None,
        )
    }
    .ok()?;

    boxed.hwnd = hwnd;
    MENU_HWND.store(hwnd.0 as isize, Ordering::Release);
    let state_ptr = Box::into_raw(boxed);
    // SAFETY: attaches `state_ptr` to `hwnd` before showing or entering the message loop.
    unsafe {
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, state_ptr as isize);
        render_and_present(&mut *state_ptr);
        let _ = ShowWindow(hwnd, SW_SHOW);
        let _ = SetForegroundWindow(hwnd);
        let _ = SetTimer(Some(hwnd), TIMER_ID, 25, None);

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        let state = Box::from_raw(state_ptr);
        state.chosen
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MON: Px = Px {
        x: 0,
        y: 0,
        w: 1920,
        h: 1080,
    };
    const MENU_SIZE: (i32, i32) = (260, 300);

    fn overlaps(a: Px, b: Px) -> bool {
        a.x < b.right() && b.x < a.right() && a.y < b.bottom() && b.y < a.bottom()
    }

    fn check(icon: Px, work: Px) -> Px {
        let (x, y) = place_menu(icon, MON, work, MENU_SIZE);
        let menu = Px {
            x,
            y,
            w: MENU_SIZE.0,
            h: MENU_SIZE.1,
        };
        assert!(!overlaps(menu, icon), "menu {menu:?} covers icon {icon:?}");
        assert!(
            menu.x >= work.x && menu.right() <= work.right(),
            "{menu:?} outside {work:?}"
        );
        assert!(
            menu.y >= work.y && menu.bottom() <= work.bottom(),
            "{menu:?} outside {work:?}"
        );
        menu
    }

    #[test]
    fn bottom_taskbar_opens_above_icon() {
        let icon = Px {
            x: 1800,
            y: 1044,
            w: 24,
            h: 24,
        };
        let m = check(icon, Px { h: 1032, ..MON });
        assert_eq!(m.bottom(), 1032 - GAP);
        assert_eq!(m.right(), 1920); // clamped at the right screen edge
    }

    #[test]
    fn top_left_right_taskbars() {
        let top = check(
            Px {
                x: 1700,
                y: 8,
                w: 24,
                h: 24,
            },
            Px { y: 48, h: 1032, ..MON },
        );
        assert!(top.y >= 48 + GAP);
        let left = check(
            Px {
                x: 12,
                y: 1000,
                w: 24,
                h: 24,
            },
            Px { x: 48, w: 1872, ..MON },
        );
        assert!(left.x >= 48 + GAP);
        let right = check(
            Px {
                x: 1884,
                y: 1000,
                w: 24,
                h: 24,
            },
            Px { w: 1872, ..MON },
        );
        assert!(right.right() <= 1872 - GAP);
    }

    #[test]
    fn auto_hide_taskbar_uses_nearest_edge() {
        let icon = Px {
            x: 1800,
            y: 1050,
            w: 24,
            h: 24,
        };
        let m = check(icon, MON);
        assert!(m.bottom() <= icon.y - GAP);
    }

    #[test]
    fn second_monitor_with_offset_origin() {
        let mon = Px {
            x: -2560,
            y: -200,
            w: 2560,
            h: 1440,
        };
        let work = Px { h: 1392, ..mon };
        let icon = Px {
            x: -40,
            y: 1200,
            w: 32,
            h: 32,
        };
        let (x, y) = place_menu(icon, mon, work, MENU_SIZE);
        assert!(x + MENU_SIZE.0 <= 0 && x >= -2560);
        assert!(y + MENU_SIZE.1 <= 1192 - GAP + 1);
    }

    #[test]
    fn palette_matches_theme_tokens() {
        let dark = palette_for(false);
        let light = palette_for(true);
        assert_eq!(dark.surface_solid, [28, 29, 34]);
        assert_eq!(dark.accent, [59, 167, 255]);
        assert_eq!(dark.rec, [255, 69, 58]);
        assert_eq!(light.surface_solid, [255, 255, 255]);
        assert_eq!(light.text, [20, 22, 26]);
        assert_eq!(light.accent, [27, 143, 240]);
        assert_eq!(light.rec, [229, 54, 43]);
    }

    #[test]
    fn menu_model_tracks_phase_and_recording() {
        let idle = build_menu_model(&Status {
            phase: Phase::Idle,
            buffer_secs: 60,
            ..Default::default()
        });
        assert_eq!(idle.headline, "Replay off");
        assert!(idle.items[0].disabled); // Save last 15 s
        assert!(idle.items[4].disabled); // Start recording
        assert_eq!(idle.items[5].label, "Turn replay on");
        assert!(!idle.items[5].disabled);
        assert!(idle.items[8].danger);

        let rec = build_menu_model(&Status {
            phase: Phase::Armed,
            buffer_secs: 90,
            buffered_secs: 42.2,
            recording: true,
            recording_secs: 75.9,
            ..Default::default()
        });
        assert_eq!(rec.headline, "Recording \u{00b7} 1:15");
        assert!(!rec.items[0].disabled);
        assert_eq!(rec.items[3].hint.as_deref(), Some("90 s"));
        assert_eq!(rec.items[4].label, "Stop recording");
        assert_eq!(rec.items[4].hint.as_deref(), Some("1:15"));
        assert!(rec.items[4].danger);
        assert!(!rec.items[4].disabled);
        assert_eq!(rec.items[5].label, "Turn replay off");
    }

    #[test]
    fn layout_and_keyboard_navigation_skip_disabled() {
        let layout = compute_layout(1.5);
        assert_eq!(layout.w, 378);
        assert_eq!(layout.h, 561);
        for pair in layout.item_rects.windows(2) {
            assert!(pair[0].bottom <= pair[1].top);
            assert!(pair[0].left >= 0 && pair[0].right <= layout.w);
        }
        assert!(layout.item_rects.last().unwrap().bottom <= layout.h);

        let idle = build_menu_model(&Status {
            phase: Phase::Idle,
            ..Default::default()
        });
        assert_eq!(step_index(&idle.items, -1, 1), 5); // skips 0..4 disabled save/record items
        assert_eq!(step_index(&idle.items, -1, -1), 8); // Quit SuiCap
        assert_eq!(step_index(&idle.items, 8, 1), 5); // wraps back to Turn replay on
    }
}
