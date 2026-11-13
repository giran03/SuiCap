//! Packets -> .mkv/.mp4 through libavformat (plan §6.2). FFI module.
//!
//! Writes to `<name>.tmp` and renames on success, so a crash never leaves a
//! half-written file under the final name. Timestamps are rebased so the first packet
//! (a video keyframe) starts at 0.

use std::{
    ffi::CString,
    path::{Path, PathBuf},
    ptr,
};

use ffmpeg_sys_next as ffi;

use crate::{
    buffer::{Packet, VIDEO_STREAM},
    encode::StreamParams,
};

#[derive(Debug, thiserror::Error)]
pub enum MuxError {
    #[error("{call} failed: {msg} ({code})")]
    Ffmpeg { call: &'static str, code: i32, msg: String },
    #[error("unsupported output path {0}: use .mkv or .mp4 (UTF-8)")]
    Path(PathBuf),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, MuxError>;

fn check(call: &'static str, code: i32) -> Result<i32> {
    if code >= 0 {
        Ok(code)
    } else {
        Err(MuxError::Ffmpeg {
            call,
            code,
            msg: crate::encode::av_err(code),
        })
    }
}

const US: ffi::AVRational = ffi::AVRational { num: 1, den: 1_000_000 };

/// One output stream: codec parameters plus track name/default flag (plan §5).
#[derive(Clone)]
pub struct MuxStream {
    pub params: std::sync::Arc<StreamParams>,
    pub title: Option<&'static str>,
    pub default: bool,
}

impl MuxStream {
    pub fn video(params: std::sync::Arc<StreamParams>) -> Self {
        Self {
            params,
            title: None,
            default: true,
        }
    }
}

pub struct Muxer {
    fmt: *mut ffi::AVFormatContext,
    /// Per-stream time base after the header (the muxer may change it).
    tb: Vec<ffi::AVRational>,
    rebase: Rebaser,
    tmp: PathBuf,
    path: PathBuf,
    pub packets: u64,
}

impl Muxer {
    /// `streams[i]` receives packets with `Packet::stream == i`.
    pub fn create(path: &Path, streams: &[MuxStream]) -> Result<Self> {
        let ext = path.extension().and_then(|e| e.to_str()).map(str::to_ascii_lowercase);
        let format = match ext.as_deref() {
            Some("mkv") => c"matroska",
            Some("mp4") => c"mp4",
            _ => return Err(MuxError::Path(path.to_path_buf())),
        };
        let mut tmp = path.as_os_str().to_owned();
        tmp.push(".tmp");
        let tmp = PathBuf::from(tmp);
        let ctmp = tmp
            .to_str()
            .and_then(|s| CString::new(s).ok())
            .ok_or_else(|| MuxError::Path(path.into()))?;

        let mut fmt = ptr::null_mut();
        // SAFETY: standard libavformat setup; `fmt` is owned by Muxer from here on, so
        // every early return frees it in Drop.
        unsafe {
            check(
                "avformat_alloc_output_context2",
                ffi::avformat_alloc_output_context2(&mut fmt, ptr::null(), format.as_ptr(), ctmp.as_ptr()),
            )?;
            let mut mux = Muxer {
                fmt,
                tb: Vec::new(),
                rebase: Rebaser::default(),
                tmp,
                path: path.to_path_buf(),
                packets: 0,
            };
            let mp4 = format == c"mp4";
            let mut created = Vec::with_capacity(streams.len());
            for desc in streams {
                let st = ffi::avformat_new_stream(fmt, ptr::null());
                if st.is_null() {
                    return Err(MuxError::Ffmpeg {
                        call: "avformat_new_stream",
                        code: -1,
                        msg: "NULL".into(),
                    });
                }
                let par = (*st).codecpar;
                check(
                    "avcodec_parameters_copy",
                    ffi::avcodec_parameters_copy(par, desc.params.as_ptr()),
                )?;
                if (*par).codec_type == ffi::AVMediaType::AVMEDIA_TYPE_AUDIO {
                    (*st).time_base = ffi::AVRational {
                        num: 1,
                        den: (*par).sample_rate.max(1),
                    };
                } else {
                    (*st).time_base = US;
                    (*st).avg_frame_rate = ffi::AVRational {
                        num: desc.params.fps as i32,
                        den: 1,
                    };
                }
                if mp4 && (*par).codec_id == ffi::AVCodecID::AV_CODEC_ID_HEVC {
                    // Tag HEVC as hvc1 so Apple players and browsers accept it (plan §6.2).
                    (*par).codec_tag = u32::from_le_bytes(*b"hvc1");
                }
                if let Some(title) = desc.title {
                    let t = CString::new(title).expect("static titles have no NUL");
                    // mkv reads "title"; mp4 writes the track name from "handler_name".
                    ffi::av_dict_set(&mut (*st).metadata, c"title".as_ptr(), t.as_ptr(), 0);
                    ffi::av_dict_set(&mut (*st).metadata, c"handler_name".as_ptr(), t.as_ptr(), 0);
                }
                (*st).disposition = if desc.default { ffi::AV_DISPOSITION_DEFAULT } else { 0 };
                created.push(st);
            }
            if (*(*fmt).oformat).flags & ffi::AVFMT_NOFILE == 0 {
                check(
                    "avio_open",
                    ffi::avio_open(&mut (*fmt).pb, ctmp.as_ptr(), ffi::AVIO_FLAG_WRITE),
                )?;
            }
            let mut opts: *mut ffi::AVDictionary = ptr::null_mut();
            if mp4 {
                ffi::av_dict_set(&mut opts, c"movflags".as_ptr(), c"+faststart".as_ptr(), 0);
            }
            let ret = ffi::avformat_write_header(fmt, &mut opts);
            ffi::av_dict_free(&mut opts);
            check("avformat_write_header", ret)?;
            mux.tb = created.iter().map(|&st| (*st).time_base).collect(); // mkv: 1/1000
            Ok(mux)
        }
    }

    pub fn write(&mut self, p: &Packet) -> Result<()> {
        let Some((pts, dts)) = self.rebase.apply(p) else {
            return Ok(()); // before the first video keyframe: undecodable, skip
        };
        let Some(&tb) = self.tb.get(usize::from(p.stream)) else {
            return Ok(()); // stream not part of this file
        };
        // SAFETY: packet allocated here and handed to the muxer, which takes the ref.
        unsafe {
            let mut pkt = ffi::av_packet_alloc();
            if pkt.is_null() {
                return Err(MuxError::Ffmpeg {
                    call: "av_packet_alloc",
                    code: -1,
                    msg: "NULL".into(),
                });
            }
            let len = i32::try_from(p.data.len()).map_err(|_| MuxError::Ffmpeg {
                call: "av_new_packet",
                code: -1,
                msg: "packet payload exceeds i32::MAX".into(),
            })?;
            let r = (|| {
                check("av_new_packet", ffi::av_new_packet(pkt, len))?;
                ptr::copy_nonoverlapping(p.data.as_ptr(), (*pkt).data, p.data.len());
                (*pkt).pts = ffi::av_rescale_q(pts, US, tb);
                (*pkt).dts = ffi::av_rescale_q(dts, US, tb);
                (*pkt).stream_index = i32::from(p.stream);
                if p.key {
                    (*pkt).flags |= ffi::AV_PKT_FLAG_KEY;
                }
                check(
                    "av_interleaved_write_frame",
                    ffi::av_interleaved_write_frame(self.fmt, pkt),
                )
            })();
            ffi::av_packet_free(&mut pkt);
            r?;
        }
        self.packets += 1;
        Ok(())
    }

    /// Write the trailer, close, and move the file to its final name.
    pub fn finish(mut self) -> Result<PathBuf> {
        // SAFETY: header was written; trailer + close on a live context.
        unsafe {
            check("av_write_trailer", ffi::av_write_trailer(self.fmt))?;
            self.close();
        }
        if let Err(e) = replace_file(&self.tmp, &self.path) {
            let _ = std::fs::remove_file(&self.tmp);
            return Err(e.into());
        }
        Ok(self.path.clone())
    }

    /// SAFETY: call at most once per open context (Drop and finish both guard via NULL).
    unsafe fn close(&mut self) {
        if self.fmt.is_null() {
            return;
        }
        // SAFETY: fmt is live; pb may be NULL for NOFILE formats, which avio_closep allows.
        unsafe {
            if (*(*self.fmt).oformat).flags & ffi::AVFMT_NOFILE == 0 {
                ffi::avio_closep(&mut (*self.fmt).pb);
            }
            ffi::avformat_free_context(self.fmt);
        }
        self.fmt = ptr::null_mut();
    }
}

impl Drop for Muxer {
    fn drop(&mut self) {
        if !self.fmt.is_null() {
            // Not finished: close and discard the partial file.
            // SAFETY: single close guarded by the NULL check.
            unsafe { self.close() };
            let _ = std::fs::remove_file(&self.tmp);
        }
    }
}

/// Shifts timestamps so the first video keyframe lands at 0 (plan §6.2). Packets before
/// it (and audio older than it) are dropped, so the file starts decodable.
#[derive(Debug, Default)]
pub struct Rebaser {
    base_us: Option<i64>,
}

impl Rebaser {
    /// Rebased `(pts, dts)` in microseconds, or `None` to skip the packet.
    pub fn apply(&mut self, p: &Packet) -> Option<(i64, i64)> {
        if self.base_us.is_none() && p.stream == VIDEO_STREAM && p.key {
            self.base_us = Some(p.pts_us);
        }
        let base = self.base_us?;
        (p.pts_us >= base).then(|| (p.pts_us - base, p.dts_us - base))
    }
}

/// Replace `to` with `from` (same folder). `std::fs::rename` can fail with
/// ERROR_NOT_SAME_DEVICE over an existing file when a filesystem filter sits on the
/// volume (seen under AppData\Roaming); `MoveFileExW` may then copy, write-through.
pub fn replace_file(from: &Path, to: &Path) -> std::io::Result<()> {
    match std::fs::rename(from, to) {
        Err(e) if e.kind() == std::io::ErrorKind::CrossesDevices => {
            use std::os::windows::ffi::OsStrExt;
            use windows::Win32::Storage::FileSystem::{
                MOVEFILE_COPY_ALLOWED, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
            };
            let wide = |p: &Path| p.as_os_str().encode_wide().chain([0]).collect::<Vec<u16>>();
            let (f, t) = (wide(from), wide(to));
            // SAFETY: both buffers are NUL-terminated UTF-16 paths that outlive the call.
            unsafe {
                MoveFileExW(
                    windows::core::PCWSTR(f.as_ptr()),
                    windows::core::PCWSTR(t.as_ptr()),
                    MOVEFILE_REPLACE_EXISTING | MOVEFILE_COPY_ALLOWED | MOVEFILE_WRITE_THROUGH,
                )
            }
            .map_err(|e| std::io::Error::from_raw_os_error(e.code().0 & 0xFFFF))
        }
        other => other,
    }
}

/// The app name made filesystem-safe; "Desktop" when nothing is left.
pub fn safe_app_name(app: &str) -> String {
    const RESERVED: &[&str] = &[
        "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8", "COM9", "LPT1",
        "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
    ];
    let safe: String = app
        .chars()
        .map(|c| {
            if c.is_control() || r#"<>:"/\|?*"#.contains(c) {
                '_'
            } else {
                c
            }
        })
        .collect::<String>()
        .trim_matches(|c: char| c == '.' || c.is_whitespace())
        .chars()
        .take(64)
        .collect();
    if safe.is_empty() {
        "Desktop".into()
    } else if RESERVED.iter().any(|r| r.eq_ignore_ascii_case(&safe)) {
        format!("_{safe}")
    } else {
        safe
    }
}

/// `<App>_<YYYY-MM-DD_HH-mm-ss>.<ext>` with the app name made filesystem-safe (plan §6.2).
pub fn clip_file_name(app: &str, t: [u16; 6], ext: &str) -> String {
    let safe = safe_app_name(app);
    let [y, mo, d, h, mi, s] = t;
    format!("{safe}_{y:04}-{mo:02}-{d:02}_{h:02}-{mi:02}-{s:02}.{ext}")
}

/// Local wall-clock time as `[year, month, day, hour, minute, second]`.
pub fn local_time() -> [u16; 6] {
    // SAFETY: GetLocalTime only fills a returned struct.
    let t = unsafe { windows::Win32::System::SystemInformation::GetLocalTime() };
    [t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond]
}

/// What a finished save produced.
#[derive(Debug, Clone)]
pub struct ClipInfo {
    pub path: PathBuf,
    pub packets: u64,
    pub first_pts_us: i64,
    pub last_pts_us: i64,
}

/// Mux worker spawned per save (plan §3): writes a snapshot while capture continues.
/// The snapshot holds `Bytes` handles only, so nothing is copied until the write.
pub fn spawn_save(
    path: PathBuf,
    streams: Vec<MuxStream>,
    packets: Vec<Packet>,
) -> std::thread::JoinHandle<Result<ClipInfo>> {
    std::thread::Builder::new()
        .name("suicap-mux".into())
        .spawn(move || {
            let mut mux = Muxer::create(&path, &streams)?;
            let mut rb = Rebaser::default();
            let (mut first, mut last) = (None, 0);
            for p in &packets {
                if let Some((pts, _)) = rb.apply(p).filter(|_| p.stream == VIDEO_STREAM) {
                    first.get_or_insert(pts);
                    last = last.max(pts);
                }
                mux.write(p)?;
            }
            let packets = mux.packets;
            let path = mux.finish()?;
            Ok(ClipInfo {
                path,
                packets,
                first_pts_us: first.unwrap_or(0),
                last_pts_us: last,
            })
        })
        .expect("spawning the mux thread")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::buffer::ReplayBuffer;
    use bytes::Bytes;

    fn pkt(stream: u8, key: bool, pts_us: i64) -> Packet {
        Packet {
            stream,
            key,
            pts_us,
            dts_us: pts_us,
            data: Bytes::from_static(b"x"),
        }
    }

    #[test]
    fn rebase_starts_at_first_video_keyframe() {
        let mut rb = Rebaser::default();
        assert_eq!(rb.apply(&pkt(1, true, 100)), None, "audio before video key");
        assert_eq!(rb.apply(&pkt(0, false, 200)), None, "delta frame before key");
        assert_eq!(rb.apply(&pkt(0, true, 5_000)), Some((0, 0)));
        assert_eq!(rb.apply(&pkt(1, true, 4_000)), None, "audio older than base");
        assert_eq!(rb.apply(&pkt(0, false, 21_666)), Some((16_666, 16_666)));
    }

    #[test]
    fn snapshot_then_rebase_starts_at_zero_on_keyframe() {
        let buf = ReplayBuffer::new(60, usize::MAX);
        for f in 0..600i64 {
            let pts = 7_000_000 + f * 1_000_000 / 60;
            buf.push(pkt(0, f % 60 == 0, pts));
        }
        let snap = buf.snapshot(5);
        let mut rb = Rebaser::default();
        let times: Vec<_> = snap.iter().filter_map(|p| rb.apply(p)).collect();
        assert_eq!(times.len(), snap.len());
        assert_eq!(times[0], (0, 0));
        assert!(snap[0].key);
        assert!(times.windows(2).all(|w| w[1].0 > w[0].0));
        let len = times.last().unwrap().0;
        assert!((5_000_000..6_000_000).contains(&len), "len {len}");
    }

    #[test]
    fn clip_names_are_safe() {
        let t = [2026, 11, 3, 9, 5, 7];
        assert_eq!(clip_file_name("Desktop", t, "mp4"), "Desktop_2026-11-03_09-05-07.mp4");
        assert_eq!(
            clip_file_name(r#"a<b>:c"/\|?*"#, t, "mkv"),
            "a_b__c_______2026-11-03_09-05-07.mkv"
        );
        assert_eq!(clip_file_name(" .. ", t, "mp4"), "Desktop_2026-11-03_09-05-07.mp4");
        assert_eq!(safe_app_name("con"), "_con");
        assert_eq!(safe_app_name("NUL."), "_NUL");
        assert_eq!(safe_app_name("com1"), "_com1");
    }
}
