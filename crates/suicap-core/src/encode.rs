//! Hardware encode through FFmpeg on our own D3D11 device (plan §4.3).
//! FFI module: owns every `unsafe` touching libavcodec/libavutil.

use std::{
    ffi::{CStr, CString, c_char},
    ptr,
};

use ffmpeg_sys_next as ffi;
use windows::{
    Win32::Graphics::Direct3D11::{D3D11_BIND_RENDER_TARGET, ID3D11Device, ID3D11Texture2D},
    core::Interface,
};

use crate::{
    buffer::{Packet, PacketSlab},
    capture::Vendor,
    config::{Codec, VideoPreset},
};

#[derive(Debug, thiserror::Error)]
pub enum EncodeError {
    #[error("no FFmpeg encoder named {0}")]
    NotFound(String),
    #[error("{call} failed: {msg} ({code})")]
    Ffmpeg { call: &'static str, code: i32, msg: String },
    #[error("{0}")]
    Unsupported(&'static str),
}

impl EncodeError {
    /// Encoder failures need a rebuild decided by the pipeline; none self-heal.
    pub fn is_recoverable(&self) -> bool {
        false
    }
}

type Result<T> = std::result::Result<T, EncodeError>;

fn check(call: &'static str, code: i32) -> Result<i32> {
    if code >= 0 {
        Ok(code)
    } else {
        Err(EncodeError::Ffmpeg {
            call,
            code,
            msg: av_err(code),
        })
    }
}

pub(crate) fn av_err(code: i32) -> String {
    let mut buf = [0 as c_char; 256];
    // SAFETY: buf is writable for its full length; av_strerror NUL-terminates.
    unsafe {
        ffi::av_strerror(code, buf.as_mut_ptr(), buf.len());
        CStr::from_ptr(buf.as_ptr()).to_string_lossy().into_owned()
    }
}

/// Silence (or restore) FFmpeg's own stderr logging.
pub fn set_log_quiet(quiet: bool) {
    let level = if quiet { ffi::AV_LOG_QUIET } else { ffi::AV_LOG_WARNING };
    // SAFETY: sets a global integer.
    unsafe { ffi::av_log_set_level(level) };
}

/// `AVHWDeviceContext` (D3D11VA) wrapping an existing device.
pub struct HwDevice(*mut ffi::AVBufferRef);

impl HwDevice {
    pub fn wrap(device: &ID3D11Device) -> Result<Self> {
        // SAFETY: alloc returns an owned ref or NULL. We hand FFmpeg its own COM reference
        // (clone + into_raw), which it releases when the context is freed.
        unsafe {
            let buf = ffi::av_hwdevice_ctx_alloc(ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_D3D11VA);
            if buf.is_null() {
                return Err(EncodeError::Unsupported("av_hwdevice_ctx_alloc(D3D11VA) returned NULL"));
            }
            let this = HwDevice(buf);
            let hw = (*buf).data as *mut ffi::AVHWDeviceContext;
            let d3d = (*hw).hwctx as *mut ffi::AVD3D11VADeviceContext;
            (*d3d).device = device.clone().into_raw() as *mut ffi::ID3D11Device;
            check("av_hwdevice_ctx_init", ffi::av_hwdevice_ctx_init(buf))?;
            Ok(this)
        }
    }
}

impl Drop for HwDevice {
    fn drop(&mut self) {
        // SAFETY: we own this reference.
        unsafe { ffi::av_buffer_unref(&mut self.0) };
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SwFormat {
    Nv12,
    P010,
}

/// Frames the video thread may have in flight (blt target + inside the encoder);
/// at this cap the next frame is dropped (plan rule 3).
pub const MAX_FRAMES_IN_FLIGHT: usize = 8;

/// `AVHWFramesContext` of D3D11 textures that VideoProcessorBlt renders into.
///
/// `initial_pool_size` stays 0: a preallocated pool is one NV12 *array* texture, and
/// D3D11 rejects arrays with `BIND_RENDER_TARGET` (E_INVALIDARG, verified on RTX 4070 /
/// FFmpeg 9.0). With 0, FFmpeg allocates single textures on demand and recycles them, so
/// the caller enforces `MAX_FRAMES_IN_FLIGHT` instead of relying on pool exhaustion.
pub struct HwFrames(*mut ffi::AVBufferRef);

impl HwFrames {
    pub fn new(dev: &HwDevice, sw: SwFormat, width: u32, height: u32) -> Result<Self> {
        // SAFETY: frames ctx allocated from a live device ref; fields set before init.
        unsafe {
            let buf = ffi::av_hwframe_ctx_alloc(dev.0);
            if buf.is_null() {
                return Err(EncodeError::Unsupported("av_hwframe_ctx_alloc returned NULL"));
            }
            let this = HwFrames(buf);
            let fc = (*buf).data as *mut ffi::AVHWFramesContext;
            (*fc).format = ffi::AVPixelFormat::AV_PIX_FMT_D3D11;
            (*fc).sw_format = match sw {
                SwFormat::Nv12 => ffi::AVPixelFormat::AV_PIX_FMT_NV12,
                SwFormat::P010 => ffi::AVPixelFormat::AV_PIX_FMT_P010LE,
            };
            (*fc).width = width as i32;
            (*fc).height = height as i32;
            (*fc).initial_pool_size = 0;
            let d3d = (*fc).hwctx as *mut ffi::AVD3D11VAFramesContext;
            // RENDER_TARGET is what the blt needs; enough for NVENC (verified). AMF/QSV
            // extra flags still [VERIFY] on that hardware.
            (*d3d).BindFlags = D3D11_BIND_RENDER_TARGET.0 as u32;
            check("av_hwframe_ctx_init", ffi::av_hwframe_ctx_init(buf))?;
            Ok(this)
        }
    }
}

impl Drop for HwFrames {
    fn drop(&mut self) {
        // SAFETY: we own this reference.
        unsafe { ffi::av_buffer_unref(&mut self.0) };
    }
}

/// FFmpeg encoder name for a vendor/codec pair.
pub fn encoder_name(vendor: Vendor, codec: Codec) -> Option<String> {
    let suffix = match vendor {
        Vendor::Nvidia => "nvenc",
        Vendor::Amd => "amf",
        Vendor::Intel => "qsv",
        Vendor::Other(_) => return None,
    };
    Some(format!("{}_{suffix}", codec.name()))
}

#[derive(Debug, Clone, Copy)]
pub struct EncoderSettings {
    pub vendor: Vendor,
    pub codec: Codec,
    pub preset: VideoPreset,
    pub max_bitrate_mbps: u32,
    /// NVENC `cq` (lower = better); ignored by other vendors.
    pub quality: u8,
}

impl EncoderSettings {
    /// Vendor option table (plan §4.3): quality at bounded bitrate, no lookahead.
    fn options(&self) -> Vec<(&'static str, String)> {
        let max = u64::from(self.max_bitrate_mbps) * 1_000_000;
        match self.vendor {
            Vendor::Nvidia => vec![
                ("preset", "p4".into()),
                ("tune", "hq".into()),
                ("rc", "vbr".into()),
                ("cq", self.quality.to_string()),
                ("rc-lookahead", "0".into()),
                ("maxrate", max.to_string()),
                ("bufsize", (2 * max).to_string()),
            ],
            Vendor::Amd => vec![
                ("usage", "transcoding".into()),
                ("quality", "balanced".into()),
                ("rc", "vbr_peak".into()),
                ("b", (max * 7 / 10).to_string()),
                ("maxrate", max.to_string()),
                ("bufsize", (2 * max).to_string()),
            ],
            Vendor::Intel => vec![
                ("preset", "medium".into()),
                ("low_power", "1".into()),
                ("async_depth", "1".into()),
                ("b", (max * 7 / 10).to_string()),
                ("maxrate", max.to_string()),
                ("bufsize", (2 * max).to_string()),
            ],
            Vendor::Other(_) => Vec::new(),
        }
    }
}

pub struct Encoder {
    ctx: *mut ffi::AVCodecContext,
    pkt: *mut ffi::AVPacket,
    slab: PacketSlab,
    pub name: String,
    /// Options the encoder did not recognise (a wrong option table shows up here).
    pub unused_options: Vec<String>,
}

impl Encoder {
    pub fn open(s: &EncoderSettings, frames: &HwFrames) -> Result<Self> {
        if s.vendor == Vendor::Intel {
            // QSV needs a derived device + frames context; implemented last (plan §4.3).
            return Err(EncodeError::Unsupported("QSV path not implemented yet"));
        }
        let name = encoder_name(s.vendor, s.codec).ok_or(EncodeError::Unsupported("unknown GPU vendor"))?;
        let cname = CString::new(name.clone()).expect("encoder names have no NUL");
        // SAFETY: standard libavcodec open sequence; ctx and pkt owned by `Encoder` from the
        // moment they are allocated so every early return frees them via Drop.
        unsafe {
            let codec = ffi::avcodec_find_encoder_by_name(cname.as_ptr());
            if codec.is_null() {
                return Err(EncodeError::NotFound(name));
            }
            let ctx = ffi::avcodec_alloc_context3(codec);
            if ctx.is_null() {
                return Err(EncodeError::Unsupported("avcodec_alloc_context3 returned NULL"));
            }
            let pkt = ffi::av_packet_alloc();
            let mut enc = Encoder {
                ctx,
                pkt,
                slab: PacketSlab::new(),
                name,
                unused_options: Vec::new(),
            };
            if pkt.is_null() {
                return Err(EncodeError::Unsupported("av_packet_alloc returned NULL"));
            }
            let fps = s.preset.fps as i32;
            (*ctx).width = s.preset.width as i32;
            (*ctx).height = s.preset.height as i32;
            (*ctx).pix_fmt = ffi::AVPixelFormat::AV_PIX_FMT_D3D11;
            (*ctx).time_base = ffi::AVRational { num: 1, den: 1_000_000 }; // QPC microseconds
            (*ctx).framerate = ffi::AVRational { num: fps, den: 1 };
            (*ctx).gop_size = fps; // 1 s GOP = trim/replay granularity
            (*ctx).max_b_frames = 0;
            (*ctx).flags |= ffi::AV_CODEC_FLAG_GLOBAL_HEADER as i32;
            (*ctx).color_primaries = ffi::AVColorPrimaries::AVCOL_PRI_BT709;
            (*ctx).color_trc = ffi::AVColorTransferCharacteristic::AVCOL_TRC_BT709;
            (*ctx).colorspace = ffi::AVColorSpace::AVCOL_SPC_BT709;
            (*ctx).color_range = ffi::AVColorRange::AVCOL_RANGE_MPEG;
            (*ctx).hw_frames_ctx = ffi::av_buffer_ref(frames.0);
            if (*ctx).hw_frames_ctx.is_null() {
                return Err(EncodeError::Unsupported("av_buffer_ref returned NULL"));
            }

            let mut dict: *mut ffi::AVDictionary = ptr::null_mut();
            for (k, v) in s.options() {
                let (k, v) = (CString::new(k).expect("static key"), CString::new(v).expect("number"));
                ffi::av_dict_set(&mut dict, k.as_ptr(), v.as_ptr(), 0);
            }
            let ret = ffi::avcodec_open2(ctx, codec, &mut dict);
            let mut e: *const ffi::AVDictionaryEntry = ptr::null();
            loop {
                e = ffi::av_dict_iterate(dict, e);
                if e.is_null() {
                    break;
                }
                enc.unused_options
                    .push(CStr::from_ptr((*e).key).to_string_lossy().into_owned());
            }
            ffi::av_dict_free(&mut dict);
            check("avcodec_open2", ret)?;
            Ok(enc)
        }
    }

    /// Size of the out-of-band headers the muxer needs (0 means GLOBAL_HEADER was ignored).
    pub fn extradata_len(&self) -> usize {
        // SAFETY: ctx is open and owned by self.
        unsafe { (*self.ctx).extradata_size.max(0) as usize }
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        // SAFETY: we own both; free functions accept NULL and free also unrefs hw_frames_ctx.
        unsafe {
            ffi::av_packet_free(&mut self.pkt);
            ffi::avcodec_free_context(&mut self.ctx);
        }
    }
}

// ---------------------------------------------------------------------------
// Frames in, packets out.

/// One D3D11 texture from the hwframes pool, as an `AVFrame`.
pub struct HwFrame(*mut ffi::AVFrame);

impl HwFrames {
    /// Check out a texture. Recycled from FFmpeg's buffer pool when one is free.
    pub fn get(&self) -> Result<HwFrame> {
        // SAFETY: frame allocated here, owned by HwFrame; get_buffer fills data[0..1].
        unsafe {
            let f = ffi::av_frame_alloc();
            if f.is_null() {
                return Err(EncodeError::Unsupported("av_frame_alloc returned NULL"));
            }
            let frame = HwFrame(f);
            check("av_hwframe_get_buffer", ffi::av_hwframe_get_buffer(self.0, f, 0))?;
            Ok(frame)
        }
    }
}

impl HwFrame {
    /// The texture to render into and its array slice (always 0 with single textures).
    pub fn texture(&self) -> (ID3D11Texture2D, u32) {
        // SAFETY: for AV_PIX_FMT_D3D11, data[0] is an ID3D11Texture2D* kept alive by the
        // frame's buffer ref, and data[1] is the slice index as intptr_t.
        unsafe {
            let raw = (*self.0).data[0] as *mut std::ffi::c_void;
            let tex = ID3D11Texture2D::from_raw_borrowed(&raw)
                .expect("hw frame has a texture")
                .clone();
            (tex, (*self.0).data[1] as usize as u32)
        }
    }
}

impl Drop for HwFrame {
    fn drop(&mut self) {
        // SAFETY: we own the frame; free unrefs the texture back to the pool.
        unsafe { ffi::av_frame_free(&mut self.0) };
    }
}

/// Result of offering a frame to the encoder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sent {
    Accepted,
    /// Encoder input queue full: drop this frame (plan rule 3).
    Busy,
}

impl Encoder {
    /// Submit `frame` with `pts_us`. The encoder takes its own reference, so the same
    /// `HwFrame` can be re-sent on the next tick (CFR repeat). `None` starts the flush.
    pub fn send(&mut self, frame: Option<&HwFrame>, pts_us: i64) -> Result<Sent> {
        // SAFETY: ctx is open; the frame pointer is live for the call.
        let ret = unsafe {
            match frame {
                Some(f) => {
                    (*f.0).pts = pts_us;
                    ffi::avcodec_send_frame(self.ctx, f.0)
                }
                None => ffi::avcodec_send_frame(self.ctx, ptr::null()),
            }
        };
        if ret == ffi::AVERROR(ffi::EAGAIN) {
            return Ok(Sent::Busy);
        }
        check("avcodec_send_frame", ret)?;
        Ok(Sent::Accepted)
    }

    /// Drain every ready packet into `out` (compressed bitstream, packed once into the
    /// packet slab; video frames themselves never leave the GPU). Returns false at EOF.
    pub fn receive(&mut self, stream: u8, out: &mut Vec<Packet>) -> Result<bool> {
        // SAFETY: self.pkt is a reusable owned packet; data is valid until av_packet_unref.
        unsafe {
            loop {
                let ret = ffi::avcodec_receive_packet(self.ctx, self.pkt);
                if ret == ffi::AVERROR(ffi::EAGAIN) {
                    return Ok(true);
                }
                if ret == ffi::AVERROR_EOF {
                    return Ok(false);
                }
                if let Err(e) = check("avcodec_receive_packet", ret) {
                    ffi::av_packet_unref(self.pkt);
                    return Err(e);
                }
                let p = &*self.pkt;
                let data = std::slice::from_raw_parts(p.data, p.size.max(0) as usize);
                out.push(Packet::new(
                    stream,
                    p.flags & ffi::AV_PKT_FLAG_KEY != 0,
                    p.pts,
                    p.dts,
                    self.slab.copy_from_slice(data),
                ));
                ffi::av_packet_unref(self.pkt);
            }
        }
    }
}

/// Codec parameters (incl. extradata) copied out of an open encoder, so a mux worker
/// thread can write a file without touching the encoder.
pub struct StreamParams {
    par: *mut ffi::AVCodecParameters,
    pub fps: u32,
}

// SAFETY: AVCodecParameters is plain owned data, immutable after creation; it is only
// read (copied into a new stream) by other threads.
unsafe impl Send for StreamParams {}
unsafe impl Sync for StreamParams {}

impl StreamParams {
    pub(crate) fn as_ptr(&self) -> *const ffi::AVCodecParameters {
        self.par
    }
}

impl Drop for StreamParams {
    fn drop(&mut self) {
        // SAFETY: we own the parameters.
        unsafe { ffi::avcodec_parameters_free(&mut self.par) };
    }
}

impl Encoder {
    pub fn stream_params(&self, fps: u32) -> Result<StreamParams> {
        // SAFETY: allocated here and owned by StreamParams; ctx is open.
        unsafe {
            let par = ffi::avcodec_parameters_alloc();
            if par.is_null() {
                return Err(EncodeError::Unsupported("avcodec_parameters_alloc returned NULL"));
            }
            let params = StreamParams { par, fps };
            check(
                "avcodec_parameters_from_context",
                ffi::avcodec_parameters_from_context(par, self.ctx),
            )?;
            Ok(params)
        }
    }
}

// ---------------------------------------------------------------------------
// Audio encode (plan §5): 48 kHz stereo float in, AAC (.mp4) or Opus (.mkv) out.

pub const AUDIO_RATE: i32 = 48_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioCodec {
    /// AAC 160 kbps, plays everywhere (default for .mp4).
    Aac,
    /// Opus 128 kbps via libopus (default for .mkv).
    Opus,
}

impl AudioCodec {
    /// Codec for a container extension (plan §5).
    pub fn for_extension(ext: &str) -> Self {
        if ext.eq_ignore_ascii_case("mkv") {
            AudioCodec::Opus
        } else {
            AudioCodec::Aac
        }
    }
}

/// One audio track encoder. Owns its context and reusable input frame/output packet.
pub struct AudioEncoder {
    ctx: *mut ffi::AVCodecContext,
    frame: *mut ffi::AVFrame,
    pkt: *mut ffi::AVPacket,
    slab: PacketSlab,
    planar: bool,
    /// Samples per channel the encoder wants per frame (AAC 1024, Opus 960).
    pub frame_size: usize,
    /// QPC microseconds of sample 0; packet PTS are reported on this timeline.
    origin_us: i64,
}

// SAFETY: the encoder is moved to the audio mix thread and used only there; FFmpeg
// codec contexts are not tied to the thread that opened them.
unsafe impl Send for AudioEncoder {}

impl AudioEncoder {
    pub fn open(codec: AudioCodec, origin_us: i64) -> Result<Self> {
        let (name, bitrate, fmt) = match codec {
            AudioCodec::Aac => (c"aac", 160_000, ffi::AVSampleFormat::AV_SAMPLE_FMT_FLTP),
            AudioCodec::Opus => (c"libopus", 128_000, ffi::AVSampleFormat::AV_SAMPLE_FMT_FLT),
        };
        // SAFETY: standard open sequence; ctx/frame/pkt owned by Self from allocation on, so
        // early returns free them in Drop.
        unsafe {
            let c = ffi::avcodec_find_encoder_by_name(name.as_ptr());
            if c.is_null() {
                return Err(EncodeError::NotFound(name.to_string_lossy().into_owned()));
            }
            let ctx = ffi::avcodec_alloc_context3(c);
            if ctx.is_null() {
                return Err(EncodeError::Unsupported("avcodec_alloc_context3 returned NULL"));
            }
            let pkt = ffi::av_packet_alloc();
            let mut enc = AudioEncoder {
                ctx,
                frame: ptr::null_mut(),
                pkt,
                slab: PacketSlab::new(),
                planar: fmt == ffi::AVSampleFormat::AV_SAMPLE_FMT_FLTP,
                frame_size: 0,
                origin_us,
            };
            if pkt.is_null() {
                return Err(EncodeError::Unsupported("av_packet_alloc returned NULL"));
            }
            (*ctx).sample_fmt = fmt;
            (*ctx).sample_rate = AUDIO_RATE;
            ffi::av_channel_layout_default(&mut (*ctx).ch_layout, 2);
            (*ctx).bit_rate = bitrate;
            (*ctx).time_base = ffi::AVRational {
                num: 1,
                den: AUDIO_RATE,
            };
            (*ctx).flags |= ffi::AV_CODEC_FLAG_GLOBAL_HEADER as i32;
            check("avcodec_open2", ffi::avcodec_open2(ctx, c, ptr::null_mut()))?;
            enc.frame_size = (*ctx).frame_size.max(1) as usize;

            let f = ffi::av_frame_alloc();
            if f.is_null() {
                return Err(EncodeError::Unsupported("av_frame_alloc returned NULL"));
            }
            enc.frame = f;
            (*f).format = fmt as i32;
            (*f).sample_rate = AUDIO_RATE;
            (*f).nb_samples = enc.frame_size as i32;
            check(
                "av_channel_layout_copy",
                ffi::av_channel_layout_copy(&mut (*f).ch_layout, &(*ctx).ch_layout),
            )?;
            check("av_frame_get_buffer", ffi::av_frame_get_buffer(f, 0))?;
            Ok(enc)
        }
    }

    /// Encode one frame of `frame_size` interleaved stereo samples starting at sample
    /// index `pos` (relative to `origin_us`). Ready packets are appended to `out`.
    pub fn encode(&mut self, interleaved: &[f32], pos: i64, stream: u8, out: &mut Vec<Packet>) -> Result<()> {
        let n = self.frame_size;
        debug_assert_eq!(interleaved.len(), n * 2);
        // SAFETY: frame buffers were allocated for n samples x 2 channels in `fmt`.
        unsafe {
            check("av_frame_make_writable", ffi::av_frame_make_writable(self.frame))?;
            let f = &mut *self.frame;
            if self.planar {
                let l = std::slice::from_raw_parts_mut(f.data[0] as *mut f32, n);
                let r = std::slice::from_raw_parts_mut(f.data[1] as *mut f32, n);
                for (i, s) in interleaved.chunks_exact(2).enumerate() {
                    l[i] = s[0];
                    r[i] = s[1];
                }
            } else {
                std::slice::from_raw_parts_mut(f.data[0] as *mut f32, n * 2).copy_from_slice(interleaved);
            }
            f.pts = pos;
            check("avcodec_send_frame", ffi::avcodec_send_frame(self.ctx, self.frame))?;
        }
        self.drain(stream, out).map(|_| ())
    }

    /// Flush at end of stream.
    pub fn flush(&mut self, stream: u8, out: &mut Vec<Packet>) -> Result<()> {
        // SAFETY: NULL frame starts draining.
        check("avcodec_send_frame", unsafe {
            ffi::avcodec_send_frame(self.ctx, ptr::null())
        })?;
        while self.drain(stream, out)? {}
        Ok(())
    }

    fn drain(&mut self, stream: u8, out: &mut Vec<Packet>) -> Result<bool> {
        let to_us = |samples: i64| self.origin_us + (i128::from(samples) * 1_000_000 / i128::from(AUDIO_RATE)) as i64;
        // SAFETY: self.pkt is a reusable owned packet; data valid until unref.
        unsafe {
            loop {
                let ret = ffi::avcodec_receive_packet(self.ctx, self.pkt);
                if ret == ffi::AVERROR(ffi::EAGAIN) {
                    return Ok(true);
                }
                if ret == ffi::AVERROR_EOF {
                    return Ok(false);
                }
                if let Err(e) = check("avcodec_receive_packet", ret) {
                    ffi::av_packet_unref(self.pkt);
                    return Err(e);
                }
                let p = &*self.pkt;
                let data = std::slice::from_raw_parts(p.data, p.size.max(0) as usize);
                out.push(Packet::new(
                    stream,
                    true, // audio packets are all sync points
                    to_us(p.pts),
                    to_us(p.dts),
                    self.slab.copy_from_slice(data),
                ));
                ffi::av_packet_unref(self.pkt);
            }
        }
    }

    pub fn stream_params(&self) -> Result<StreamParams> {
        // SAFETY: as Encoder::stream_params.
        unsafe {
            let par = ffi::avcodec_parameters_alloc();
            if par.is_null() {
                return Err(EncodeError::Unsupported("avcodec_parameters_alloc returned NULL"));
            }
            let params = StreamParams { par, fps: 0 };
            check(
                "avcodec_parameters_from_context",
                ffi::avcodec_parameters_from_context(par, self.ctx),
            )?;
            Ok(params)
        }
    }
}

impl Drop for AudioEncoder {
    fn drop(&mut self) {
        // SAFETY: we own frame, pkt, and ctx; free functions accept NULL.
        unsafe {
            ffi::av_packet_free(&mut self.pkt);
            ffi::av_frame_free(&mut self.frame);
            ffi::avcodec_free_context(&mut self.ctx);
        }
    }
}
