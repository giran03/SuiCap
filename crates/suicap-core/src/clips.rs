//! Clip library (plan §6.3): list, thumbnails, lossless trim, delete and disk-quota
//! clean-up. Every path that arrives from the UI goes through [`resolve`], which confines
//! it to the clips folder; nothing here takes a path it did not list itself.

use std::{
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use serde::Serialize;

use crate::mux;

const VIDEO_EXTS: [&str; 2] = ["mp4", "mkv"];
const IMAGE_EXTS: [&str; 1] = ["png"];
/// Files touched this recently are never auto-deleted (a recording may still be writing).
const QUOTA_GRACE: Duration = Duration::from_secs(120);
/// Longest range a trim may ask for.
const MAX_TRIM_SECS: f64 = 24.0 * 3600.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Video,
    Image,
}

#[derive(Debug, Clone, Serialize)]
pub struct ClipEntry {
    pub path: PathBuf,
    /// The per-app folder name ("" for files directly in the clips folder).
    pub app: String,
    pub name: String,
    pub kind: Kind,
    pub size: u64,
    /// Unix seconds.
    pub modified: u64,
}

fn kind_of(path: &Path) -> Option<Kind> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    if VIDEO_EXTS.contains(&ext.as_str()) {
        Some(Kind::Video)
    } else if IMAGE_EXTS.contains(&ext.as_str()) {
        Some(Kind::Image)
    } else {
        None
    }
}

fn unix(t: SystemTime) -> u64 {
    t.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

fn scan_dir(dir: &Path, app: &str, out: &mut Vec<ClipEntry>, recurse: bool) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let Ok(ft) = e.file_type() else { continue };
        if ft.is_symlink() {
            continue; // links could point out of the clips folder
        }
        let path = e.path();
        if ft.is_dir() {
            if recurse {
                let name = e.file_name().to_string_lossy().into_owned();
                scan_dir(&path, &name, out, false);
            }
        } else if let Some(kind) = kind_of(&path)
            && let Ok(md) = e.metadata()
        {
            out.push(ClipEntry {
                name: e.file_name().to_string_lossy().into_owned(),
                app: app.to_owned(),
                kind,
                size: md.len(),
                modified: md.modified().map_or(0, unix),
                path,
            });
        }
    }
}

/// Every clip and screenshot in `root` (and its per-app folders), newest first.
pub fn list(root: &Path) -> Vec<ClipEntry> {
    let mut out = Vec::new();
    scan_dir(root, "", &mut out, true);
    out.sort_by(|a, b| b.modified.cmp(&a.modified).then_with(|| a.name.cmp(&b.name)));
    out
}

/// The canonical path of a library file, or why it is refused. The file must exist, sit
/// inside `root` once links are resolved, and be a clip or screenshot type.
pub fn resolve(root: &Path, path: &Path) -> Result<PathBuf, String> {
    let root = root
        .canonicalize()
        .map_err(|_| "the clips folder does not exist yet".to_string())?;
    let real = path
        .canonicalize()
        .map_err(|_| "the file no longer exists".to_string())?;
    if !real.starts_with(&root) {
        return Err("that file is not in the clips folder".into());
    }
    let plain_name = real
        .file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| !n.contains(':'));
    if !real.is_file() || kind_of(&real).is_none() || !plain_name {
        return Err("that is not a clip".into());
    }
    Ok(real)
}

/// Remove a library file: to the Recycle Bin, or for good when `permanent` is set (the
/// user chose that in the confirmation dialog).
pub fn delete(root: &Path, path: &Path, permanent: bool) -> Result<(), String> {
    let real = resolve(root, path)?;
    if permanent {
        std::fs::remove_file(&real).map_err(|e| format!("could not delete the file: {e}"))
    } else {
        recycle(&real)
    }
}

/// Without the `\\?\` prefix canonicalize adds; the shell mishandles it.
fn shell_path(path: &Path) -> std::ffi::OsString {
    let text = path.as_os_str().to_string_lossy();
    match text.strip_prefix(r"\\?\") {
        Some(rest) if !rest.starts_with("UNC\\") => rest.into(),
        _ => path.as_os_str().to_owned(),
    }
}

/// Recycle Bin only: `FOFX_RECYCLEONDELETE` makes the shell fail instead of deleting for
/// good (Recycle Bin off, file too big for it, network or removable drive).
fn recycle(path: &Path) -> Result<(), String> {
    use windows::{
        Win32::{
            System::Com::{CLSCTX_ALL, CoCreateInstance},
            UI::Shell::{
                FILEOPERATION_FLAGS, FOF_ALLOWUNDO, FOF_NOCONFIRMATION, FOF_NOERRORUI, FOF_SILENT,
                FOFX_RECYCLEONDELETE, FileOperation, IFileOperation, IShellItem, SHCreateItemFromParsingName,
            },
        },
        core::HSTRING,
    };
    crate::capture::com_init();
    let run = || -> windows::core::Result<bool> {
        // SAFETY: COM calls on locals; the operation is performed once and then dropped.
        unsafe {
            let op: IFileOperation = CoCreateInstance(&FileOperation, None, CLSCTX_ALL)?;
            op.SetOperationFlags(FILEOPERATION_FLAGS(
                FOF_ALLOWUNDO.0 | FOF_NOCONFIRMATION.0 | FOF_NOERRORUI.0 | FOF_SILENT.0 | FOFX_RECYCLEONDELETE.0,
            ))?;
            let item: IShellItem = SHCreateItemFromParsingName(&HSTRING::from(shell_path(path)), None)?;
            op.DeleteItem(&item, None)?;
            op.PerformOperations()?;
            Ok(op.GetAnyOperationsAborted()?.as_bool())
        }
    };
    match run() {
        Ok(false) if !path.exists() => Ok(()),
        Ok(_) => Err("the file could not be moved to the Recycle Bin".into()),
        Err(e) => Err(format!("could not move the file to the Recycle Bin: {}", e.message())),
    }
}

/// Reserves `<stem>_trim.<ext>` next to `path` (numbered if taken) by creating it empty, so
/// two trims never pick the same name; the finished copy replaces the placeholder.
fn reserve_trim_target(path: &Path) -> std::io::Result<PathBuf> {
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let ext = path
        .extension()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let dir = path.parent().unwrap_or(Path::new(""));
    let mut n = 1;
    loop {
        let out = if n == 1 {
            dir.join(format!("{stem}_trim.{ext}"))
        } else {
            dir.join(format!("{stem}_trim-{n}.{ext}"))
        };
        match std::fs::OpenOptions::new().write(true).create_new(true).open(&out) {
            Ok(_) => return Ok(out),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists && n < 1000 => n += 1,
            Err(e) => return Err(e),
        }
    }
}

/// Length of a video in seconds, when the file says.
pub fn duration_secs(root: &Path, path: &Path) -> Result<Option<f64>, String> {
    let real = resolve(root, path)?;
    if kind_of(&real) != Some(Kind::Video) {
        return Err("only videos have a length".into());
    }
    mux::duration_us(&real)
        .map(|d| d.map(|us| us as f64 / 1e6))
        .map_err(|e| e.to_string())
}

/// Lossless copy of `[start_s, end_s]` as a new file beside the original, which stays
/// untouched. Returns the new file's path.
pub fn trim(root: &Path, path: &Path, start_s: f64, end_s: f64) -> Result<PathBuf, String> {
    let real = resolve(root, path)?;
    if kind_of(&real) != Some(Kind::Video) {
        return Err("only videos can be trimmed".into());
    }
    if !(start_s.is_finite() && end_s.is_finite()) || start_s < 0.0 || end_s - start_s < 0.5 {
        return Err("choose an end at least half a second after the start".into());
    }
    if end_s - start_s > MAX_TRIM_SECS || end_s > 2.0 * MAX_TRIM_SECS {
        return Err("that range is too long".into());
    }
    let out = reserve_trim_target(&real).map_err(|e| format!("could not create the trimmed file: {e}"))?;
    match mux::trim(&real, &out, (start_s * 1e6) as i64, (end_s * 1e6) as i64) {
        Ok(_) => Ok(out),
        Err(e) => {
            let _ = std::fs::remove_file(&out); // the empty placeholder
            Err(e.to_string())
        }
    }
}

/// `<App>_YYYY-MM-DD_HH-MM-SS` in the name: how SuiCap names what it saves. The quota never
/// touches anything else, so pointing the clips folder at Videos cannot cost other files.
fn is_suicap_name(name: &str) -> bool {
    let b = name.as_bytes();
    const SHAPE: &[u8; 19] = b"dddd-dd-dd_dd-dd-dd";
    b.windows(SHAPE.len()).any(|w| {
        w.iter()
            .zip(SHAPE)
            .all(|(c, s)| if *s == b'd' { c.is_ascii_digit() } else { c == s })
    })
}

/// Delete the oldest files until the library fits in `limit_bytes`. Files changed in the
/// last two minutes are kept. Returns what was removed. This is the one permanent delete
/// here: its purpose is to free disk space, and it is off unless the user sets a limit.
pub fn enforce_quota(root: &Path, limit_bytes: u64) -> Vec<PathBuf> {
    let mut entries = list(root);
    entries.retain(|e| is_suicap_name(&e.name));
    let mut total: u64 = entries.iter().map(|e| e.size).sum();
    let now = unix(SystemTime::now());
    entries.sort_by_key(|e| e.modified); // oldest first
    let mut removed = Vec::new();
    for e in entries {
        if total <= limit_bytes {
            break;
        }
        if now.saturating_sub(e.modified) < QUOTA_GRACE.as_secs() {
            continue;
        }
        let Ok(real) = resolve(root, &e.path) else { continue };
        if std::fs::remove_file(&real).is_ok() {
            total = total.saturating_sub(e.size);
            removed.push(real);
        }
    }
    removed
}

/// A thumbnail from the Windows shell (the same one Explorer shows), as base64 PNG.
/// Fails when no codec is installed for the file; the UI then shows a placeholder.
pub fn thumbnail(root: &Path, path: &Path, size: u32) -> Result<String, String> {
    use windows::{
        Win32::{
            Foundation::SIZE,
            Graphics::Gdi::{
                BITMAP, BITMAPINFO, BITMAPINFOHEADER, DIB_RGB_COLORS, DeleteObject, GetDC, GetDIBits, GetObjectW,
                HGDIOBJ, ReleaseDC,
            },
            UI::Shell::{
                IShellItemImageFactory, SHCreateItemFromParsingName, SIIGBF_BIGGERSIZEOK, SIIGBF_THUMBNAILONLY,
            },
        },
        core::HSTRING,
    };
    let real = resolve(root, path)?;
    let size = size.clamp(32, 512) as i32;
    crate::capture::com_init();
    // SAFETY: shell and GDI calls on locals; the HBITMAP is deleted on every path.
    let (w, h, bgra) = unsafe {
        let factory: IShellItemImageFactory =
            SHCreateItemFromParsingName(&HSTRING::from(shell_path(&real)), None).map_err(|e| e.message())?;
        let hbm = factory
            .GetImage(SIZE { cx: size, cy: size }, SIIGBF_BIGGERSIZEOK | SIIGBF_THUMBNAILONLY)
            .map_err(|_| "no thumbnail available".to_string())?;
        let mut bm = BITMAP::default();
        let got = GetObjectW(
            HGDIOBJ(hbm.0),
            size_of::<BITMAP>() as i32,
            Some(std::ptr::from_mut(&mut bm).cast()),
        );
        let (w, h) = (bm.bmWidth, bm.bmHeight);
        if got == 0 || w <= 0 || h <= 0 || w > 2048 || h > 2048 {
            let _ = DeleteObject(HGDIOBJ(hbm.0));
            return Err("unusable thumbnail".into());
        }
        let mut info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w,
                biHeight: -h, // top-down
                biPlanes: 1,
                biBitCount: 32,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut buf = vec![0u8; (w * h * 4) as usize];
        let hdc = GetDC(None);
        let lines = GetDIBits(
            hdc,
            hbm,
            0,
            h as u32,
            Some(buf.as_mut_ptr().cast()),
            &mut info,
            DIB_RGB_COLORS,
        );
        ReleaseDC(None, hdc);
        let _ = DeleteObject(HGDIOBJ(hbm.0));
        if lines == 0 {
            return Err("unusable thumbnail".into());
        }
        (w as u32, h as u32, buf)
    };
    let mut rgba = bgra;
    for px in rgba.chunks_exact_mut(4) {
        px.swap(0, 2);
        px[3] = 255;
    }
    let (rgba, w, h) = shrink(&rgba, w, h, 320);
    let mut png_bytes = Vec::new();
    {
        let mut enc = png::Encoder::new(&mut png_bytes, w, h);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        enc.set_compression(png::Compression::Fast);
        let mut writer = enc.write_header().map_err(|e| e.to_string())?;
        writer.write_image_data(&rgba).map_err(|e| e.to_string())?;
        writer.finish().map_err(|e| e.to_string())?;
    }
    Ok(base64(&png_bytes))
}

/// Box-filter downscale by a whole factor so the width is at most `max_w`.
fn shrink(rgba: &[u8], w: u32, h: u32, max_w: u32) -> (Vec<u8>, u32, u32) {
    let k = w.div_ceil(max_w).max(1);
    if k == 1 {
        return (rgba.to_vec(), w, h);
    }
    let (nw, nh) = ((w / k).max(1), (h / k).max(1));
    let mut out = Vec::with_capacity((nw * nh * 4) as usize);
    for y in 0..nh {
        for x in 0..nw {
            let mut acc = [0u32; 4];
            for dy in 0..k {
                for dx in 0..k {
                    let i = (((y * k + dy) * w + x * k + dx) * 4) as usize;
                    for (a, v) in acc.iter_mut().zip(&rgba[i..i + 4]) {
                        *a += u32::from(*v);
                    }
                }
            }
            out.extend(acc.iter().map(|a| (a / (k * k)) as u8));
        }
    }
    (out, nw, nh)
}

/// The image type of `bytes` by its magic number: PNG, JPEG or WebP only (no SVG or GIF, so
/// a chosen file can never carry script or animation into the badge).
pub fn image_mime(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]) {
        Some("image/png")
    } else if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some("image/jpeg")
    } else if bytes.len() > 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Some("image/webp")
    } else {
        None
    }
}

pub fn base64(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for c in data.chunks(3) {
        let n =
            (u32::from(c[0]) << 16) | (u32::from(*c.get(1).unwrap_or(&0)) << 8) | u32::from(*c.get(2).unwrap_or(&0));
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        out.push(if c.len() > 1 {
            T[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if c.len() > 2 { T[n as usize & 63] as char } else { '=' });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("suicap-clips-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn file(path: &Path, bytes: usize, age_secs: u64) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, vec![0u8; bytes]).unwrap();
        let t = SystemTime::now() - Duration::from_secs(age_secs);
        std::fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(t)
            .unwrap();
    }

    #[test]
    fn lists_clips_newest_first_and_skips_other_files() {
        let d = temp("list");
        file(&d.join("Game/a.mp4"), 10, 500);
        file(&d.join("Game/b.png"), 5, 100);
        file(&d.join("Game/c.mp4.tmp"), 5, 100);
        file(&d.join("notes.txt"), 5, 100);
        file(&d.join("top.mkv"), 7, 900);
        let names: Vec<_> = list(&d).into_iter().map(|e| (e.name, e.app)).collect();
        assert_eq!(
            names,
            [
                ("b.png".into(), "Game".into()),
                ("a.mp4".into(), "Game".into()),
                ("top.mkv".into(), "".into())
            ]
        );
        std::fs::remove_dir_all(d).unwrap();
    }

    #[test]
    fn resolve_confines_paths_to_the_clips_folder() {
        let d = temp("resolve");
        let outside = temp("outside");
        file(&d.join("Game/a.mp4"), 1, 10);
        file(&outside.join("evil.mp4"), 1, 10);
        file(&d.join("Game/readme.txt"), 1, 10);
        assert!(resolve(&d, &d.join("Game/a.mp4")).is_ok());
        assert!(resolve(&d, &d.join("Game/../Game/a.mp4")).is_ok());
        assert!(resolve(&d, &outside.join("evil.mp4")).is_err());
        assert!(
            resolve(
                &d,
                &d.join("Game/../../")
                    .join(outside.file_name().unwrap())
                    .join("evil.mp4")
            )
            .is_err()
        );
        assert!(resolve(&d, &d.join("Game/readme.txt")).is_err());
        assert!(resolve(&d, &d.join("Game")).is_err());
        assert!(resolve(&d, &d.join("missing.mp4")).is_err());
        assert!(resolve(&d, &d.join("Game/a.mp4:stream.mp4")).is_err());
        std::fs::remove_dir_all(d).unwrap();
        std::fs::remove_dir_all(outside).unwrap();
    }

    #[test]
    fn quota_removes_oldest_first_and_spares_recent_files() {
        let d = temp("quota");
        file(&d.join("G/G_2026-01-01_10-00-00.mp4"), 100, 10_000);
        file(&d.join("G/G_2026-01-02_10-00-00.mp4"), 100, 5_000);
        file(&d.join("G/holiday.mp4"), 500, 20_000); // not SuiCap-named: never deleted
        file(&d.join("G/G_2026-01-03_10-00-00.mp4"), 100, 5);
        let removed = enforce_quota(&d, 250);
        assert_eq!(removed.len(), 1);
        assert!(removed[0].ends_with("G_2026-01-01_10-00-00.mp4"));
        // Still over the limit, but the newest is protected and mid is next.
        let removed = enforce_quota(&d, 10);
        assert_eq!(removed.len(), 1);
        assert!(d.join("G/G_2026-01-03_10-00-00.mp4").exists() && !d.join("G/G_2026-01-02_10-00-00.mp4").exists());
        assert!(d.join("G/holiday.mp4").exists());
        std::fs::remove_dir_all(d).unwrap();
    }

    #[test]
    fn trim_rejects_bad_ranges_and_non_videos() {
        let d = temp("trim");
        file(&d.join("G/a.mp4"), 1, 10);
        file(&d.join("G/s.png"), 1, 10);
        let a = d.join("G/a.mp4");
        assert!(trim(&d, &a, 5.0, 5.2).is_err());
        assert!(trim(&d, &a, -1.0, 5.0).is_err());
        assert!(trim(&d, &a, f64::NAN, 5.0).is_err());
        assert!(trim(&d, &a, 0.0, 1e9).is_err());
        assert!(trim(&d, &d.join("G/s.png"), 0.0, 5.0).is_err());
        assert!(trim(&d, &d.join("../x.mp4"), 0.0, 5.0).is_err());
        std::fs::remove_dir_all(d).unwrap();
    }

    #[test]
    fn trim_target_is_reserved_and_numbered() {
        let d = temp("target");
        file(&d.join("a.mp4"), 1, 10);
        let first = reserve_trim_target(&d.join("a.mp4")).unwrap();
        assert!(first.ends_with("a_trim.mp4") && first.exists());
        let second = reserve_trim_target(&d.join("a.mp4")).unwrap();
        assert!(second.ends_with("a_trim-2.mp4"));
        std::fs::remove_dir_all(d).unwrap();
    }

    #[test]
    fn shrink_halves_and_averages() {
        let px = [
            [0u8, 0, 0, 255],
            [100, 100, 100, 255],
            [100, 100, 100, 255],
            [200, 200, 200, 255],
        ]
        .concat();
        let (out, w, h) = shrink(&px, 2, 2, 1);
        assert_eq!((w, h, out), (1, 1, vec![100, 100, 100, 255]));
    }

    #[test]
    fn image_mime_accepts_only_png_jpeg_webp() {
        assert_eq!(
            image_mime(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 0]),
            Some("image/png")
        );
        assert_eq!(image_mime(&[0xFF, 0xD8, 0xFF, 0xE0]), Some("image/jpeg"));
        assert_eq!(image_mime(b"RIFF\x10\0\0\0WEBPVP8 "), Some("image/webp"));
        assert_eq!(image_mime(b"RIFF\x10\0\0\0WAVEfmt "), None);
        assert_eq!(image_mime(b"<svg xmlns='http://www.w3.org/2000/svg'/>"), None);
        assert_eq!(image_mime(b"GIF89a"), None);
        assert_eq!(image_mime(b""), None);
    }

    #[test]
    fn base64_matches_known_vectors() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
    }

    /// Manual check against real clips: `SUICAP_CLIPS=<dir> cargo test -p suicap-core thumbs -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn thumbs_of_real_clips() {
        let root = PathBuf::from(std::env::var("SUICAP_CLIPS").expect("set SUICAP_CLIPS"));
        for c in list(&root) {
            let r = thumbnail(&root, &c.path, 256);
            println!(
                "{} -> {}",
                c.name,
                r.as_ref()
                    .map_or_else(|e| format!("ERR {e}"), |b| format!("{} b64 chars", b.len()))
            );
        }
    }
}
