//! Apps for the audio settings page: which programs have an audio session on a playback
//! device (and whether they are playing right now), and their icons as PNG. FFI module;
//! the PNG and base64 encoders are plain Rust and unit-tested.

use std::{
    collections::{BTreeMap, HashMap},
    ffi::c_void,
    path::{Path, PathBuf},
    sync::Mutex,
    time::{Duration, Instant},
};

use windows::{
    Win32::{
        Devices::FunctionDiscovery::PKEY_Device_FriendlyName,
        Foundation::{CloseHandle, HWND},
        Graphics::Gdi::{
            BI_RGB, BITMAP, BITMAPINFO, BITMAPINFOHEADER, DIB_RGB_COLORS, DeleteObject, GetDC, GetDIBits, GetObjectW,
            HGDIOBJ, ReleaseDC,
        },
        Media::Audio::{
            AudioSessionStateActive, DEVICE_STATE_ACTIVE, IAudioSessionControl2, IAudioSessionManager2, IMMDevice,
            IMMDeviceEnumerator, MMDeviceEnumerator, eCapture, eConsole, eRender,
        },
        Storage::FileSystem::FILE_ATTRIBUTE_NORMAL,
        System::{
            Com::{
                CLSCTX_ALL, CoCreateInstance, CoTaskMemFree, STGM_READ, StructuredStorage::PropVariantToStringAlloc,
            },
            Registry::{HKEY, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ, RegGetValueW},
            Threading::{
                OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
            },
        },
        UI::{
            Shell::{SHFILEINFOW, SHGFI_ICON, SHGFI_LARGEICON, SHGetFileInfoW},
            WindowsAndMessaging::{DestroyIcon, GetIconInfo, ICONINFO},
        },
    },
    core::{Interface, PCWSTR, PWSTR},
};

use crate::{audio, capture};

/// A program with an audio session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioApp {
    pub exe: String,
    /// Full path of the running image.
    pub path: String,
    /// The session's own display name, else the exe name without `.exe`.
    pub name: String,
    pub playing: bool,
}

/// Paths seen by the last scan, so an icon comes from the instance that is running.
static SEEN: Mutex<Option<HashMap<String, PathBuf>>> = Mutex::new(None);

// ---------------------------------------------------------------------------
// Microphones.

/// An active capture device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InputDevice {
    /// Endpoint id, stable across reboots; what the settings store.
    pub id: String,
    pub name: String,
    /// The system default input right now.
    pub is_default: bool,
}

/// Active microphones and other inputs, default first, then by name.
pub fn input_devices() -> Vec<InputDevice> {
    capture::com_init();
    let mut list = Vec::new();
    // SAFETY: COM calls on interfaces we own; CoTaskMem strings are freed after copying.
    let _ = (|| -> windows::core::Result<()> {
        unsafe {
            let en: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
            let default_id = en
                .GetDefaultAudioEndpoint(eCapture, eConsole)
                .ok()
                .and_then(|d| device_id(&d));
            let devices = en.EnumAudioEndpoints(eCapture, DEVICE_STATE_ACTIVE)?;
            for i in 0..devices.GetCount()? {
                let dev = devices.Item(i)?;
                let Some(id) = device_id(&dev) else {
                    continue;
                };
                let name = friendly_name(&dev).unwrap_or_else(|| "Unnamed input".into());
                list.push(InputDevice {
                    is_default: default_id.as_deref() == Some(id.as_str()),
                    id,
                    name,
                });
            }
        }
        Ok(())
    })();
    list.sort_by(|a, b| b.is_default.cmp(&a.is_default).then_with(|| a.name.cmp(&b.name)));
    list
}

unsafe fn device_id(dev: &IMMDevice) -> Option<String> {
    unsafe {
        let raw = dev.GetId().ok()?;
        let id = raw.to_string().ok();
        CoTaskMemFree(Some(raw.0.cast()));
        id
    }
}

unsafe fn friendly_name(dev: &IMMDevice) -> Option<String> {
    unsafe {
        let store = dev.OpenPropertyStore(STGM_READ).ok()?;
        let value = store.GetValue(&PKEY_Device_FriendlyName).ok()?;
        let raw = PropVariantToStringAlloc(&raw const value).ok()?;
        let name = raw.to_string().ok();
        CoTaskMemFree(Some(raw.0.cast()));
        name.map(|n| n.trim().to_owned()).filter(|n| !n.is_empty())
    }
}

// ---------------------------------------------------------------------------
// Which programs are on this machine.

/// Per-folder entry budget, so one odd directory cannot stall a scan.
const SCAN_BUDGET: usize = 60_000;
const SCAN_TTL: Duration = Duration::from_secs(120);
/// Folders that are large and never hold programs worth listing.
const SKIP_DIRS: [&str; 9] = [
    "temp",
    "packages",
    "cache",
    "code cache",
    "gpucache",
    "node_modules",
    "crashdumps",
    "d3dscache",
    "windowsapps",
];

static INSTALLED: Mutex<Option<(Instant, HashMap<String, PathBuf>)>> = Mutex::new(None);

/// A folder to index: how many levels below it to look, and whether it sits in the user's
/// profile (where Microsoft's own huge folder is skipped).
struct Root {
    path: PathBuf,
    depth: usize,
    in_profile: bool,
}

/// Program Files is looked at deeply (Chrome sits four folders down); the profile folders are
/// big and cluttered, so only their top levels are (Discord is `Local\Discord\app-x\`).
fn scan_roots() -> Vec<Root> {
    let env = |k: &str| std::env::var_os(k).map(PathBuf::from);
    let mut roots = Vec::new();
    let mut add = |path: Option<PathBuf>, depth, in_profile| {
        if let Some(path) = path.filter(|p| p.is_dir() && !roots.iter().any(|r: &Root| r.path == *p)) {
            roots.push(Root {
                path,
                depth,
                in_profile,
            });
        }
    };
    for k in ["ProgramFiles", "ProgramFiles(x86)", "ProgramW6432"] {
        add(env(k), 4, false);
    }
    add(env("LOCALAPPDATA").map(|p| p.join("Programs")), 3, true);
    add(env("LOCALAPPDATA"), 2, true);
    add(env("APPDATA"), 2, true);
    roots
}

/// Every `.exe` under the usual install folders, name (lowercase) -> path. Cached for a couple
/// of minutes because walking Program Files takes a moment.
fn installed_index() -> HashMap<String, PathBuf> {
    if let Ok(g) = INSTALLED.lock()
        && let Some((at, map)) = g.as_ref()
        && at.elapsed() < SCAN_TTL
    {
        return map.clone();
    }
    // One thread per folder: this is disk latency, not CPU.
    let mut map: HashMap<String, PathBuf> = HashMap::new();
    let roots = scan_roots();
    let parts: Vec<HashMap<String, PathBuf>> = std::thread::scope(|s| {
        let jobs: Vec<_> = roots
            .iter()
            .map(|r| {
                s.spawn(move || {
                    let mut part = HashMap::new();
                    let mut budget = SCAN_BUDGET;
                    walk(&r.path, 0, r.depth, r.in_profile, &mut budget, &mut part);
                    part
                })
            })
            .collect();
        jobs.into_iter().filter_map(|j| j.join().ok()).collect()
    });
    for part in parts {
        for (k, v) in part {
            map.entry(k).or_insert(v);
        }
    }
    if let Ok(mut g) = INSTALLED.lock() {
        *g = Some((Instant::now(), map.clone()));
    }
    map
}

fn walk(
    dir: &Path,
    depth: usize,
    max: usize,
    in_profile: bool,
    budget: &mut usize,
    out: &mut HashMap<String, PathBuf>,
) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        if *budget == 0 {
            return;
        }
        *budget -= 1;
        let path = e.path();
        let name = e.file_name().to_string_lossy().to_ascii_lowercase();
        let Ok(kind) = e.file_type() else {
            continue;
        };
        if kind.is_dir() {
            let skip = SKIP_DIRS.contains(&name.as_str()) || (in_profile && name == "microsoft");
            if depth < max && !skip {
                walk(&path, depth + 1, max, in_profile, budget, out);
            }
        } else if name.ends_with(".exe") {
            out.entry(name).or_insert(path);
        }
    }
}

/// Start indexing the install folders in the background, so `present` is quick later.
pub fn warm_up() {
    std::thread::spawn(|| drop(installed_index()));
}

/// Which of `exes` exist here: running right now, installed under a usual folder, or
/// registered in App Paths. Lowercase names.
pub fn present(exes: &[String]) -> Vec<String> {
    let running: std::collections::HashSet<String> = audio::process_list()
        .into_iter()
        .map(|(_, n)| n.to_ascii_lowercase())
        .collect();
    let wanted: Vec<String> = exes.iter().map(|e| e.to_ascii_lowercase()).collect();
    // Cheap answers first; the folder scan only runs for what is still unknown.
    let unknown = |e: &&String| !running.contains(*e) && registered_path(e).is_none();
    let rest: Vec<&String> = wanted.iter().filter(unknown).collect();
    let index = if rest.is_empty() {
        HashMap::new()
    } else {
        installed_index()
    };
    wanted
        .iter()
        .filter(|e| running.contains(*e) || index.contains_key(*e) || registered_path(e).is_some())
        .cloned()
        .collect()
}

/// Exe name and full path of `pid`. Asks for PROCESS_QUERY_LIMITED_INFORMATION only, never
/// memory or full access, so protected (anti-cheat) games are left alone.
fn image_info(pid: u32) -> Option<(String, String)> {
    if pid == 0 || pid == 4 {
        return None;
    }
    // SAFETY: handle closed below; the buffer outlives the call.
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut buf = [0u16; 1024];
        let mut len = buf.len() as u32;
        let ok = QueryFullProcessImageNameW(h, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut len).is_ok();
        let _ = CloseHandle(h);
        if !ok || len == 0 {
            return None;
        }
        let path = String::from_utf16_lossy(&buf[..len as usize]);
        let exe = std::path::Path::new(&path).file_name()?.to_str()?.to_owned();
        Some((exe, path))
    }
}

/// Programs with an audio session on any active playback device, playing ones first.
/// Detection follows Stellars: walk every active render endpoint's session manager, resolve
/// each session's own PID to its image, and merge sessions of one exe (a browser has many).
/// Empty if the audio stack cannot be queried.
pub fn audio_apps() -> Vec<AudioApp> {
    capture::com_init();
    let mut found: BTreeMap<String, AudioApp> = BTreeMap::new();
    let me = std::process::id();
    // SAFETY: COM calls on interfaces we own, on a COM-initialised thread.
    let _ = (|| -> windows::core::Result<()> {
        unsafe {
            let enumerator: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
            let devices = enumerator.EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE)?;
            for d in 0..devices.GetCount()? {
                let Ok(manager) = devices.Item(d)?.Activate::<IAudioSessionManager2>(CLSCTX_ALL, None) else {
                    continue;
                };
                let sessions = manager.GetSessionEnumerator()?;
                for i in 0..sessions.GetCount()? {
                    let Ok(control) = sessions.GetSession(i)?.cast::<IAudioSessionControl2>() else {
                        continue;
                    };
                    if control.IsSystemSoundsSession().0 == 0 {
                        continue; // S_OK: the Windows system-sounds session
                    }
                    let pid = control.GetProcessId().unwrap_or(0);
                    if pid == 0 || pid == me {
                        continue;
                    }
                    let Some((exe, path)) = image_info(pid) else {
                        continue;
                    };
                    let playing = control.GetState().is_ok_and(|s| s == AudioSessionStateActive);
                    let shown = session_name(&control);
                    let entry = found.entry(exe.to_ascii_lowercase()).or_insert_with(|| AudioApp {
                        name: exe.strip_suffix(".exe").unwrap_or(&exe).to_owned(),
                        exe,
                        path,
                        playing: false,
                    });
                    entry.playing |= playing;
                    // Keep the first real session name; the exe stem is only the placeholder.
                    if let Some(n) = shown
                        && entry.name.eq_ignore_ascii_case(entry.exe.trim_end_matches(".exe"))
                    {
                        entry.name = n;
                    }
                }
            }
        }
        Ok(())
    })();
    let mut list: Vec<AudioApp> = found.into_values().collect();
    list.sort_by_key(|a| !a.playing);
    if let Ok(mut seen) = SEEN.lock() {
        let map = seen.get_or_insert_with(HashMap::new);
        for a in &list {
            map.insert(a.exe.to_ascii_lowercase(), PathBuf::from(&a.path));
        }
    }
    list
}

/// The session's display name, unless it is empty or a `@resource` string.
fn session_name(control: &IAudioSessionControl2) -> Option<String> {
    // SAFETY: the returned string is CoTaskMem-allocated and freed here.
    unsafe {
        let raw = control.GetDisplayName().ok()?;
        let s = raw.to_string().ok();
        CoTaskMemFree(Some(raw.0.cast()));
        s.map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty() && !s.starts_with('@'))
    }
}

// ---------------------------------------------------------------------------
// Icons.

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

/// Full path of a running process by exe name, else the App Paths registration of an
/// installed program.
fn exe_path(exe: &str) -> Option<PathBuf> {
    // From the last scan, else from any running process of that name.
    if let Some(p) = SEEN
        .lock()
        .ok()
        .and_then(|s| s.as_ref()?.get(&exe.to_ascii_lowercase()).cloned())
        && p.is_file()
    {
        return Some(p);
    }
    for (pid, name) in audio::process_list() {
        if name.eq_ignore_ascii_case(exe)
            && let Some((_, path)) = image_info(pid)
        {
            return Some(PathBuf::from(path));
        }
    }
    installed_index()
        .get(&exe.to_ascii_lowercase())
        .cloned()
        .or_else(|| registered_path(exe))
}

/// The App Paths registration of an installed program.
fn registered_path(exe: &str) -> Option<PathBuf> {
    let key = wide(&format!(r"SOFTWARE\Microsoft\Windows\CurrentVersion\App Paths\{exe}"));
    for root in [HKEY_LOCAL_MACHINE, HKEY_CURRENT_USER] {
        if let Some(p) = registry_default(root, &key) {
            let p = PathBuf::from(p.trim_matches('"'));
            if p.is_file() {
                return Some(p);
            }
        }
    }
    None
}

fn registry_default(root: HKEY, subkey: &[u16]) -> Option<String> {
    let mut buf = [0u16; 520];
    let mut bytes = (buf.len() * 2) as u32;
    // SAFETY: the buffer and its byte length describe the same allocation.
    let r = unsafe {
        RegGetValueW(
            root,
            PCWSTR(subkey.as_ptr()),
            PCWSTR::null(),
            RRF_RT_REG_SZ,
            None,
            Some(buf.as_mut_ptr().cast::<c_void>()),
            Some(&mut bytes),
        )
    };
    if r.0 != 0 {
        return None;
    }
    let n = (bytes as usize / 2).saturating_sub(1).min(buf.len());
    Some(String::from_utf16_lossy(&buf[..n]))
}

/// Where a program lives: the running image, the last scan, an install folder or App Paths.
pub fn app_path(exe: &str) -> Option<PathBuf> {
    exe_path(exe)
}

/// The program's icon (32 px) as a PNG, or `None` if it cannot be found.
pub fn app_icon_png(exe: &str) -> Option<Vec<u8>> {
    let path = exe_path(exe)?;
    let wpath = wide(&path.to_string_lossy());
    // SAFETY: every GDI object created here is released before returning.
    unsafe {
        let mut info = SHFILEINFOW::default();
        let r = SHGetFileInfoW(
            PCWSTR(wpath.as_ptr()),
            FILE_ATTRIBUTE_NORMAL,
            Some(&mut info),
            size_of::<SHFILEINFOW>() as u32,
            SHGFI_ICON | SHGFI_LARGEICON,
        );
        if r == 0 || info.hIcon.is_invalid() {
            return None;
        }
        let mut ii = ICONINFO::default();
        let got = GetIconInfo(info.hIcon, &mut ii).is_ok();
        let rgba = if got { read_rgba(&ii) } else { None };
        if got {
            let _ = DeleteObject(HGDIOBJ(ii.hbmColor.0));
            let _ = DeleteObject(HGDIOBJ(ii.hbmMask.0));
        }
        let _ = DestroyIcon(info.hIcon);
        let (w, h, px) = rgba?;
        Some(encode_png(w, h, &px))
    }
}

/// BGRA pixels of the icon's colour bitmap as RGBA.
unsafe fn read_rgba(ii: &ICONINFO) -> Option<(u32, u32, Vec<u8>)> {
    unsafe {
        if ii.hbmColor.is_invalid() {
            return None;
        }
        let mut bm = BITMAP::default();
        if GetObjectW(
            HGDIOBJ(ii.hbmColor.0),
            size_of::<BITMAP>() as i32,
            Some((&raw mut bm).cast()),
        ) == 0
        {
            return None;
        }
        let (w, h) = (bm.bmWidth as u32, bm.bmHeight as u32);
        if w == 0 || h == 0 || w > 256 || h > 256 {
            return None;
        }
        let mut bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w as i32,
                biHeight: -(h as i32), // top-down
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut px = vec![0u8; (w * h * 4) as usize];
        let dc = GetDC(Some(HWND::default()));
        let lines = GetDIBits(
            dc,
            ii.hbmColor,
            0,
            h,
            Some(px.as_mut_ptr().cast()),
            &mut bmi,
            DIB_RGB_COLORS,
        );
        ReleaseDC(Some(HWND::default()), dc);
        if lines == 0 {
            return None;
        }
        // Icons without an alpha channel come back with alpha 0 everywhere: make them opaque.
        let opaque = px.chunks_exact(4).all(|p| p[3] == 0);
        for p in px.chunks_exact_mut(4) {
            p.swap(0, 2); // BGRA -> RGBA
            if opaque {
                p[3] = 255;
            }
        }
        Some((w, h, px))
    }
}

// ---------------------------------------------------------------------------
// PNG (stored deflate) and base64, so icons need no extra crates.

fn crc32(data: &[u8]) -> u32 {
    let mut c = 0xFFFF_FFFFu32;
    for &b in data {
        c ^= u32::from(b);
        for _ in 0..8 {
            c = if c & 1 != 0 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 };
        }
    }
    !c
}

fn adler32(data: &[u8]) -> u32 {
    let (mut a, mut b) = (1u32, 0u32);
    for &x in data {
        a = (a + u32::from(x)) % 65_521;
        b = (b + a) % 65_521;
    }
    (b << 16) | a
}

fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], body: &[u8]) {
    out.extend((body.len() as u32).to_be_bytes());
    let start = out.len();
    out.extend(kind);
    out.extend(body);
    let crc = crc32(&out[start..]);
    out.extend(crc.to_be_bytes());
}

/// An 8-bit RGBA PNG with uncompressed deflate blocks (icons are tiny).
pub fn encode_png(w: u32, h: u32, rgba: &[u8]) -> Vec<u8> {
    let mut raw = Vec::with_capacity(rgba.len() + h as usize);
    for row in rgba.chunks_exact((w * 4) as usize) {
        raw.push(0); // filter: none
        raw.extend(row);
    }
    let mut z = vec![0x78, 0x01];
    let mut blocks = raw.chunks(65_535).peekable();
    while let Some(b) = blocks.next() {
        z.push(u8::from(blocks.peek().is_none())); // BFINAL, BTYPE=00
        z.extend((b.len() as u16).to_le_bytes());
        z.extend((!(b.len() as u16)).to_le_bytes());
        z.extend(b);
    }
    z.extend(adler32(&raw).to_be_bytes());

    let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
    let mut ihdr = Vec::new();
    ihdr.extend(w.to_be_bytes());
    ihdr.extend(h.to_be_bytes());
    ihdr.extend([8, 6, 0, 0, 0]); // 8-bit RGBA
    chunk(&mut out, b"IHDR", &ihdr);
    chunk(&mut out, b"IDAT", &z);
    chunk(&mut out, b"IEND", &[]);
    out
}

pub fn base64(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut s = String::with_capacity(data.len().div_ceil(3) * 4);
    for c in data.chunks(3) {
        let n =
            (u32::from(c[0]) << 16) | (u32::from(*c.get(1).unwrap_or(&0)) << 8) | u32::from(*c.get(2).unwrap_or(&0));
        s.push(T[(n >> 18) as usize & 63] as char);
        s.push(T[(n >> 12) as usize & 63] as char);
        s.push(if c.len() > 1 {
            T[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        s.push(if c.len() > 2 { T[n as usize & 63] as char } else { '=' });
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Smoke test on the real audio stack: names come back, and at most one input is the default.
    #[test]
    fn lists_input_devices_on_this_machine() {
        let devices = input_devices();
        for d in &devices {
            println!("[mic] {} | {} | default={}", d.id, d.name, d.is_default);
            assert!(!d.id.is_empty() && !d.name.is_empty());
        }
        assert!(devices.iter().filter(|d| d.is_default).count() <= 1);
    }

    #[test]
    fn base64_matches_rfc4648() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn present_finds_running_programs_and_skips_missing_ones() {
        let me = std::env::current_exe().unwrap();
        let me = me.file_name().unwrap().to_string_lossy().into_owned();
        let t0 = Instant::now();
        let found = present(&[me.clone(), "definitely-not-installed-9f3a.exe".into()]);
        println!(
            "[apps] scan {:?}, {} exes indexed",
            t0.elapsed(),
            installed_index().len()
        );
        assert_eq!(found, vec![me.to_ascii_lowercase()]);
    }

    #[test]
    fn crc_and_adler_known_values() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        assert_eq!(adler32(b"Wikipedia"), 0x11E6_0398);
    }

    #[test]
    fn png_has_signature_chunks_and_matching_size() {
        let png = encode_png(2, 2, &[255; 16]);
        assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
        assert_eq!(&png[12..16], b"IHDR");
        assert_eq!(u32::from_be_bytes(png[16..20].try_into().unwrap()), 2);
        assert_eq!(&png[png.len() - 8..png.len() - 4], b"IEND");
    }

    /// Smoke test on the real audio stack: it must not panic, and icons it finds must be PNGs.
    #[test]
    fn lists_audio_apps_and_icons_on_this_machine() {
        let apps = audio_apps();
        for a in &apps {
            let icon = app_icon_png(&a.exe);
            println!(
                "[apps] {} | {} | playing={} | icon={:?}",
                a.exe,
                a.name,
                a.playing,
                icon.as_ref().map(Vec::len)
            );
            if let Some(png) = icon {
                println!("[apps] crc={:08x}", crc32(&png));
                if let Ok(dir) = std::env::var("SUICAP_ICON_DUMP") {
                    let _ = std::fs::write(format!("{dir}/{}.png", a.exe), &png);
                }
                assert!(png.starts_with(&[0x89, b'P', b'N', b'G']));
            }
        }
    }
}
