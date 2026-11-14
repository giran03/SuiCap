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
    pkt: *mut ffi::AVPacket,
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
            let pkt = ffi::av_packet_alloc();
            let mut mux = Muxer {
                fmt,
                pkt,
                tb: Vec::new(),
                rebase: Rebaser::default(),
                tmp,
                path: path.to_path_buf(),
                packets: 0,
            };
            if pkt.is_null() {
                return Err(MuxError::Ffmpeg {
                    call: "av_packet_alloc",
                    code: -1,
                    msg: "NULL".into(),
                });
            }
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
        // SAFETY: self.pkt is a reusable owned AVPacket; av_packet_unref always runs after the call.
        unsafe {
            let len = i32::try_from(p.data.len()).map_err(|_| MuxError::Ffmpeg {
                call: "av_new_packet",
                code: -1,
                msg: "packet payload exceeds i32::MAX".into(),
            })?;
            let r = (|| {
                check("av_new_packet", ffi::av_new_packet(self.pkt, len))?;
                ptr::copy_nonoverlapping(p.data.as_ptr(), (*self.pkt).data, p.data.len());
                (*self.pkt).pts = ffi::av_rescale_q(pts, US, tb);
                (*self.pkt).dts = ffi::av_rescale_q(dts, US, tb);
                (*self.pkt).stream_index = i32::from(p.stream);
                (*self.pkt).flags = if p.key { ffi::AV_PKT_FLAG_KEY } else { 0 };
                check(
                    "av_interleaved_write_frame",
                    ffi::av_interleaved_write_frame(self.fmt, self.pkt),
                )
            })();
            ffi::av_packet_unref(self.pkt);
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
        // SAFETY: we own pkt; av_packet_free accepts NULL.
        unsafe { ffi::av_packet_free(&mut self.pkt) };
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
        (p.pts_us >= base).then(|| (p.pts_us - base, p.dts_us() - base))
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

struct InFormat(*mut ffi::AVFormatContext);

impl Drop for InFormat {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: owned open input context.
            unsafe { ffi::avformat_close_input(&mut self.0) };
        }
    }
}

struct OutFormat {
    fmt: *mut ffi::AVFormatContext,
    tmp: PathBuf,
    finished: bool,
}

impl OutFormat {
    unsafe fn close(&mut self) {
        if self.fmt.is_null() {
            return;
        }
        // SAFETY: `fmt` is live; `avio_closep` accepts NULL `pb`.
        unsafe {
            if (*(*self.fmt).oformat).flags & ffi::AVFMT_NOFILE == 0 {
                ffi::avio_closep(&mut (*self.fmt).pb);
            }
            ffi::avformat_free_context(self.fmt);
        }
        self.fmt = ptr::null_mut();
    }
}

impl Drop for OutFormat {
    fn drop(&mut self) {
        if !self.fmt.is_null() {
            // SAFETY: single close guarded by NULL check.
            unsafe { self.close() };
        }
        if !self.finished {
            let _ = std::fs::remove_file(&self.tmp);
        }
    }
}

struct AvPacket(*mut ffi::AVPacket);

impl Drop for AvPacket {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: owned packet.
            unsafe { ffi::av_packet_free(&mut self.0) };
        }
    }
}

/// Stream-copy remux from `.mkv` to `.mp4` (plan §6.3).
///
/// Copies all streams and codec parameters, tags HEVC as `hvc1`, enables
/// `movflags=+faststart`, writes to `<mp4>.tmp` and atomically replaces `mp4`.
/// Cleans up `.tmp` on any error.
pub fn remux_to_mp4(mkv: &Path, mp4: &Path) -> Result<()> {
    let cmkv = mkv
        .to_str()
        .and_then(|s| CString::new(s).ok())
        .ok_or_else(|| MuxError::Path(mkv.to_path_buf()))?;
    let mut tmp = mp4.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    let ctmp = tmp
        .to_str()
        .and_then(|s| CString::new(s).ok())
        .ok_or_else(|| MuxError::Path(mp4.to_path_buf()))?;

    // SAFETY: standard libavformat remux; RAII guards close contexts and remove `.tmp` on error.
    unsafe {
        let mut in_ptr = ptr::null_mut();
        check(
            "avformat_open_input",
            ffi::avformat_open_input(&mut in_ptr, cmkv.as_ptr(), ptr::null(), ptr::null_mut()),
        )?;
        let in_ctx = InFormat(in_ptr);
        check(
            "avformat_find_stream_info",
            ffi::avformat_find_stream_info(in_ctx.0, ptr::null_mut()),
        )?;

        let mut out_ptr = ptr::null_mut();
        check(
            "avformat_alloc_output_context2",
            ffi::avformat_alloc_output_context2(&mut out_ptr, ptr::null(), c"mp4".as_ptr(), ctmp.as_ptr()),
        )?;
        let mut out_ctx = OutFormat {
            fmt: out_ptr,
            tmp: tmp.clone(),
            finished: false,
        };

        let nb_streams = (*in_ctx.0).nb_streams as usize;
        for i in 0..nb_streams {
            let in_st = *(*in_ctx.0).streams.add(i);
            let out_st = ffi::avformat_new_stream(out_ctx.fmt, ptr::null());
            if out_st.is_null() {
                return Err(MuxError::Ffmpeg {
                    call: "avformat_new_stream",
                    code: -1,
                    msg: "NULL".into(),
                });
            }
            check(
                "avcodec_parameters_copy",
                ffi::avcodec_parameters_copy((*out_st).codecpar, (*in_st).codecpar),
            )?;
            // Let the MP4 muxer choose standard tags, except HEVC which must be `hvc1`.
            (*(*out_st).codecpar).codec_tag = if (*(*out_st).codecpar).codec_id == ffi::AVCodecID::AV_CODEC_ID_HEVC {
                u32::from_le_bytes(*b"hvc1")
            } else {
                0
            };
            (*out_st).time_base = (*in_st).time_base;
            (*out_st).avg_frame_rate = (*in_st).avg_frame_rate;
            (*out_st).disposition = (*in_st).disposition;
            ffi::av_dict_copy(&mut (*out_st).metadata, (*in_st).metadata, 0);
        }

        if (*(*out_ctx.fmt).oformat).flags & ffi::AVFMT_NOFILE == 0 {
            check(
                "avio_open",
                ffi::avio_open(&mut (*out_ctx.fmt).pb, ctmp.as_ptr(), ffi::AVIO_FLAG_WRITE),
            )?;
        }

        let mut opts: *mut ffi::AVDictionary = ptr::null_mut();
        ffi::av_dict_set(&mut opts, c"movflags".as_ptr(), c"+faststart".as_ptr(), 0);
        let ret = ffi::avformat_write_header(out_ctx.fmt, &mut opts);
        ffi::av_dict_free(&mut opts);
        check("avformat_write_header", ret)?;

        let pkt_raw = ffi::av_packet_alloc();
        if pkt_raw.is_null() {
            return Err(MuxError::Ffmpeg {
                call: "av_packet_alloc",
                code: -1,
                msg: "NULL".into(),
            });
        }
        let pkt = AvPacket(pkt_raw);

        loop {
            let ret = ffi::av_read_frame(in_ctx.0, pkt.0);
            if ret == ffi::AVERROR_EOF {
                break;
            }
            check("av_read_frame", ret)?;
            let idx = (*pkt.0).stream_index as usize;
            if idx < (*in_ctx.0).nb_streams as usize && idx < (*out_ctx.fmt).nb_streams as usize {
                let in_tb = (*(*(*in_ctx.0).streams.add(idx))).time_base;
                let out_tb = (*(*(*out_ctx.fmt).streams.add(idx))).time_base;
                ffi::av_packet_rescale_ts(pkt.0, in_tb, out_tb);
                (*pkt.0).pos = -1;
                let w = ffi::av_interleaved_write_frame(out_ctx.fmt, pkt.0);
                ffi::av_packet_unref(pkt.0);
                check("av_interleaved_write_frame", w)?;
            } else {
                ffi::av_packet_unref(pkt.0);
            }
        }

        check("av_write_trailer", ffi::av_write_trailer(out_ctx.fmt))?;
        out_ctx.close();
        drop(in_ctx);

        if let Err(e) = replace_file(&tmp, mp4) {
            return Err(e.into());
        }
        out_ctx.finished = true;
    }
    Ok(())
}

/// What a trim produced.
#[derive(Debug, Clone, Copy)]
pub struct TrimInfo {
    /// Where the copy really starts: the keyframe at or before the requested start.
    pub start_us: i64,
    pub duration_us: i64,
}

/// Length of a clip, or `None` when the container does not say (e.g. a crashed `.mkv`).
pub fn duration_us(path: &Path) -> Result<Option<i64>> {
    let cpath = path
        .to_str()
        .and_then(|s| CString::new(s).ok())
        .ok_or_else(|| MuxError::Path(path.to_path_buf()))?;
    // SAFETY: read-only probe; the RAII guard closes the input.
    unsafe {
        let in_ctx = open_media(&cpath)?;
        check(
            "avformat_find_stream_info",
            ffi::avformat_find_stream_info(in_ctx.0, ptr::null_mut()),
        )?;
        let d = (*in_ctx.0).duration;
        Ok((d != ffi::AV_NOPTS_VALUE && d > 0).then_some(d))
    }
}

/// Open `cpath` as a media file only: no playlist or concat formats, no network.
///
/// # Safety
/// `cpath` must be a valid C string.
unsafe fn open_media(cpath: &std::ffi::CStr) -> Result<InFormat> {
    // SAFETY: caller guarantees `cpath`; the dictionary is freed before returning.
    unsafe {
        let mut opts: *mut ffi::AVDictionary = ptr::null_mut();
        ffi::av_dict_set(
            &mut opts,
            c"format_whitelist".as_ptr(),
            c"mov,mp4,m4a,matroska,webm".as_ptr(),
            0,
        );
        ffi::av_dict_set(&mut opts, c"protocol_whitelist".as_ptr(), c"file".as_ptr(), 0);
        let mut in_ptr = ptr::null_mut();
        let ret = ffi::avformat_open_input(&mut in_ptr, cpath.as_ptr(), ptr::null(), &mut opts);
        ffi::av_dict_free(&mut opts);
        check("avformat_open_input", ret)?;
        Ok(InFormat(in_ptr))
    }
}

/// Packets held while looking for the start keyframe may not exceed this.
const MAX_HELD_BYTES: usize = 256 << 20;

fn no_video(msg: &str) -> MuxError {
    MuxError::Ffmpeg {
        call: "trim",
        code: -1,
        msg: msg.into(),
    }
}

/// Lossless trim: stream-copies `[start_us, end_us]` of `input` to `output` (`.mp4` or
/// `.mkv`). The start snaps back to the previous video keyframe, so the cut can be up to
/// one GOP early (plan §6.3: 1 s precision). Written to `<output>.tmp`, then renamed;
/// the source is never modified.
pub fn trim(input: &Path, output: &Path, start_us: i64, end_us: i64) -> Result<TrimInfo> {
    let path_c = |p: &Path| {
        p.to_str()
            .and_then(|s| CString::new(s).ok())
            .ok_or_else(|| MuxError::Path(p.to_path_buf()))
    };
    let cin = path_c(input)?;
    let mut tmp = output.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    let ctmp = path_c(&tmp)?;
    let is_mp4 = output.extension().is_some_and(|e| e.eq_ignore_ascii_case("mp4"));
    let fmt_name: &std::ffi::CStr = if is_mp4 { c"mp4" } else { c"matroska" };

    // SAFETY: libavformat stream copy; RAII guards close contexts and remove `.tmp` on error.
    unsafe {
        let in_ctx = open_media(&cin)?;
        check(
            "avformat_find_stream_info",
            ffi::avformat_find_stream_info(in_ctx.0, ptr::null_mut()),
        )?;
        let nb = (*in_ctx.0).nb_streams as usize;
        let video = (0..nb)
            .find(|&i| (*(*(*(*in_ctx.0).streams.add(i))).codecpar).codec_type == ffi::AVMediaType::AVMEDIA_TYPE_VIDEO)
            .ok_or_else(|| no_video("the file has no video"))?;

        let mut out_ptr = ptr::null_mut();
        check(
            "avformat_alloc_output_context2",
            ffi::avformat_alloc_output_context2(&mut out_ptr, ptr::null(), fmt_name.as_ptr(), ctmp.as_ptr()),
        )?;
        let mut out_ctx = OutFormat {
            fmt: out_ptr,
            tmp: tmp.clone(),
            finished: false,
        };
        for i in 0..nb {
            let in_st = *(*in_ctx.0).streams.add(i);
            let out_st = ffi::avformat_new_stream(out_ctx.fmt, ptr::null());
            if out_st.is_null() {
                return Err(no_video("avformat_new_stream returned NULL"));
            }
            check(
                "avcodec_parameters_copy",
                ffi::avcodec_parameters_copy((*out_st).codecpar, (*in_st).codecpar),
            )?;
            (*(*out_st).codecpar).codec_tag =
                if is_mp4 && (*(*out_st).codecpar).codec_id == ffi::AVCodecID::AV_CODEC_ID_HEVC {
                    u32::from_le_bytes(*b"hvc1")
                } else {
                    0
                };
            (*out_st).time_base = (*in_st).time_base;
            (*out_st).avg_frame_rate = (*in_st).avg_frame_rate;
            (*out_st).disposition = (*in_st).disposition;
            ffi::av_dict_copy(&mut (*out_st).metadata, (*in_st).metadata, 0);
        }
        if (*(*out_ctx.fmt).oformat).flags & ffi::AVFMT_NOFILE == 0 {
            check(
                "avio_open",
                ffi::avio_open(&mut (*out_ctx.fmt).pb, ctmp.as_ptr(), ffi::AVIO_FLAG_WRITE),
            )?;
        }
        let mut opts: *mut ffi::AVDictionary = ptr::null_mut();
        if is_mp4 {
            ffi::av_dict_set(&mut opts, c"movflags".as_ptr(), c"+faststart".as_ptr(), 0);
        }
        let ret = ffi::avformat_write_header(out_ctx.fmt, &mut opts);
        ffi::av_dict_free(&mut opts);
        check("avformat_write_header", ret)?;

        // Jump to the keyframe at or before the start; a file without an index reads from 0.
        let _ = ffi::av_seek_frame(in_ctx.0, -1, start_us.max(0), ffi::AVSEEK_FLAG_BACKWARD);

        let pkt_raw = ffi::av_packet_alloc();
        if pkt_raw.is_null() {
            return Err(no_video("av_packet_alloc returned NULL"));
        }
        let pkt = AvPacket(pkt_raw);
        let mut base_us: Option<i64> = None;
        let mut last_us = 0i64;
        // Packets since the latest video keyframe at or before the start. The seek above
        // is only a shortcut; this makes the cut correct even when it lands too early.
        let mut held: Vec<AvPacket> = Vec::new();
        let mut held_bytes = 0usize;
        let mut finished_reading = false;
        let mut saw_past_start = false;

        // Writes one packet rebased to the new time zero. `Ok(true)` = past the end.
        let emit = |p: *mut ffi::AVPacket, base: i64, last_us: &mut i64| -> Result<bool> {
            let idx = (*p).stream_index as usize;
            let in_tb = (*(*(*in_ctx.0).streams.add(idx))).time_base;
            let stamp = if (*p).pts != ffi::AV_NOPTS_VALUE {
                (*p).pts
            } else {
                (*p).dts
            };
            let ts_us = ffi::av_rescale_q(stamp, in_tb, US);
            if ts_us > end_us {
                return Ok(idx == video);
            }
            if ts_us < base {
                return Ok(false);
            }
            let shift = ffi::av_rescale_q(base, US, in_tb);
            if (*p).pts != ffi::AV_NOPTS_VALUE {
                (*p).pts = (*p).pts.saturating_sub(shift);
            }
            if (*p).dts != ffi::AV_NOPTS_VALUE {
                (*p).dts = (*p).dts.saturating_sub(shift);
            }
            let out_tb = (*(*(*out_ctx.fmt).streams.add(idx))).time_base;
            ffi::av_packet_rescale_ts(p, in_tb, out_tb);
            (*p).pos = -1;
            *last_us = (*last_us).max(ts_us.saturating_sub(base));
            check(
                "av_interleaved_write_frame",
                ffi::av_interleaved_write_frame(out_ctx.fmt, p),
            )?;
            Ok(false)
        };

        'read: loop {
            let ret = ffi::av_read_frame(in_ctx.0, pkt.0);
            if ret == ffi::AVERROR_EOF {
                finished_reading = true;
            } else {
                check("av_read_frame", ret)?;
            }
            let idx = (*pkt.0).stream_index as usize;
            let stamp = if (*pkt.0).pts != ffi::AV_NOPTS_VALUE {
                (*pkt.0).pts
            } else {
                (*pkt.0).dts
            };
            if !finished_reading && (idx >= nb || stamp == ffi::AV_NOPTS_VALUE) {
                ffi::av_packet_unref(pkt.0);
                continue;
            }
            let (ts_us, is_video, is_key) = if finished_reading {
                (i64::MAX, false, false)
            } else {
                let in_tb = (*(*(*in_ctx.0).streams.add(idx))).time_base;
                (
                    ffi::av_rescale_q(stamp, in_tb, US),
                    idx == video,
                    (*pkt.0).flags & ffi::AV_PKT_FLAG_KEY != 0,
                )
            };
            if base_us.is_none() {
                if !finished_reading && ts_us <= start_us {
                    // Still before the start: remember the latest keyframe and what follows it.
                    if is_video && is_key {
                        held.clear();
                        held_bytes = 0;
                    }
                    if is_video && is_key || !held.is_empty() {
                        held_bytes += (*pkt.0).size.max(0) as usize;
                        if held_bytes > MAX_HELD_BYTES {
                            return Err(no_video("the clip has too few keyframes to cut here"));
                        }
                        let copy = ffi::av_packet_clone(pkt.0);
                        if !copy.is_null() {
                            held.push(AvPacket(copy));
                        }
                    }
                    ffi::av_packet_unref(pkt.0);
                    continue;
                }
                saw_past_start |= !finished_reading;
                // First packet past the start (or end of file): begin at the held keyframe,
                // or at this one if it is a keyframe and nothing earlier qualifies.
                if held.is_empty() && !finished_reading && is_video && is_key {
                    let copy = ffi::av_packet_clone(pkt.0);
                    if !copy.is_null() {
                        held.push(AvPacket(copy));
                    }
                    ffi::av_packet_unref(pkt.0);
                    // The clone is written with the held packets; this one is not repeated.
                    let first = held[0].0;
                    let tb = (*(*(*in_ctx.0).streams.add(video))).time_base;
                    let stamp = if (*first).pts != ffi::AV_NOPTS_VALUE {
                        (*first).pts
                    } else {
                        (*first).dts
                    };
                    base_us = Some(ffi::av_rescale_q(stamp, tb, US));
                    for h in held.drain(..) {
                        emit(h.0, base_us.unwrap_or(0), &mut last_us)?;
                    }
                    continue;
                }
                if held.is_empty() || (finished_reading && !saw_past_start) {
                    if finished_reading {
                        break 'read;
                    }
                    ffi::av_packet_unref(pkt.0);
                    continue; // wait for a keyframe
                }
                let first = held[0].0;
                let tb = (*(*(*in_ctx.0).streams.add(video))).time_base;
                let stamp = if (*first).pts != ffi::AV_NOPTS_VALUE {
                    (*first).pts
                } else {
                    (*first).dts
                };
                let base = ffi::av_rescale_q(stamp, tb, US);
                base_us = Some(base);
                for h in held.drain(..) {
                    if emit(h.0, base, &mut last_us)? {
                        break 'read;
                    }
                }
                if finished_reading {
                    break 'read;
                }
            }
            if finished_reading {
                break 'read;
            }
            let base = base_us.unwrap_or(0);
            let past_end = emit(pkt.0, base, &mut last_us)?;
            ffi::av_packet_unref(pkt.0);
            if past_end {
                break 'read;
            }
        }
        let Some(base) = base_us else {
            return Err(no_video("no video in the chosen range"));
        };
        check("av_write_trailer", ffi::av_write_trailer(out_ctx.fmt))?;
        out_ctx.close();
        drop(in_ctx);
        replace_file(&tmp, output)?;
        out_ctx.finished = true;
        Ok(TrimInfo {
            start_us: base,
            duration_us: last_us,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::buffer::ReplayBuffer;
    use bytes::Bytes;

    fn pkt(stream: u8, key: bool, pts_us: i64) -> Packet {
        Packet::new(stream, key, pts_us, pts_us, Bytes::from_static(b"x"))
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

    #[test]
    fn remux_to_mp4_preserves_hevc_and_tags_hvc1() {
        let dir = std::env::temp_dir().join(format!("suicap_remux_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mkv = dir.join("test.mkv");
        let mp4 = dir.join("test.mp4");

        // Write a synthetic HEVC .mkv using libavformat.
        unsafe {
            let cmkv = CString::new(mkv.to_str().unwrap()).unwrap();
            let mut fmt = ptr::null_mut();
            check(
                "avformat_alloc_output_context2",
                ffi::avformat_alloc_output_context2(&mut fmt, ptr::null(), c"matroska".as_ptr(), cmkv.as_ptr()),
            )
            .unwrap();
            let in_ctx = OutFormat {
                fmt,
                tmp: mkv.clone(),
                finished: true,
            };
            let st = ffi::avformat_new_stream(in_ctx.fmt, ptr::null());
            assert!(!st.is_null());
            (*(*st).codecpar).codec_type = ffi::AVMediaType::AVMEDIA_TYPE_VIDEO;
            (*(*st).codecpar).codec_id = ffi::AVCodecID::AV_CODEC_ID_HEVC;
            (*(*st).codecpar).width = 640;
            (*(*st).codecpar).height = 360;
            // Minimal valid hvcC (HEVCDecoderConfigurationRecord, 23 bytes, numOfArrays = 0).
            let hvcc: [u8; 23] = [
                1, 0x01, 0x60, 0, 0, 0, 0x90, 0, 0, 0, 0, 0, 0x5d, 0xf0, 0, 0xfc, 0xfd, 0xf8, 0xf8, 0, 0, 0x0f, 0,
            ];
            let extra = ffi::av_mallocz(hvcc.len() + ffi::AV_INPUT_BUFFER_PADDING_SIZE as usize) as *mut u8;
            assert!(!extra.is_null());
            ptr::copy_nonoverlapping(hvcc.as_ptr(), extra, hvcc.len());
            (*(*st).codecpar).extradata = extra;
            (*(*st).codecpar).extradata_size = hvcc.len() as i32;
            (*st).time_base = US;
            (*st).avg_frame_rate = ffi::AVRational { num: 60, den: 1 };

            check(
                "avio_open",
                ffi::avio_open(&mut (*in_ctx.fmt).pb, cmkv.as_ptr(), ffi::AVIO_FLAG_WRITE),
            )
            .unwrap();
            check(
                "avformat_write_header",
                ffi::avformat_write_header(in_ctx.fmt, ptr::null_mut()),
            )
            .unwrap();

            // Minimal Annex B HEVC IDR NALU payload.
            let payload = [0x00u8, 0x00, 0x00, 0x01, 0x26, 0x01, 0xAF, 0x00];
            for i in 0..5i64 {
                let pkt = AvPacket(ffi::av_packet_alloc());
                assert!(!pkt.0.is_null());
                check("av_new_packet", ffi::av_new_packet(pkt.0, payload.len() as i32)).unwrap();
                ptr::copy_nonoverlapping(payload.as_ptr(), (*pkt.0).data, payload.len());
                let pts = ffi::av_rescale_q(i * 16_666, US, (*st).time_base);
                (*pkt.0).pts = pts;
                (*pkt.0).dts = pts;
                (*pkt.0).stream_index = 0;
                if i == 0 {
                    (*pkt.0).flags |= ffi::AV_PKT_FLAG_KEY;
                }
                check(
                    "av_interleaved_write_frame",
                    ffi::av_interleaved_write_frame(in_ctx.fmt, pkt.0),
                )
                .unwrap();
            }
            check("av_write_trailer", ffi::av_write_trailer(in_ctx.fmt)).unwrap();
            drop(in_ctx);
        }

        remux_to_mp4(&mkv, &mp4).unwrap();
        assert!(mp4.exists());
        assert!(!mp4.with_extension("mp4.tmp").exists());

        // Verify the remuxed .mp4 opens cleanly, has HEVC tagged as hvc1, and all 5 packets.
        unsafe {
            let cmp4 = CString::new(mp4.to_str().unwrap()).unwrap();
            let mut fmt = ptr::null_mut();
            check(
                "avformat_open_input",
                ffi::avformat_open_input(&mut fmt, cmp4.as_ptr(), ptr::null(), ptr::null_mut()),
            )
            .unwrap();
            let ctx = InFormat(fmt);
            check(
                "avformat_find_stream_info",
                ffi::avformat_find_stream_info(ctx.0, ptr::null_mut()),
            )
            .unwrap();
            assert_eq!((*ctx.0).nb_streams, 1);
            let st = *(*ctx.0).streams;
            assert_eq!((*(*st).codecpar).codec_id, ffi::AVCodecID::AV_CODEC_ID_HEVC);
            assert_eq!((*(*st).codecpar).codec_tag, u32::from_le_bytes(*b"hvc1"));
            assert_eq!((*(*st).codecpar).width, 640);
            assert_eq!((*(*st).codecpar).height, 360);

            let pkt = AvPacket(ffi::av_packet_alloc());
            let mut count = 0;
            loop {
                let ret = ffi::av_read_frame(ctx.0, pkt.0);
                if ret == ffi::AVERROR_EOF {
                    break;
                }
                check("av_read_frame", ret).unwrap();
                ffi::av_packet_unref(pkt.0);
                count += 1;
            }
            assert_eq!(count, 5);
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn remux_to_mp4_missing_input_leaves_no_tmp() {
        let dir = std::env::temp_dir().join(format!("suicap_remux_err_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mkv = dir.join("missing.mkv");
        let mp4 = dir.join("out.mp4");

        assert!(remux_to_mp4(&mkv, &mp4).is_err());
        assert!(!mp4.exists());
        assert!(!dir.join("out.mp4.tmp").exists());

        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod trim_tests {
    use super::*;

    /// A synthetic HEVC `.mkv` of `n` packets, 16 666 us apart, a keyframe every `key_every`.
    fn synth_mkv(path: &Path, n: i64, key_every: i64) {
        // SAFETY: libavformat calls on locals, same pattern as the remux test.
        unsafe {
            let cpath = CString::new(path.to_str().unwrap()).unwrap();
            let mut fmt = ptr::null_mut();
            check(
                "avformat_alloc_output_context2",
                ffi::avformat_alloc_output_context2(&mut fmt, ptr::null(), c"matroska".as_ptr(), cpath.as_ptr()),
            )
            .unwrap();
            let out = OutFormat {
                fmt,
                tmp: path.to_path_buf(),
                finished: true,
            };
            let st = ffi::avformat_new_stream(out.fmt, ptr::null());
            assert!(!st.is_null());
            (*(*st).codecpar).codec_type = ffi::AVMediaType::AVMEDIA_TYPE_VIDEO;
            (*(*st).codecpar).codec_id = ffi::AVCodecID::AV_CODEC_ID_HEVC;
            (*(*st).codecpar).width = 640;
            (*(*st).codecpar).height = 360;
            let hvcc: [u8; 23] = [
                1, 0x01, 0x60, 0, 0, 0, 0x90, 0, 0, 0, 0, 0, 0x5d, 0xf0, 0, 0xfc, 0xfd, 0xf8, 0xf8, 0, 0, 0x0f, 0,
            ];
            let extra = ffi::av_mallocz(hvcc.len() + ffi::AV_INPUT_BUFFER_PADDING_SIZE as usize) as *mut u8;
            ptr::copy_nonoverlapping(hvcc.as_ptr(), extra, hvcc.len());
            (*(*st).codecpar).extradata = extra;
            (*(*st).codecpar).extradata_size = hvcc.len() as i32;
            (*st).time_base = US;
            (*st).avg_frame_rate = ffi::AVRational { num: 60, den: 1 };
            check(
                "avio_open",
                ffi::avio_open(&mut (*out.fmt).pb, cpath.as_ptr(), ffi::AVIO_FLAG_WRITE),
            )
            .unwrap();
            check(
                "avformat_write_header",
                ffi::avformat_write_header(out.fmt, ptr::null_mut()),
            )
            .unwrap();
            let payload = [0x00u8, 0x00, 0x00, 0x01, 0x26, 0x01, 0xAF, 0x00];
            for i in 0..n {
                let pkt = AvPacket(ffi::av_packet_alloc());
                check("av_new_packet", ffi::av_new_packet(pkt.0, payload.len() as i32)).unwrap();
                ptr::copy_nonoverlapping(payload.as_ptr(), (*pkt.0).data, payload.len());
                let ts = ffi::av_rescale_q(i * 16_666, US, (*st).time_base);
                (*pkt.0).pts = ts;
                (*pkt.0).dts = ts;
                (*pkt.0).stream_index = 0;
                if i % key_every == 0 {
                    (*pkt.0).flags |= ffi::AV_PKT_FLAG_KEY;
                }
                check(
                    "av_interleaved_write_frame",
                    ffi::av_interleaved_write_frame(out.fmt, pkt.0),
                )
                .unwrap();
            }
            check("av_write_trailer", ffi::av_write_trailer(out.fmt)).unwrap();
        }
    }

    /// (packets, first pts us) of a file.
    fn read_back(path: &Path) -> (usize, i64) {
        // SAFETY: read-only demux with RAII guards.
        unsafe {
            let cpath = CString::new(path.to_str().unwrap()).unwrap();
            let mut fmt = ptr::null_mut();
            check(
                "avformat_open_input",
                ffi::avformat_open_input(&mut fmt, cpath.as_ptr(), ptr::null(), ptr::null_mut()),
            )
            .unwrap();
            let ctx = InFormat(fmt);
            check(
                "avformat_find_stream_info",
                ffi::avformat_find_stream_info(ctx.0, ptr::null_mut()),
            )
            .unwrap();
            let tb = (*(*(*ctx.0).streams)).time_base;
            let pkt = AvPacket(ffi::av_packet_alloc());
            let (mut n, mut first) = (0, i64::MAX);
            while ffi::av_read_frame(ctx.0, pkt.0) >= 0 {
                first = first.min(ffi::av_rescale_q((*pkt.0).pts, tb, US));
                ffi::av_packet_unref(pkt.0);
                n += 1;
            }
            (n, first)
        }
    }

    #[test]
    fn trim_snaps_to_the_previous_keyframe_and_rebases_time() {
        let dir = std::env::temp_dir().join(format!("suicap_trim_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let (src, out) = (dir.join("src.mkv"), dir.join("out.mp4"));
        synth_mkv(&src, 180, 60); // 3 s, a keyframe each second
        assert!(duration_us(&src).unwrap().is_some_and(|d| d > 2_900_000));

        let info = trim(&src, &out, 1_200_000, 1_700_000).unwrap();
        assert!(
            (info.start_us - 999_960).abs() < 1000,
            "snapped to the 1 s keyframe: {}",
            info.start_us
        );
        let (n, first) = read_back(&out);
        assert!((40..=46).contains(&n), "about 43 frames, got {n}");
        assert!(first.abs() < 1000, "time restarts at zero, got {first}");
        assert!(!out.with_extension("mp4.tmp").exists());
        assert!(src.exists());

        // A range past the end has no video to copy and leaves nothing behind.
        let none = dir.join("none.mkv");
        assert!(trim(&src, &none, 50_000_000, 51_000_000).is_err());
        assert!(!none.exists() && !dir.join("none.mkv.tmp").exists());
        assert!(trim(&dir.join("missing.mkv"), &dir.join("m.mkv"), 0, 1_000_000).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
