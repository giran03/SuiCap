//! Application windows that can be recorded instead of a whole display: listing them, finding
//! the one a source setting means, and the window in front (full screen or not).

use windows::{
    Win32::{
        Foundation::{HWND, LPARAM, RECT},
        Graphics::Dwm::{DWMWA_CLOAKED, DwmGetWindowAttribute},
        System::Threading::GetCurrentProcessId,
        UI::WindowsAndMessaging::{
            EnumWindows, GWL_EXSTYLE, GetClassNameW, GetForegroundWindow, GetWindowLongW, GetWindowRect,
            GetWindowTextLengthW, GetWindowTextW, GetWindowThreadProcessId, IsIconic, IsWindow, IsWindowVisible,
            WS_EX_APPWINDOW, WS_EX_TOOLWINDOW,
        },
    },
    core::BOOL,
};

static ACTIVE_APP: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

/// The program the video source is recording as a window right now (set by the pipeline), so
/// clip folders are named after it.
pub fn set_active_app(exe: Option<String>) {
    *ACTIVE_APP.lock().unwrap_or_else(|e| e.into_inner()) = exe;
}

pub fn active_app() -> Option<String> {
    ACTIVE_APP.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

/// One top-level window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowInfo {
    pub hwnd: isize,
    pub pid: u32,
    /// Program file name, e.g. `game.exe`.
    pub exe: String,
    pub title: String,
    pub class: String,
    pub width: u32,
    pub height: u32,
}

/// Programs that own the desktop and taskbar: never a game, never worth recording alone.
const SHELL: [&str; 5] = [
    "explorer.exe",
    "searchhost.exe",
    "shellexperiencehost.exe",
    "applicationframehost.exe",
    "startmenuexperiencehost.exe",
];

pub fn is_shell(exe: &str) -> bool {
    SHELL.iter().any(|s| exe.eq_ignore_ascii_case(s))
}

fn hw(h: isize) -> HWND {
    HWND(h as *mut _)
}

/// Window still exists (a recorded game can close and reopen with a new handle).
pub fn window_exists(hwnd: isize) -> bool {
    // SAFETY: IsWindow accepts any handle value.
    unsafe { IsWindow(Some(hw(hwnd))).as_bool() }
}

fn text_of(hwnd: HWND) -> String {
    // SAFETY: buffer sized from the reported length; a failed call leaves it empty.
    unsafe {
        let n = GetWindowTextLengthW(hwnd);
        if n <= 0 {
            return String::new();
        }
        let mut buf = vec![0u16; n as usize + 1];
        let got = GetWindowTextW(hwnd, &mut buf);
        String::from_utf16_lossy(&buf[..got.max(0) as usize])
    }
}

fn class_of(hwnd: HWND) -> String {
    let mut buf = [0u16; 256];
    // SAFETY: fixed buffer.
    let n = unsafe { GetClassNameW(hwnd, &mut buf) };
    String::from_utf16_lossy(&buf[..n.max(0) as usize])
}

pub(crate) fn info_of(hwnd: HWND, exe_of: &dyn Fn(u32) -> Option<String>) -> Option<WindowInfo> {
    // SAFETY: plain Win32 queries with locals as out-params.
    unsafe {
        let mut pid = 0u32;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        if pid == 0 || pid == GetCurrentProcessId() {
            return None;
        }
        let mut r = RECT::default();
        GetWindowRect(hwnd, &mut r).ok()?;
        Some(WindowInfo {
            hwnd: hwnd.0 as isize,
            pid,
            exe: exe_of(pid)?,
            title: text_of(hwnd),
            class: class_of(hwnd),
            width: (r.right - r.left).max(0) as u32,
            height: (r.bottom - r.top).max(0) as u32,
        })
    }
}

fn exe_lookup() -> impl Fn(u32) -> Option<String> {
    let procs = crate::audio::process_list();
    move |pid| procs.iter().find(|(p, _)| *p == pid).map(|(_, e)| e.clone())
}

/// Windows you could record: visible, not minimised or hidden by Windows, with a title, not
/// tool windows, not SuiCap's own, and big enough to be a real window.
pub fn list_windows() -> Vec<WindowInfo> {
    struct Ctx {
        found: Vec<HWND>,
    }
    unsafe extern "system" fn each(hwnd: HWND, lparam: LPARAM) -> BOOL {
        // SAFETY: lparam is the `Ctx` passed to EnumWindows below, alive for the whole call.
        let ctx = unsafe { &mut *(lparam.0 as *mut Ctx) };
        // SAFETY: plain Win32 queries on a handle EnumWindows gave us.
        unsafe {
            if !IsWindowVisible(hwnd).as_bool() || IsIconic(hwnd).as_bool() {
                return true.into();
            }
            let mut cloaked = 0u32;
            let _ = DwmGetWindowAttribute(hwnd, DWMWA_CLOAKED, (&raw mut cloaked).cast(), size_of::<u32>() as u32);
            if cloaked != 0 {
                return true.into();
            }
            let ex = GetWindowLongW(hwnd, GWL_EXSTYLE) as u32;
            if ex & WS_EX_TOOLWINDOW.0 != 0 && ex & WS_EX_APPWINDOW.0 == 0 {
                return true.into();
            }
            if GetWindowTextLengthW(hwnd) == 0 {
                return true.into();
            }
        }
        ctx.found.push(hwnd);
        true.into()
    }
    let mut ctx = Ctx { found: Vec::new() };
    // SAFETY: the callback only reads `ctx`, which outlives the call.
    unsafe {
        let _ = EnumWindows(Some(each), LPARAM((&raw mut ctx) as isize));
    }
    let exe_of = exe_lookup();
    let mut list: Vec<WindowInfo> = ctx
        .found
        .into_iter()
        .filter_map(|h| info_of(h, &exe_of))
        .filter(|w| w.width >= 100 && w.height >= 60 && !is_shell(&w.exe))
        .collect();
    list.sort_by(|a, b| {
        a.exe
            .to_ascii_lowercase()
            .cmp(&b.exe.to_ascii_lowercase())
            .then_with(|| (b.width * b.height).cmp(&(a.width * a.height)))
    });
    list
}

/// The window a source setting means: the program must match; a title, if given, narrows
/// it (a match is preferred, but a lone window of the program still wins when the title has
/// changed, e.g. a browser tab); among the rest the biggest one.
pub fn pick_window<'a>(list: &'a [WindowInfo], exe: &str, title: &str) -> Option<&'a WindowInfo> {
    let title = title.trim().to_lowercase();
    list.iter().filter(|w| w.exe.eq_ignore_ascii_case(exe)).max_by_key(|w| {
        let hit = !title.is_empty() && w.title.to_lowercase().contains(&title);
        (hit, u64::from(w.width) * u64::from(w.height))
    })
}

/// Find the window for `exe` / `title` right now.
pub fn find_window(exe: &str, title: &str) -> Option<WindowInfo> {
    pick_window(&list_windows(), exe, title).cloned()
}

/// The window in front, if it is an ordinary app window (not the desktop or shell). For the
/// game (full screen, exclusive, or a known game in a window) use [`crate::game::detect`].
pub fn foreground_window() -> Option<WindowInfo> {
    // SAFETY: plain Win32 query.
    let hwnd = unsafe { GetForegroundWindow() };
    if hwnd.is_invalid() {
        return None;
    }
    let info = info_of(hwnd, &crate::audio::pid_exe)?;
    (!is_shell(&info.exe) && info.width >= 100 && info.height >= 60).then_some(info)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(exe: &str, title: &str, width: u32, height: u32) -> WindowInfo {
        WindowInfo {
            hwnd: 1,
            pid: 1,
            exe: exe.into(),
            title: title.into(),
            class: "c".into(),
            width,
            height,
        }
    }

    #[test]
    fn picks_by_program_then_title_then_size() {
        let list = vec![
            w("game.exe", "Launcher", 400, 300),
            w("game.exe", "Game - Level 1", 1920, 1080),
            w("other.exe", "Game - Level 1", 3000, 2000),
        ];
        // the program must match, the biggest window wins
        assert_eq!(pick_window(&list, "GAME.EXE", "").unwrap().title, "Game - Level 1");
        // a title match beats size
        assert_eq!(pick_window(&list, "game.exe", "launch").unwrap().title, "Launcher");
        // a title that no longer matches still finds the program's window
        assert_eq!(pick_window(&list, "game.exe", "gone").unwrap().width, 1920);
        assert!(pick_window(&list, "missing.exe", "").is_none());
    }

    #[test]
    fn shell_programs_are_not_targets() {
        assert!(is_shell("Explorer.EXE"));
        assert!(!is_shell("game.exe"));
    }

    /// Smoke test on the real desktop: nothing panics, entries are sane, SuiCap is not listed.
    #[test]
    fn lists_windows_on_this_machine() {
        for w in list_windows() {
            println!("[win] {} | {} | {}x{}", w.exe, w.title, w.width, w.height);
            assert!(!w.exe.is_empty() && w.width >= 100 && w.pid != std::process::id());
        }
    }
}
