//! Tray icon and the windows it opens (plan §6.3; Tauri workflow §5).
//!
//! RAM budget (plan §3): the settings window and the styled tray menu are created on
//! demand and destroyed on close/blur, never hidden, so WebView2 unloads while the app
//! sits in the tray.

use std::{
    sync::Mutex,
    time::{Duration, Instant},
};

use tauri::{
    AppHandle, LogicalSize, Manager, PhysicalPosition, Rect, WebviewUrl, WebviewWindow, WebviewWindowBuilder,
    WindowEvent,
    image::Image,
    tray::{MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent},
};

use crate::state::{Phase, Status};

pub const SETTINGS: &str = "settings";
pub const MENU: &str = "menu";
const TRAY_ID: &str = "main";
/// Gap between the menu and the tray icon / taskbar.
const GAP: i32 = 6;

/// Tray icon rectangle and cursor of the last right-click, in physical pixels.
#[derive(Debug, Clone, Copy)]
struct Anchor {
    icon: Px,
    cursor: (i32, i32),
}

static ANCHOR: Mutex<Option<Anchor>> = Mutex::new(None);
/// When the menu last closed on blur: a right-click on the icon while it is open
/// first blurs it, and that click must not reopen it.
static LAST_MENU_CLOSE: Mutex<Option<Instant>> = Mutex::new(None);
static SHOWN_PHASE: Mutex<Option<(Phase, String)>> = Mutex::new(None);

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

/// Update icon and tooltip when the phase changes (status ticks every second).
pub fn refresh(app: &AppHandle, st: &Status) {
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
            .inner_size(1280.0, 780.0)
            .min_inner_size(1280.0, 640.0)
            .decorations(false) // the page draws its own title bar (ui/TitleBar.svelte)
            .disable_drag_drop_handler() // the audio page uses HTML5 drag and drop
            .center()
            .visible(false) // the page shows it once styled, so no white flash
            .build();
        if let Err(e) = built {
            eprintln!("settings window: {e}");
        }
    });
}

fn open_menu(app: &AppHandle, rect: Rect, cursor: PhysicalPosition<f64>) {
    if let Some(w) = app.get_webview_window(MENU) {
        let _ = w.close();
        return;
    }
    if lock(&LAST_MENU_CLOSE).is_some_and(|t| t.elapsed() < Duration::from_millis(250)) {
        return; // this click closed it via blur
    }
    let pos = rect.position.to_physical::<f64>(1.0);
    let size = rect.size.to_physical::<f64>(1.0);
    *lock(&ANCHOR) = Some(Anchor {
        icon: Px {
            x: pos.x as i32,
            y: pos.y as i32,
            w: size.width as i32,
            h: size.height as i32,
        },
        cursor: (cursor.x as i32, cursor.y as i32),
    });
    // Off the event-loop thread, like the settings window: the first webview after all
    // others were closed needs the loop free to start WebView2.
    let app = app.clone();
    std::thread::spawn(move || {
        let built = WebviewWindowBuilder::new(&app, MENU, WebviewUrl::App("index.html".into()))
            .title("SuiCap menu")
            .inner_size(260.0, 360.0)
            .position(-32000.0, -32000.0)
            .decorations(false)
            .transparent(true)
            .shadow(false)
            .resizable(false)
            .always_on_top(true)
            .skip_taskbar(true)
            .visible(false)
            .build();
        match built {
            Ok(w) => {
                let handle = w.clone();
                w.on_window_event(move |e| {
                    // Ignore focus changes while still hidden (creation, the tray overflow
                    // flyout taking focus back); only a shown menu closes on blur.
                    if let WindowEvent::Focused(false) = e
                        && handle.is_visible().unwrap_or(false)
                    {
                        *lock(&LAST_MENU_CLOSE) = Some(Instant::now());
                        let _ = handle.close();
                    }
                });
            }
            Err(e) => eprintln!("tray menu: {e}"),
        }
    });
}

/// The menu page measured itself (logical px): place it at the icon and show it.
pub fn show_menu(win: &WebviewWindow, width: f64, height: f64) -> tauri::Result<()> {
    let Some(anchor) = *lock(&ANCHOR) else {
        return win.close();
    };
    let (cx, cy) = anchor.cursor;
    let monitor = win
        .monitor_from_point(f64::from(cx), f64::from(cy))?
        .or(win.primary_monitor()?)
        .ok_or(tauri::Error::WindowNotFound)?;
    let scale = monitor.scale_factor();
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
        } // no icon rect: anchor at the cursor
    };
    let size = ((width * scale).ceil() as i32, (height * scale).ceil() as i32);
    let (x, y) = place_menu(icon, m, work, size);
    win.set_position(PhysicalPosition::new(x, y))?;
    win.set_size(LogicalSize::new(width, height))?;
    win.show()?;
    win.set_focus()
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
}
