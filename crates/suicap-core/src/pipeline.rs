//! Video thread wiring (plan §3): capture -> blt -> send_frame/receive_packet -> sink.
//! `record` sinks packets into a file; `replay` into the RAM replay buffer, saving
//! snapshots on a mux worker while capture keeps running.

use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use crate::{
    audio::{AudioError, AudioOptions, AudioPath, AudioStats},
    buffer::{Packet, ReplayBuffer, VIDEO_STREAM},
    capture::{self, Adapter, CaptureError, CaptureSource, Output, Pacer, WgcSource},
    config::{self, Codec, VideoPreset},
    encode::{
        self, Encoder, EncoderSettings, HwDevice, HwFrame, HwFrames, MAX_FRAMES_IN_FLIGHT, Sent, StreamParams, SwFormat,
    },
    fault,
    mux::{self, ClipInfo, MuxError, MuxStream, Muxer},
    preprocess::{PreprocessError, Preprocessor},
};
use windows::Win32::Graphics::Direct3D11::ID3D11Device;

#[derive(Debug, thiserror::Error)]
pub enum PipelineError {
    #[error("requested monitor not found on a hardware adapter")]
    NoOutput,
    #[error("no encoder opened: {0}")]
    NoEncoder(String),
    #[error("no first frame from the capture source within {0:?}")]
    NoFirstFrame(Duration),
    #[error("capture: {0}")]
    Capture(#[from] CaptureError),
    #[error("preprocess: {0}")]
    Preprocess(#[from] PreprocessError),
    #[error("encode: {0}")]
    Encode(#[from] encode::EncodeError),
    #[error("mux: {0}")]
    Mux(#[from] MuxError),
    #[error("audio: {0}")]
    Audio(#[from] AudioError),
    #[error("mux worker panicked")]
    MuxPanic,
    #[error("the captured monitor moved to another graphics adapter")]
    AdapterChanged,
}

impl PipelineError {
    pub fn is_recoverable(&self) -> bool {
        match self {
            PipelineError::Capture(e) => e.is_recoverable(),
            PipelineError::Preprocess(e) => e.is_recoverable(),
            PipelineError::Audio(e) => e.is_recoverable(),
            _ => false,
        }
    }
}

type Result<T> = std::result::Result<T, PipelineError>;

/// What to capture and how to encode it.
#[derive(Debug, Clone)]
pub struct VideoOptions {
    pub preset: VideoPreset,
    /// Pin a codec instead of probing AV1 -> HEVC -> H.264.
    pub codec: Option<Codec>,
    pub cursor: bool,
    /// Capture `\\.\DISPLAY<n>` instead of the primary monitor.
    pub display: Option<u32>,
    /// Constant-quality level, bounded by the max bitrate (plan §4.3).
    pub quality: u8,
}

impl Default for VideoOptions {
    fn default() -> Self {
        Self {
            preset: VideoPreset::P1080_60,
            codec: None,
            cursor: true,
            display: None,
            quality: 23,
        }
    }
}

impl VideoOptions {
    pub fn from_settings(s: &config::Settings) -> Self {
        let v = s.video();
        Self {
            preset: VideoPreset {
                width: v.width,
                height: v.height,
                fps: v.fps,
            },
            codec: v.codec,
            cursor: s.cursor,
            display: s.display,
            quality: v.quality,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct VideoStats {
    pub encoder: String,
    pub codec: Option<Codec>,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub source: String,
    /// Ticks that produced an encoded frame (new or repeated).
    pub frames: u64,
    /// New source frames converted (the rest were CFR repeats).
    pub fresh: u64,
    /// Frames skipped: late ticks, encoder busy or in-flight cap (plan rule 3).
    pub dropped: u64,
    pub packets: u64,
    pub bytes: u64,
    /// Capture source recreated after access loss (plan §7 row 1).
    pub source_restarts: u64,
    /// Degraded features to show the user (plan §7 last row).
    pub notes: Vec<String>,
}

/// The wanted monitor, or the primary one if it is gone (plan §7 row 3). The flag
/// says the fallback was used.
fn pick_output_or_primary(display: Option<u32>) -> Result<(Adapter, Output, bool)> {
    match pick_output(display) {
        Ok((a, o)) => Ok((a, o, false)),
        Err(PipelineError::NoOutput) if display.is_some() => pick_output(None).map(|(a, o)| (a, o, true)),
        Err(e) => Err(e),
    }
}

/// The adapter and output to capture (plan §4.1: device on the owning adapter).
fn pick_output(display: Option<u32>) -> Result<(Adapter, Output)> {
    let wanted = |o: &Output| match display {
        Some(n) => o.attached && o.device_name.eq_ignore_ascii_case(&format!(r"\\.\DISPLAY{n}")),
        None => o.is_primary(),
    };
    for a in capture::adapters()?.into_iter().filter(|a| !a.software) {
        if let Some(o) = capture::outputs(&a)?.into_iter().find(wanted) {
            return Ok((a, o));
        }
    }
    Err(PipelineError::NoOutput)
}

/// Open the first codec that works, in plan order (plan §4.3).
fn open_encoder(
    adapter: &Adapter,
    preset: VideoPreset,
    pinned: Option<Codec>,
    quality: u8,
    frames: &HwFrames,
) -> Result<(Encoder, Codec)> {
    let candidates: Vec<Codec> = match pinned {
        Some(c) => vec![c],
        None => Codec::PREFERENCE
            .into_iter()
            .filter(|c| *c != Codec::H264 || preset.allows_h264())
            .collect(),
    };
    let mut errors = Vec::new();
    for codec in candidates {
        let s = EncoderSettings {
            vendor: adapter.vendor,
            codec,
            preset,
            max_bitrate_mbps: preset.max_bitrate_mbps(codec),
            quality,
        };
        match Encoder::open(&s, frames) {
            Ok(e) => return Ok((e, codec)),
            Err(e) => errors.push(format!("{}: {e}", codec.name())),
        }
    }
    Err(PipelineError::NoEncoder(errors.join("; ")))
}

fn wait_first_frame(src: &mut impl CaptureSource, limit: Duration) -> Result<capture::Frame> {
    let start = std::time::Instant::now();
    loop {
        if let Some(f) = src.next_frame(0)? {
            return Ok(f);
        }
        if start.elapsed() > limit {
            return Err(PipelineError::NoFirstFrame(limit));
        }
        std::thread::sleep(Duration::from_millis(2));
    }
}

fn convert(pre: &mut Preprocessor, frames: &HwFrames, src: &capture::Frame) -> Result<HwFrame> {
    let dst = frames.get()?;
    let (tex, index) = dst.texture();
    pre.blt(src, &tex, index)?;
    Ok(dst)
}

/// `SUICAP_MEM_TRACE=1`: print private bytes after each setup stage (budget work, §1).
struct MemTrace(bool);

impl MemTrace {
    fn from_env() -> Self {
        Self(std::env::var_os("SUICAP_MEM_TRACE").is_some())
    }

    fn mark(&self, stage: &str) {
        if self.0 {
            let u = capture::process_usage();
            let (commit, ws) = (u.private_bytes as f64 / 1e6, u.private_ws_bytes as f64 / 1e6);
            eprintln!("[mem] commit {commit:6.1} MB  private WS {ws:6.1} MB  after {stage}");
        }
    }
}

/// Result of a capture-source rebuild on the helper thread.
enum Reopen {
    /// New source; `true` if it fell back to the primary monitor.
    Ready(WgcSource, bool),
    /// Not possible yet (no monitor, transient error): try again later.
    Retry,
    /// Needs a full rebuild (adapter changed) or cannot continue.
    Fatal(PipelineError),
}

/// The running video thread state: one call to [`VideoPath::step`] per CFR tick.
pub struct VideoPath {
    // Field order is drop order: frames in flight first, then their producers.
    last: Option<HwFrame>,
    /// `None` while the source is being recreated; ticks repeat the last frame.
    src: Option<WgcSource>,
    /// A helper thread rebuilding the source (WGC setup takes ~80 ms; ticks must not
    /// wait for it).
    reopening: Option<crossbeam_channel::Receiver<Reopen>>,
    retry_at_us: i64,
    device: ID3D11Device,
    luid: (u32, i32),
    display: Option<u32>,
    cursor: bool,
    pre: Preprocessor,
    encoder: Encoder,
    frames: HwFrames,
    _hw: HwDevice,
    /// Started on the first `step`, so setup after `open` (audio) costs no ticks.
    pacer: Option<Pacer>,
    sent: u64,
    pub stats: VideoStats,
}

impl VideoPath {
    /// Build the whole path and convert the first frame; the clock starts now.
    pub fn open(opts: &VideoOptions) -> Result<Self> {
        capture::com_init();
        capture::mmcss("Capture"); // video thread must not lose ticks to audio threads
        encode::set_log_quiet(false); // warnings and errors only
        let trace = MemTrace::from_env();
        trace.mark("start");
        let (adapter, output, fell_back) = pick_output_or_primary(opts.display)?;
        let mut preset = opts.preset;
        if preset.width == 0 || preset.height == 0 {
            // Monitor size, rounded down to even (NV12), capped at 8K.
            preset.width = (output.width.clamp(320, 7680) as u32) & !1;
            preset.height = (output.height.clamp(320, 7680) as u32) & !1;
        }
        if output.refresh_hz > 0 {
            preset.fps = preset.fps.min(output.refresh_hz); // plan §1: clamp to refresh rate
        }
        trace.mark("adapters enumerated");
        let (device, context) = capture::create_device(&adapter)?;
        trace.mark("D3D11 device");
        let hw = HwDevice::wrap(&device)?;
        let frames = HwFrames::new(&hw, SwFormat::Nv12, preset.width, preset.height)?;
        trace.mark("FFmpeg hw device + frames");
        let (encoder, codec) = open_encoder(&adapter, preset, opts.codec, opts.quality, &frames)?;
        trace.mark("encoder open");
        let mut pre = Preprocessor::new(&device, &context, preset.width, preset.height, preset.fps)?;
        let mut src = WgcSource::for_monitor(&device, &output, opts.cursor)?;
        let mut notes = Vec::new();
        if fell_back {
            notes.push(format!(
                "Display {} was not found; recording the main monitor.",
                opts.display.unwrap_or(0)
            ));
        }
        if !src.border_removed {
            notes.push("Windows shows a yellow border around the captured screen.".into());
        }
        if opts.codec.is_none() && codec != Codec::Av1 {
            notes.push(format!(
                "AV1 encoding is not available; using {}.",
                codec.name().to_uppercase()
            ));
        }
        trace.mark("WGC session");
        let first = wait_first_frame(&mut src, Duration::from_secs(2))?;
        let last = Some(convert(&mut pre, &frames, &first)?);
        drop(first);
        trace.mark("first frame converted");
        let stats = VideoStats {
            encoder: encoder.name.clone(),
            codec: Some(codec),
            width: preset.width,
            height: preset.height,
            fps: preset.fps,
            source: format!(
                "WGC {} {}x{} @ {} Hz",
                output.device_name, output.width, output.height, output.refresh_hz
            ),
            fresh: 1,
            notes,
            ..Default::default()
        };
        Ok(Self {
            last,
            src: Some(src),
            reopening: None,
            retry_at_us: 0,
            device,
            luid: adapter.luid,
            display: opts.display,
            cursor: opts.cursor,
            pre,
            encoder,
            frames,
            _hw: hw,
            pacer: None,
            sent: 0,
            stats,
        })
    }

    /// The preset after resolving the monitor size and refresh clamp.
    pub fn preset(&self) -> VideoPreset {
        VideoPreset {
            width: self.stats.width,
            height: self.stats.height,
            fps: self.stats.fps,
        }
    }

    pub fn stream_params(&self) -> Result<StreamParams> {
        Ok(self.encoder.stream_params(self.stats.fps)?)
    }

    /// Wait for the next tick, encode one frame (new or repeated) and append any ready
    /// packets to `out`. Returns the tick index.
    pub fn step(&mut self, out: &mut Vec<Packet>) -> Result<i64> {
        let pacer = match &mut self.pacer {
            Some(p) => p,
            None => self.pacer.insert(Pacer::new(self.stats.fps, capture::qpc_now_us())?),
        };
        let tick = pacer.wait()?;
        self.stats.dropped += tick.missed as u64;
        // No B-frames: one packet per frame, so frames sent minus packets out = in flight.
        if self.sent.saturating_sub(self.stats.packets) >= MAX_FRAMES_IN_FLIGHT as u64 {
            self.stats.dropped += 1;
        } else {
            if let Some(f) = self.poll_source()? {
                self.last = Some(convert(&mut self.pre, &self.frames, &f)?);
                self.stats.fresh += 1;
            }
            let frame = self.last.as_ref().expect("first frame converted in open");
            match self.encoder.send(Some(frame), tick.pts_us)? {
                Sent::Accepted => {
                    self.sent += 1;
                    self.stats.frames += 1;
                }
                Sent::Busy => self.stats.dropped += 1,
            }
        }
        self.receive(out)?;
        Ok(tick.index)
    }

    /// Newest frame from the source, recreating it after access loss (UAC, mode
    /// switch, monitor gone: plan §7). The rebuild runs on a helper thread and is
    /// retried every 500 ms; meanwhile ticks repeat the last frame.
    fn poll_source(&mut self) -> Result<Option<capture::Frame>> {
        if fault::fire(fault::Kind::DeviceLost) {
            return Err(CaptureError::DeviceLost(windows::core::Error::from_hresult(
                windows::Win32::Graphics::Dxgi::DXGI_ERROR_DEVICE_REMOVED,
            ))
            .into());
        }
        if fault::fire(fault::Kind::AccessLost) && self.src.is_some() {
            let old = self.src.take();
            self.start_reopen(old);
        }
        if let Some(rx) = &self.reopening {
            match rx.try_recv() {
                Ok(Reopen::Ready(src, fell_back)) => {
                    self.reopening = None;
                    self.src = Some(src);
                    self.stats.source_restarts += 1;
                    if fell_back {
                        let note = format!(
                            "Display {} was disconnected; recording the main monitor.",
                            self.display.unwrap_or(0)
                        );
                        if !self.stats.notes.contains(&note) {
                            self.stats.notes.push(note);
                        }
                    }
                }
                Ok(Reopen::Retry) | Err(crossbeam_channel::TryRecvError::Disconnected) => {
                    self.reopening = None;
                    self.retry_at_us = capture::qpc_now_us() + 500_000;
                    return Ok(None);
                }
                Ok(Reopen::Fatal(e)) => return Err(e),
                Err(crossbeam_channel::TryRecvError::Empty) => return Ok(None),
            }
        }
        let Some(src) = self.src.as_mut() else {
            if capture::qpc_now_us() >= self.retry_at_us {
                self.start_reopen(None);
            }
            return Ok(None);
        };
        match src.next_frame(0) {
            Err(CaptureError::AccessLost) => {
                let old = self.src.take();
                self.start_reopen(old);
                Ok(None)
            }
            other => Ok(other?),
        }
    }

    /// Drop `old` and build a new source on a helper thread.
    fn start_reopen(&mut self, old: Option<WgcSource>) {
        let (tx, rx) = crossbeam_channel::bounded(1);
        let device = capture::SendDevice(self.device.clone());
        let (display, cursor, luid) = (self.display, self.cursor, self.luid);
        let spawned = std::thread::Builder::new()
            .name("suicap-wgc-reopen".into())
            .spawn(move || {
                capture::com_init();
                drop(old);
                let result = match pick_output_or_primary(display) {
                    Ok((adapter, _, _)) if adapter.luid != luid => Reopen::Fatal(PipelineError::AdapterChanged),
                    Ok((_, output, fell_back)) => match WgcSource::for_monitor(&device.0, &output, cursor) {
                        Ok(src) => Reopen::Ready(src, fell_back),
                        Err(e) if e.is_recoverable() || matches!(e, CaptureError::Win(_)) => Reopen::Retry,
                        Err(e) => Reopen::Fatal(e.into()),
                    },
                    Err(PipelineError::NoOutput) => Reopen::Retry, // no monitor at all yet
                    Err(e) => Reopen::Fatal(e),
                };
                let _ = tx.send(result);
            });
        self.reopening = spawned.is_ok().then_some(rx);
        if self.reopening.is_none() {
            self.retry_at_us = capture::qpc_now_us() + 500_000;
        }
    }

    fn receive(&mut self, out: &mut Vec<Packet>) -> Result<bool> {
        let before = out.len();
        let more = self.encoder.receive(VIDEO_STREAM, out)?;
        self.stats.packets += (out.len() - before) as u64;
        self.stats.bytes += out[before..].iter().map(|p| p.data.len() as u64).sum::<u64>();
        Ok(more)
    }

    /// Flush the encoder; remaining packets go to `out`.
    pub fn finish(mut self, out: &mut Vec<Packet>) -> Result<VideoStats> {
        self.last = None;
        self.encoder.send(None, 0)?;
        while self.receive(out)? {}
        Ok(self.stats.clone())
    }
}

// ---------------------------------------------------------------------------
// Audio alongside video.

fn start_audio(opts: Option<&AudioOptions>) -> Result<Option<AudioPath>> {
    let Some(o) = opts else { return Ok(None) };
    let trace = MemTrace::from_env();
    let audio = AudioPath::start(o, VIDEO_STREAM + 1)?;
    trace.mark(&format!("audio started ({} tracks)", audio.streams.len()));
    Ok(Some(audio))
}

/// Video stream 0, then one stream per audio track.
fn mux_streams(video: Arc<StreamParams>, audio: Option<&AudioPath>) -> Vec<MuxStream> {
    let mut streams = vec![MuxStream::video(video)];
    for a in audio.map(|a| a.streams.as_slice()).unwrap_or_default() {
        streams.push(MuxStream {
            params: a.params.clone(),
            title: Some(a.title),
            default: a.default,
        });
    }
    streams
}

fn drain_audio(audio: Option<&AudioPath>, mut sink: impl FnMut(Packet) -> Result<()>) -> Result<()> {
    if let Some(a) = audio {
        for p in a.rx.try_iter() {
            sink(p)?;
        }
    }
    Ok(())
}

/// Stop audio and hand its last packets to `sink`.
fn stop_audio(audio: Option<AudioPath>, mut sink: impl FnMut(Packet) -> Result<()>) -> Result<Option<AudioStats>> {
    let Some(a) = audio else { return Ok(None) };
    let (stats, rx) = a.stop()?;
    for p in rx.try_iter() {
        sink(p)?;
    }
    Ok(Some(stats))
}

// ---------------------------------------------------------------------------
// record

#[derive(Debug, Clone)]
pub struct RecordOptions {
    pub secs: u32,
    pub out: PathBuf,
    pub video: VideoOptions,
    /// `None`: video only.
    pub audio: Option<AudioOptions>,
}

#[derive(Debug, Clone)]
pub struct RecordStats {
    pub path: PathBuf,
    pub video: VideoStats,
    pub audio: Option<AudioStats>,
}

/// `suicap-cli record <secs>`: CFR capture straight to a file.
pub fn record(opts: &RecordOptions) -> Result<RecordStats> {
    let mut v = VideoPath::open(&opts.video)?;
    let audio = start_audio(opts.audio.as_ref())?;
    let mut mux = Muxer::create(&opts.out, &mux_streams(Arc::new(v.stream_params()?), audio.as_ref()))?;
    let total = i64::from(opts.secs) * i64::from(v.stats.fps);
    let mut packets = Vec::new();
    loop {
        let index = v.step(&mut packets)?;
        for p in packets.drain(..) {
            mux.write(&p)?;
        }
        drain_audio(audio.as_ref(), |p| Ok(mux.write(&p)?))?;
        if index + 1 >= total {
            break;
        }
    }
    let video = v.finish(&mut packets)?;
    for p in packets.drain(..) {
        mux.write(&p)?;
    }
    let audio = stop_audio(audio, |p| Ok(mux.write(&p)?))?;
    Ok(RecordStats {
        path: mux.finish()?,
        video,
        audio,
    })
}

/// Default output name in `dir`: `Desktop_<YYYY-MM-DD_HH-mm-ss>.<ext>`.
pub fn default_output_path(dir: &Path, ext: &str) -> PathBuf {
    dir.join(mux::clip_file_name("Desktop", mux::local_time(), ext))
}

// ---------------------------------------------------------------------------
// replay

#[derive(Debug, Clone)]
pub struct ReplayOptions {
    /// Buffer length; the save happens once the buffer has run this long.
    pub buffer_secs: u32,
    pub save_secs: u32,
    pub out: PathBuf,
    pub video: VideoOptions,
    /// `None`: video only.
    pub audio: Option<AudioOptions>,
}

#[derive(Debug, Clone)]
pub struct ReplayStats {
    pub clip: ClipInfo,
    pub video: VideoStats,
    /// Buffer contents when the snapshot was taken.
    pub buffer: crate::buffer::Stats,
    /// Upper bound from plan §4.3 (max bitrate x seconds / 8).
    pub buffer_estimate_mb: u64,
    /// Ticks that kept capturing while the mux worker wrote the clip.
    pub ticks_during_save: u64,
    /// CPU used while armed (buffer filling, no save running), as % of one core.
    pub armed_cpu_pct_one_core: f64,
    pub private_bytes_armed: u64,
    pub private_ws_armed: u64,
    pub armed_secs: f64,
    pub audio: Option<AudioStats>,
}

/// `suicap-cli replay <buffer_s> <save_s>`: arm the buffer, let it fill, save the last
/// `save_secs` on a mux worker while capture continues, then stop.
pub fn replay(opts: &ReplayOptions) -> Result<ReplayStats> {
    let mut v = VideoPath::open(&opts.video)?;
    let audio = start_audio(opts.audio.as_ref())?;
    let streams = mux_streams(Arc::new(v.stream_params()?), audio.as_ref());
    let codec = v.stats.codec.unwrap_or(Codec::Hevc);
    let max_mbps = v.preset().max_bitrate_mbps(codec);
    let estimate_mb = config::buffer_ram_estimate_mb(max_mbps, opts.buffer_secs);
    // Byte cap: twice the max-bitrate estimate, a guard against runaway bitrate only.
    let buf = ReplayBuffer::new(
        opts.buffer_secs,
        (estimate_mb as usize).saturating_mul(2 << 20).max(64 << 20),
    );

    let fill_ticks = i64::from(opts.buffer_secs) * i64::from(v.stats.fps);
    let mut packets = Vec::new();
    let start_usage = capture::process_usage();
    let start_us = capture::qpc_now_us();
    loop {
        let index = v.step(&mut packets)?;
        for p in packets.drain(..) {
            buf.push(p);
        }
        drain_audio(audio.as_ref(), |p| {
            buf.push(p);
            Ok(())
        })?;
        if index + 1 >= fill_ticks {
            break;
        }
    }
    let armed_usage = capture::process_usage();
    let armed_secs = (capture::qpc_now_us() - start_us) as f64 / 1e6;

    let snapshot_stats = buf.stats();
    let worker = mux::spawn_save(opts.out.clone(), streams, buf.snapshot(opts.save_secs));
    let mut ticks_during_save = 0;
    while !worker.is_finished() {
        v.step(&mut packets)?;
        for p in packets.drain(..) {
            buf.push(p);
        }
        drain_audio(audio.as_ref(), |p| {
            buf.push(p);
            Ok(())
        })?;
        ticks_during_save += 1;
    }
    let clip = worker.join().map_err(|_| PipelineError::MuxPanic)??;
    let video = v.finish(&mut packets)?;
    let audio = stop_audio(audio, |_| Ok(()))?;

    Ok(ReplayStats {
        audio,
        clip,
        video,
        buffer: snapshot_stats,
        buffer_estimate_mb: estimate_mb,
        ticks_during_save,
        armed_cpu_pct_one_core: (armed_usage.cpu_us - start_usage.cpu_us) as f64 / (armed_secs * 1e6) * 100.0,
        private_bytes_armed: armed_usage.private_bytes,
        private_ws_armed: armed_usage.private_ws_bytes,
        armed_secs,
    })
}

// ---------------------------------------------------------------------------
// session: the armed replay buffer the app shell drives (plan §3 state machine).

#[derive(Debug, Clone)]
pub struct SessionOptions {
    pub buffer_secs: u32,
    pub video: VideoOptions,
    /// `None`: video only.
    pub audio: Option<AudioOptions>,
    /// Clips go to `<clips_dir>\<App>\<App>_<time>.<ext>`.
    pub clips_dir: PathBuf,
    pub container: config::Container,
}

/// Buffer and capture counters, published about once a second.
#[derive(Debug, Clone, Default)]
pub struct SessionStatus {
    pub buffered_secs: f64,
    pub buffered_bytes: u64,
    pub frames: u64,
    pub dropped: u64,
    /// Upper bound of buffer RAM at the max bitrate (plan §4.3).
    pub buffer_estimate_mb: u64,
}

#[derive(Debug, Clone)]
pub enum SessionEvent {
    /// The capture path is open and the buffer is filling.
    Armed(VideoStats),
    Status(SessionStatus),
    ClipSaved(ClipInfo),
    /// A save that produced no file (buffer still empty, or the write failed).
    SaveFailed(String),
    /// The video path failed and is being rebuilt (plan §7); the buffer was cleared.
    Recovering(String),
    /// The session ended: `None` after `stop`, otherwise the fatal error.
    Stopped(Option<String>),
}

enum SessionCmd {
    /// `None` = the whole buffer.
    Save(Option<u32>),
    Stop,
}

/// A running replay buffer on its own video thread. Dropping it stops capture and
/// waits for saves in progress to finish.
pub struct Session {
    cmd: crossbeam_channel::Sender<SessionCmd>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Session {
    /// Start capturing on a new thread; returns at once. Progress and errors arrive
    /// through `on_event`, called on the session thread (keep it short).
    pub fn start(opts: SessionOptions, on_event: impl Fn(SessionEvent) + Send + 'static) -> std::io::Result<Self> {
        let (cmd, rx) = crossbeam_channel::bounded(16);
        let thread = std::thread::Builder::new().name("suicap-video".into()).spawn(move || {
            let result = run_session(&opts, &rx, &on_event);
            on_event(SessionEvent::Stopped(result.err().map(|e| e.to_string())));
        })?;
        Ok(Self {
            cmd,
            thread: Some(thread),
        })
    }

    /// Save the last `secs` seconds (`None` = all of it). Never blocks; a full
    /// command queue drops the request (plan rule 3).
    pub fn save(&self, secs: Option<u32>) -> bool {
        self.cmd.try_send(SessionCmd::Save(secs)).is_ok()
    }

    pub fn is_running(&self) -> bool {
        self.thread.as_ref().is_some_and(|t| !t.is_finished())
    }

    pub fn stop(mut self) {
        self.shutdown();
    }

    fn shutdown(&mut self) {
        let _ = self.cmd.send(SessionCmd::Stop);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// `<dir>\<App>\<App>_<time>.<ext>`, adding `-2`, `-3`... if two saves share a second.
fn clip_path(dir: &Path, ext: &str) -> std::io::Result<PathBuf> {
    let app = mux::safe_app_name(&crate::audio::foreground_app_name().unwrap_or_default());
    let folder = dir.join(&app);
    std::fs::create_dir_all(&folder)?;
    let name = mux::clip_file_name(&app, mux::local_time(), ext);
    let stem = name.trim_end_matches(&format!(".{ext}")).to_owned();
    let mut path = folder.join(&name);
    let mut n = 2;
    while path.exists() || path.with_extension(format!("{ext}.tmp")).exists() {
        path = folder.join(format!("{stem}-{n}.{ext}"));
        n += 1;
    }
    Ok(path)
}

/// Rebuild attempts after a runtime video failure before giving up (plan §7 row 2).
const REBUILD_ATTEMPTS: u32 = 6;

type Workers = Vec<std::thread::JoinHandle<mux::Result<ClipInfo>>>;

fn reap(workers: &mut Workers, all: bool, on_event: &dyn Fn(SessionEvent)) {
    let mut i = 0;
    while i < workers.len() {
        if all || workers[i].is_finished() {
            match workers.swap_remove(i).join() {
                Ok(Ok(clip)) => on_event(SessionEvent::ClipSaved(clip)),
                Ok(Err(e)) => on_event(SessionEvent::SaveFailed(e.to_string())),
                Err(_) => on_event(SessionEvent::SaveFailed("mux worker panicked".into())),
            }
        } else {
            i += 1;
        }
    }
}

/// Degraded features to show next to the video ones (plan §7 last row).
fn audio_notes(opts: &SessionOptions, audio: Option<&AudioPath>) -> Vec<String> {
    let mut notes = Vec::new();
    if let Some(a) = audio {
        if !a.process_loopback {
            notes.push("Per-app audio tracks are not available here; all desktop sound goes to one track.".into());
        }
        if opts.audio.as_ref().is_some_and(|o| o.mic) && !a.mic_open {
            notes.push("No microphone found; its track stays silent until one is connected.".into());
        }
    }
    notes
}

fn armed_event(v: &VideoPath, extra: &[String]) -> SessionEvent {
    let mut stats = v.stats.clone();
    stats.notes.extend(extra.iter().cloned());
    SessionEvent::Armed(stats)
}

fn run_session(
    opts: &SessionOptions,
    rx: &crossbeam_channel::Receiver<SessionCmd>,
    on_event: &dyn Fn(SessionEvent),
) -> Result<()> {
    let v = VideoPath::open(&opts.video)?;
    let audio = start_audio(opts.audio.as_ref())?;
    let notes = audio_notes(opts, audio.as_ref());
    let mut streams = mux_streams(Arc::new(v.stream_params()?), audio.as_ref());
    let estimate = |v: &VideoPath| {
        let codec = v.stats.codec.unwrap_or(Codec::Hevc);
        config::buffer_ram_estimate_mb(v.preset().max_bitrate_mbps(codec), opts.buffer_secs)
    };
    let mut estimate_mb = estimate(&v);
    let buf = ReplayBuffer::new(
        opts.buffer_secs,
        (estimate_mb as usize).saturating_mul(2 << 20).max(64 << 20),
    );
    on_event(armed_event(&v, &notes));

    let mut workers: Workers = Vec::new();
    let mut packets = Vec::new();
    let mut video = Some(v);
    let result = 'session: loop {
        let v = video.as_mut().expect("video path open while running");
        let fps = i64::from(v.stats.fps.max(1));
        // Run until a command stops us or the video path fails.
        let failure = loop {
            let index = match v.step(&mut packets) {
                Ok(i) => i,
                Err(e) => break e,
            };
            for p in packets.drain(..) {
                buf.push(p);
            }
            if let Err(e) = drain_audio(audio.as_ref(), |p| {
                buf.push(p);
                Ok(())
            }) {
                break 'session Err(e);
            }
            match rx.try_recv() {
                Ok(SessionCmd::Save(secs)) => {
                    let secs = secs.unwrap_or(opts.buffer_secs).min(opts.buffer_secs);
                    let snapshot = buf.snapshot(secs);
                    if snapshot.is_empty() {
                        on_event(SessionEvent::SaveFailed("the replay buffer is still empty".into()));
                    } else {
                        match clip_path(&opts.clips_dir, opts.container.ext()) {
                            Ok(path) => workers.push(mux::spawn_save(path, streams.clone(), snapshot)),
                            Err(e) => on_event(SessionEvent::SaveFailed(format!("clips folder: {e}"))),
                        }
                    }
                }
                Ok(SessionCmd::Stop) | Err(crossbeam_channel::TryRecvError::Disconnected) => break 'session Ok(()),
                Err(crossbeam_channel::TryRecvError::Empty) => {}
            }
            reap(&mut workers, false, on_event);
            if index % fps == 0 {
                let b = buf.stats();
                on_event(SessionEvent::Status(SessionStatus {
                    buffered_secs: b.span_us as f64 / 1e6,
                    buffered_bytes: b.bytes as u64,
                    frames: v.stats.frames,
                    dropped: v.stats.dropped,
                    buffer_estimate_mb: estimate_mb,
                }));
            }
        };

        // Plan §7 row 2: rebuild device + encoder, flush the buffer (stream parameters
        // change), back off between attempts. Audio keeps running; its packets are
        // dropped by the empty buffer until the first new keyframe.
        on_event(SessionEvent::Recovering(failure.to_string()));
        video = None;
        buf.flush();
        let mut attempt = 0;
        let v = loop {
            attempt += 1;
            let wait = Duration::from_millis(500 << (attempt - 1).min(4)); // 0.5 .. 8 s
            let deadline = std::time::Instant::now() + wait;
            while let Some(left) = deadline.checked_duration_since(std::time::Instant::now()) {
                match rx.recv_timeout(left.min(Duration::from_millis(50))) {
                    Ok(SessionCmd::Stop) | Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
                        break 'session Ok(());
                    }
                    Ok(SessionCmd::Save(_)) => on_event(SessionEvent::SaveFailed(
                        "capture is restarting; nothing to save yet".into(),
                    )),
                    Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
                }
                let _ = drain_audio(audio.as_ref(), |_| Ok(()));
                reap(&mut workers, false, on_event);
            }
            match VideoPath::open(&opts.video) {
                Ok(new) => break new,
                Err(e) if attempt >= REBUILD_ATTEMPTS => break 'session Err(e),
                Err(e) => on_event(SessionEvent::Recovering(format!("attempt {attempt} failed: {e}"))),
            }
        };
        match v.stream_params() {
            Ok(params) => streams = mux_streams(Arc::new(params), audio.as_ref()),
            Err(e) => break 'session Err(e),
        }
        estimate_mb = estimate(&v);
        on_event(armed_event(&v, &notes));
        video = Some(v);
    };
    // Saves already started still finish (they hold their own packet handles).
    reap(&mut workers, true, on_event);
    let finished = video.map(|v| v.finish(&mut packets)).transpose();
    let stopped = stop_audio(audio, |_| Ok(()));
    result?;
    finished?;
    stopped?;
    Ok(())
}
