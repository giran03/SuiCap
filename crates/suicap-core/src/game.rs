//! Finding the game: the window being played in, whatever its window mode.
//!
//! A window is the game when it
//! - is in exclusive fullscreen (the shell reports a Direct3D full-screen app in front),
//! - covers its whole monitor (borderless or windowed fullscreen, or exclusive at any mode),
//! - or belongs to a program Windows knows as a game (the Game Bar list), in any window mode.
//!
//! SuiCap's own windows, the shell, click-through overlays and minimised windows never count,
//! so the settings window opened in front of a running game does not hide it.

use std::{
    collections::{HashMap, HashSet},
    sync::Mutex,
    time::{Duration, Instant},
};

use windows::{
    Win32::{
        Foundation::{ERROR_SUCCESS, HWND, LPARAM, RECT},
        Graphics::{
            Dwm::{DWMWA_CLOAKED, DWMWA_EXTENDED_FRAME_BOUNDS, DwmGetWindowAttribute},
            Gdi::{GetMonitorInfoW, HMONITOR, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromWindow},
        },
        System::{
            Registry::{
                HKEY, HKEY_CURRENT_USER, KEY_READ, REG_VALUE_TYPE, RRF_RT_REG_DWORD, RRF_RT_REG_SZ, RegCloseKey,
                RegEnumKeyExW, RegGetValueW, RegOpenKeyExW,
            },
            Threading::GetCurrentProcessId,
        },
        UI::{
            Shell::{QUNS_RUNNING_D3D_FULL_SCREEN, SHQueryUserNotificationState},
            WindowsAndMessaging::{
                EnumWindows, GWL_EXSTYLE, GWL_STYLE, GetForegroundWindow, GetWindowLongW, GetWindowRect,
                GetWindowThreadProcessId, IsIconic, IsWindowVisible, IsZoomed, WS_CAPTION, WS_EX_APPWINDOW,
                WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TRANSPARENT, WS_THICKFRAME,
            },
        },
    },
    core::{BOOL, PCWSTR, PWSTR, w},
};

use crate::targets::{WindowInfo, is_shell};

/// How the game is shown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GameMode {
    /// Exclusive fullscreen: the game owns the display (and may have changed its mode).
    Exclusive,
    /// Covers its whole monitor: borderless or windowed fullscreen.
    Fullscreen,
    /// A known game in an ordinary window.
    Windowed,
}

impl GameMode {
    /// The game fills its monitor, so a display capture shows only the game.
    pub fn is_fullscreen(self) -> bool {
        matches!(self, GameMode::Exclusive | GameMode::Fullscreen)
    }
}

#[derive(Debug, Clone)]
pub struct Game {
    pub window: WindowInfo,
    pub mode: GameMode,
    /// Windows lists the program as a game (not just an app that fills the screen).
    pub known: bool,
}

/// What detection knows about one window. Pure data, so the rules below are unit-tested.
#[derive(Debug, Clone, Copy, Default)]
struct Facts {
    foreground: bool,
    /// The window in front is SuiCap, the shell or nothing: no other app is being used.
    front_is_ours: bool,
    /// No big window of another app is above it on its monitor.
    top_of_monitor: bool,
    covers_monitor: bool,
    /// A normal maximised window (caption or sizing frame): it fills the work area by design,
    /// and the whole monitor when the taskbar auto-hides. Not fullscreen.
    maximized_frame: bool,
    known_game: bool,
    /// The shell reports an exclusive Direct3D full-screen app in front.
    exclusive_front: bool,
}

fn classify(f: Facts) -> Option<GameMode> {
    if f.foreground && f.exclusive_front {
        return Some(GameMode::Exclusive);
    }
    let full = f.covers_monitor && (!f.maximized_frame || f.known_game);
    // Something that only fills the screen is the game when it is in front, or when nothing
    // else is (SuiCap's window opened over it). Known games count wherever they are.
    if full && (f.foreground || f.known_game || (f.front_is_ours && f.top_of_monitor)) {
        return Some(GameMode::Fullscreen);
    }
    f.known_game.then_some(GameMode::Windowed)
}

/// Lower is better; ties go to the window higher in Z-order.
fn rank(f: &Facts, mode: GameMode) -> u8 {
    match (f.foreground, mode, f.known_game) {
        (true, GameMode::Exclusive, _) => 0,
        (true, GameMode::Fullscreen, true) => 0,
        (true, GameMode::Fullscreen, false) => 1,
        (true, GameMode::Windowed, _) => 2,
        (false, GameMode::Fullscreen, true) => 3,
        (false, GameMode::Fullscreen, false) => 4,
        (false, _, _) => 5,
    }
}

/// Covers `m` (a monitor rectangle), allowing one pixel of slack on each edge.
fn covers(r: &RECT, m: &RECT) -> bool {
    r.left <= m.left + 1 && r.top <= m.top + 1 && r.right >= m.right - 1 && r.bottom >= m.bottom - 1
}

fn area(r: &RECT) -> i64 {
    i64::from((r.right - r.left).max(0)) * i64::from((r.bottom - r.top).max(0))
}

/// Programs that draw over games or the desktop without being one.
const NOT_GAMES: [&str; 6] = [
    "nvidia overlay.exe",
    "gamebar.exe",
    "gamebarftserver.exe",
    "textinputhost.exe",
    "lockapp.exe",
    "screenclippinghost.exe",
];

fn never_a_game(exe: &str) -> bool {
    is_shell(exe) || NOT_GAMES.iter().any(|n| exe.eq_ignore_ascii_case(n))
}

/// Known game executables (lower-cased full paths) from the Game Bar list
/// (`HKCU\System\GameConfigStore\Children\*`, `Type` 1 = game). Re-read every 30 s.
fn known_games() -> HashSet<String> {
    static CACHE: Mutex<Option<(Instant, HashSet<String>)>> = Mutex::new(None);
    let mut g = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((at, set)) = g.as_ref()
        && at.elapsed() < Duration::from_secs(30)
    {
        return set.clone();
    }
    let set = read_known_games();
    *g = Some((Instant::now(), set.clone()));
    set
}

fn read_known_games() -> HashSet<String> {
    let mut set = HashSet::new();
    // SAFETY: the key is closed below; every buffer is a sized local.
    unsafe {
        let mut root = HKEY::default();
        if RegOpenKeyExW(
            HKEY_CURRENT_USER,
            w!("System\\GameConfigStore\\Children"),
            None,
            KEY_READ,
            &mut root,
        ) != ERROR_SUCCESS
        {
            return set;
        }
        for i in 0..4096u32 {
            let mut name = [0u16; 256];
            let mut len = name.len() as u32;
            if RegEnumKeyExW(root, i, Some(PWSTR(name.as_mut_ptr())), &mut len, None, None, None, None)
                != ERROR_SUCCESS
            {
                break;
            }
            let sub = PCWSTR(name.as_ptr());
            let mut kind = 0u32;
            let mut size = size_of::<u32>() as u32;
            let typed = RegGetValueW(
                root,
                sub,
                w!("Type"),
                RRF_RT_REG_DWORD,
                None,
                Some((&raw mut kind).cast()),
                Some(&mut size),
            ) == ERROR_SUCCESS;
            if typed && kind != 1 {
                continue;
            }
            let mut path = [0u16; 1024];
            let mut bytes = (path.len() * 2) as u32;
            let mut vt = REG_VALUE_TYPE::default();
            if RegGetValueW(
                root,
                sub,
                w!("MatchedExeFullPath"),
                RRF_RT_REG_SZ,
                Some(&mut vt),
                Some(path.as_mut_ptr().cast()),
                Some(&mut bytes),
            ) == ERROR_SUCCESS
            {
                let chars = (bytes as usize / 2).min(path.len());
                let end = path[..chars].iter().position(|&c| c == 0).unwrap_or(chars);
                let p = String::from_utf16_lossy(&path[..end]);
                if !p.is_empty() {
                    set.insert(p.to_lowercase());
                }
            }
        }
        let _ = RegCloseKey(root);
    }
    set
}

/// Visible bounds without the invisible resize borders (falls back to the window rect).
fn visible_bounds(hwnd: HWND) -> Option<RECT> {
    let mut r = RECT::default();
    // SAFETY: plain queries with a sized local.
    unsafe {
        if DwmGetWindowAttribute(hwnd, DWMWA_EXTENDED_FRAME_BOUNDS, (&raw mut r).cast(), size_of::<RECT>() as u32)
            .is_ok()
            && area(&r) > 0
        {
            return Some(r);
        }
        GetWindowRect(hwnd, &mut r).ok()?;
    }
    Some(r)
}

/// Shown, not minimised, not hidden by Windows, not a tool window or a click-through overlay.
fn eligible(hwnd: HWND) -> bool {
    // SAFETY: plain Win32 queries on a live handle.
    unsafe {
        if !IsWindowVisible(hwnd).as_bool() || IsIconic(hwnd).as_bool() {
            return false;
        }
        let mut cloaked = 0u32;
        let _ = DwmGetWindowAttribute(hwnd, DWMWA_CLOAKED, (&raw mut cloaked).cast(), size_of::<u32>() as u32);
        if cloaked != 0 {
            return false;
        }
        let ex = GetWindowLongW(hwnd, GWL_EXSTYLE) as u32;
        let tool = ex & WS_EX_TOOLWINDOW.0 != 0 && ex & WS_EX_APPWINDOW.0 == 0;
        !(tool || ex & WS_EX_TRANSPARENT.0 != 0 || ex & WS_EX_NOACTIVATE.0 != 0)
    }
}

fn top_level_windows() -> Vec<HWND> {
    unsafe extern "system" fn each(hwnd: HWND, lparam: LPARAM) -> BOOL {
        // SAFETY: lparam is the Vec passed below, alive for the whole call.
        let v = unsafe { &mut *(lparam.0 as *mut Vec<HWND>) };
        v.push(hwnd);
        true.into()
    }
    let mut v = Vec::new();
    // SAFETY: the callback only pushes into `v`, which outlives the call.
    unsafe {
        let _ = EnumWindows(Some(each), LPARAM((&raw mut v) as isize));
    }
    v
}

/// The game being played right now, if any. Takes a few milliseconds: call it from a
/// watcher thread, not once per video frame.
pub fn detect() -> Option<Game> {
    // SAFETY: plain Win32 queries with locals as out-params.
    let (fg, exclusive_front, me) = unsafe {
        let exclusive = SHQueryUserNotificationState().is_ok_and(|s| s == QUNS_RUNNING_D3D_FULL_SCREEN);
        (GetForegroundWindow(), exclusive, GetCurrentProcessId())
    };
    let known = known_games();
    let mut paths: HashMap<u32, Option<String>> = HashMap::new();
    let mut path_of = |pid: u32| paths.entry(pid).or_insert_with(|| crate::audio::pid_path(pid)).clone();

    let pid_of = |hwnd: HWND| {
        let mut pid = 0u32;
        // SAFETY: out-param is a local.
        unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
        pid
    };
    let front_is_ours = fg.is_invalid()
        || pid_of(fg) == me
        || path_of(pid_of(fg)).and_then(|p| crate::audio::exe_name(&p)).is_none_or(|e| never_a_game(&e));

    // Big windows already seen per monitor, in Z-order: what covers a lower window.
    let mut covered: Vec<HMONITOR> = Vec::new();
    let mut best: Option<(u8, Game)> = None;
    for hwnd in top_level_windows() {
        if !eligible(hwnd) {
            continue;
        }
        let pid = pid_of(hwnd);
        if pid == 0 || pid == me {
            continue;
        }
        let Some(bounds) = visible_bounds(hwnd) else { continue };
        if (bounds.right - bounds.left) < 100 || (bounds.bottom - bounds.top) < 60 {
            continue;
        }
        let Some(path) = path_of(pid) else { continue };
        let Some(exe) = crate::audio::exe_name(&path) else { continue };
        if never_a_game(&exe) {
            continue;
        }
        // SAFETY: plain Win32 queries on a live handle.
        let (monitor, mi, style, zoomed) = unsafe {
            let monitor = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
            let mut mi = MONITORINFO {
                cbSize: size_of::<MONITORINFO>() as u32,
                ..Default::default()
            };
            if !GetMonitorInfoW(monitor, &mut mi).as_bool() {
                continue;
            }
            (monitor, mi, GetWindowLongW(hwnd, GWL_STYLE) as u32, IsZoomed(hwnd).as_bool())
        };
        let facts = Facts {
            foreground: hwnd == fg,
            front_is_ours,
            top_of_monitor: !covered.contains(&monitor),
            covers_monitor: covers(&bounds, &mi.rcMonitor),
            maximized_frame: zoomed && style & (WS_CAPTION.0 | WS_THICKFRAME.0) != 0,
            known_game: known.contains(&path.to_lowercase()),
            exclusive_front,
        };
        if area(&bounds) * 2 >= area(&mi.rcMonitor) && !covered.contains(&monitor) {
            covered.push(monitor);
        }
        let Some(mode) = classify(facts) else { continue };
        let r = rank(&facts, mode);
        if best.as_ref().is_some_and(|(b, _)| *b <= r) {
            continue;
        }
        let Some(window) = crate::targets::info_of(hwnd, &|_| Some(exe.clone())) else {
            continue;
        };
        best = Some((
            r,
            Game {
                window,
                mode,
                known: facts.known_game,
            },
        ));
        if r == 0 {
            break;
        }
    }
    best.map(|(_, g)| g)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f(edit: impl FnOnce(&mut Facts)) -> Facts {
        let mut x = Facts {
            top_of_monitor: true,
            ..Default::default()
        };
        edit(&mut x);
        x
    }

    #[test]
    fn exclusive_fullscreen_in_front_is_the_game_even_with_odd_bounds() {
        let x = f(|x| {
            x.foreground = true;
            x.exclusive_front = true;
        });
        assert_eq!(classify(x), Some(GameMode::Exclusive));
        assert_eq!(rank(&x, GameMode::Exclusive), 0);
    }

    #[test]
    fn borderless_or_windowed_fullscreen_in_front_is_the_game() {
        let x = f(|x| {
            x.foreground = true;
            x.covers_monitor = true;
        });
        assert_eq!(classify(x), Some(GameMode::Fullscreen));
    }

    #[test]
    fn a_maximised_app_is_not_fullscreen_unless_it_is_a_known_game() {
        let app = f(|x| {
            x.foreground = true;
            x.covers_monitor = true; // auto-hidden taskbar: maximised fills the monitor
            x.maximized_frame = true;
        });
        assert_eq!(classify(app), None);
        let game = f(|x| {
            x.foreground = true;
            x.covers_monitor = true;
            x.maximized_frame = true;
            x.known_game = true;
        });
        assert_eq!(classify(game), Some(GameMode::Fullscreen));
    }

    #[test]
    fn a_fullscreen_window_behind_suicap_is_found() {
        // SuiCap started (its settings window in front) while a game already ran full screen.
        let behind = f(|x| {
            x.front_is_ours = true;
            x.covers_monitor = true;
        });
        assert_eq!(classify(behind), Some(GameMode::Fullscreen));
        // ...but not behind another app the user is working in, unless Windows knows the game.
        let behind_work = f(|x| x.covers_monitor = true);
        assert_eq!(classify(behind_work), None);
        let covered = f(|x| {
            x.front_is_ours = true;
            x.covers_monitor = true;
            x.top_of_monitor = false;
        });
        assert_eq!(classify(covered), None);
    }

    #[test]
    fn known_games_count_in_any_window_mode_and_rank_below_the_front() {
        let windowed = f(|x| x.known_game = true);
        assert_eq!(classify(windowed), Some(GameMode::Windowed));
        let front_full = f(|x| {
            x.foreground = true;
            x.covers_monitor = true;
        });
        assert!(rank(&front_full, GameMode::Fullscreen) < rank(&windowed, GameMode::Windowed));
        let behind_known_full = f(|x| {
            x.covers_monitor = true;
            x.known_game = true;
        });
        assert_eq!(classify(behind_known_full), Some(GameMode::Fullscreen));
        assert!(rank(&behind_known_full, GameMode::Fullscreen) < rank(&windowed, GameMode::Windowed));
    }

    #[test]
    fn an_ordinary_window_is_not_a_game() {
        assert_eq!(classify(f(|x| x.foreground = true)), None);
    }

    #[test]
    fn cover_allows_one_pixel_of_slack() {
        let m = RECT {
            left: 0,
            top: 0,
            right: 1920,
            bottom: 1080,
        };
        assert!(covers(&RECT { left: 1, top: 0, right: 1919, bottom: 1080 }, &m));
        assert!(!covers(&RECT { left: 0, top: 0, right: 1920, bottom: 1040 }, &m));
    }

    /// Smoke test on the real desktop and registry: nothing panics, SuiCap is never the game.
    #[test]
    fn detects_on_this_machine() {
        println!("[games] {} known", known_games().len());
        let g = detect();
        println!("[game] {g:?}");
        assert!(g.is_none_or(|g| g.window.pid != std::process::id() && !never_a_game(&g.window.exe)));
    }
}
