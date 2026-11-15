//! Video thread wiring (plan §3): capture -> blt -> send_frame/receive_packet -> sink.
//! `record` sinks packets into a file; `replay` into the RAM replay buffer, saving
//! snapshots on a mux worker while capture keeps running.

use std::{
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering},
    },
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
    fault, game,
    mux::{self, ClipInfo, MuxError, MuxStream, Muxer},
    preprocess::{PreprocessError, Preprocessor},
    shot, targets,
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
    /// Record the display, one application's window, or follow the full-screen game.
    pub source: config::SourceSettings,
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
            source: config::SourceSettings::default(),
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
            source: s.source.clone(),
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
    pub source_width: u32,
    pub source_height: u32,
    pub fps: u32,
    pub source: String,
    /// What is being recorded now: "display", "window" or "waiting" (held frame).
    pub capture_kind: &'static str,
    /// The recorded window's program and title, while `capture_kind` is "window".
    pub capture_exe: Option<String>,
    pub capture_title: Option<String>,
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

/// What the GPU that captures `display` can encode, found by opening each hardware encoder.
#[derive(Debug, Clone, serde::Serialize)]
pub struct CodecSupport {
    /// The GPU name, e.g. "NVIDIA GeForce RTX 4070".
    pub gpu: String,
    /// Codecs whose encoder opened, best first. Empty when the GPU has none.
    pub codecs: Vec<Codec>,
    /// Why nothing is available, when that is the case.
    pub problem: Option<String>,
}

/// Probe the encoders of the GPU behind `display` (`None` = the main monitor). Takes a moment
/// (it opens each encoder once), so callers should cache the answer.
pub fn codec_support(display: Option<u32>) -> std::result::Result<CodecSupport, String> {
    capture::com_init();
    let (adapter, _, _) = pick_output_or_primary(display).map_err(|e| e.to_string())?;
    let gpu = adapter.name.clone();
    if matches!(adapter.vendor, capture::Vendor::Other(_)) {
        return Ok(CodecSupport {
            gpu,
            codecs: Vec::new(),
            problem: Some(
                "SuiCap records with the GPU's own video encoder, and it supports NVIDIA, AMD and Intel GPUs only."
                    .into(),
            ),
        });
    }
    let (device, _) = capture::create_device(&adapter).map_err(|e| e.to_string())?;
    encode::set_log_quiet(true);
    let found = crate::probe::probe_encoders(adapter.vendor, &device);
    encode::set_log_quiet(false);
    let codecs: Vec<Codec> = found.iter().filter(|e| e.result.is_ok()).map(|e| e.codec).collect();
    let problem = codecs
        .is_empty()
        .then(|| "No video encoder opened on this GPU. Update its graphics driver and try again.".to_string());
    Ok(CodecSupport { gpu, codecs, problem })
}

/// Open the first codec that works, in plan order (plan §4.3).
fn open_encoder(
    adapter: &Adapter,
    preset: VideoPreset,
    pinned: Option<Codec>,
    quality: u8,
    frames: &HwFrames,
) -> Result<(Encoder, Codec)> {
    // A pinned codec goes first; if this GPU cannot do it, fall back to the others rather than
    // failing to record at all (the settings page greys out what the GPU cannot encode).
    let auto = Codec::PREFERENCE
        .into_iter()
        .filter(|c| *c != Codec::H264 || preset.allows_h264());
    let candidates: Vec<Codec> = match pinned {
        Some(c) => std::iter::once(c).chain(auto.filter(|x| *x != c)).collect(),
        None => auto.collect(),
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

/// What the video source is pointed at right now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    Display,
    Window {
        hwnd: isize,
        exe: String,
        title: String,
    },
    /// The chosen app is not there and there is no fallback: the last frame is held.
    Waiting,
}

/// What the source setting means right now. `current` keeps an automatic pick on its game
/// while that window is still there, so glancing at another window does not flip the source.
/// `game` is the detected game ([`game::detect`]); only the automatic source uses it.
fn resolve(src: &config::SourceSettings, current: Option<&Target>, game: Option<game::Game>) -> Target {
    use config::SourceKind;
    let window = |w: targets::WindowInfo| Target::Window {
        hwnd: w.hwnd,
        exe: w.exe,
        title: w.title,
    };
    match src.kind {
        SourceKind::Display => Target::Display,
        SourceKind::App => match targets::find_window(&src.exe, &src.title) {
            Some(w) => window(w),
            None if src.fallback => Target::Display,
            None => Target::Waiting,
        },
        SourceKind::Auto => {
            if let Some(Target::Window { hwnd, .. }) = current
                && let Some(w) = targets::list_windows().into_iter().find(|w| w.hwnd == *hwnd)
            {
                return window(w);
            }
            match game {
                Some(g) => window(g.window),
                None => Target::Display,
            }
        }
    }
}

/// Re-resolves the source setting and looks for the game once a second on its own thread
/// (listing windows is too slow for a video tick); the video thread just reads the answers.
struct Watch {
    latest: Arc<Mutex<Target>>,
    /// A game fills a monitor (exclusive, borderless or windowed fullscreen).
    game_fullscreen: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
}

impl Watch {
    fn start(src: config::SourceSettings, initial: Target, game_fullscreen: bool) -> Self {
        let latest = Arc::new(Mutex::new(initial));
        let full = Arc::new(AtomicBool::new(game_fullscreen));
        let stop = Arc::new(AtomicBool::new(false));
        let (l, f, s) = (latest.clone(), full.clone(), stop.clone());
        let _ = std::thread::Builder::new()
            .name("suicap-source-watch".into())
            .spawn(move || {
                while !s.load(Ordering::Acquire) {
                    std::thread::sleep(Duration::from_millis(1000));
                    let found = game::detect();
                    f.store(
                        found.as_ref().is_some_and(|g| g.mode.is_fullscreen()),
                        Ordering::Release,
                    );
                    if src.kind == config::SourceKind::Display {
                        continue;
                    }
                    let cur = l.lock().map(|g| g.clone()).unwrap_or(Target::Display);
                    let next = resolve(&src, Some(&cur), found);
                    if let Ok(mut g) = l.lock() {
                        *g = next;
                    }
                }
            });
        Self {
            latest,
            game_fullscreen: full,
            stop,
        }
    }

    fn latest(&self) -> Target {
        self.latest.lock().map(|g| g.clone()).unwrap_or(Target::Display)
    }

    fn game_fullscreen(&self) -> bool {
        self.game_fullscreen.load(Ordering::Acquire)
    }
}

impl Drop for Watch {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        targets::set_active_app(None);
    }
}

/// Fill the stats the UI shows about the source.
fn describe(stats: &mut VideoStats, active: &Target, src: &config::SourceSettings, display_label: &str) {
    if !matches!(active, Target::Window { .. }) {
        targets::set_active_app(None);
    }
    match active {
        Target::Display => {
            stats.capture_kind = "display";
            stats.capture_exe = None;
            stats.capture_title = None;
            stats.source = display_label.to_owned();
        }
        Target::Window { exe, title, .. } => {
            targets::set_active_app(Some(exe.clone()));
            stats.capture_kind = "window";
            stats.capture_exe = Some(exe.clone());
            stats.capture_title = Some(title.clone());
            stats.source = format!("WGC window {exe}");
        }
        Target::Waiting => {
            stats.capture_kind = "waiting";
            stats.capture_exe = Some(src.exe.clone());
            stats.capture_title = None;
            stats.source = format!("waiting for {}", src.exe);
        }
    }
}

/// The adapter and output of the monitor a window is on, so the GPU that shows the game also
/// converts it; the chosen display if that monitor cannot be matched.
fn pick_output_for_window(hwnd: isize, display: Option<u32>) -> Result<(Adapter, Output, bool)> {
    use windows::Win32::{
        Foundation::HWND,
        Graphics::Gdi::{MONITOR_DEFAULTTONEAREST, MonitorFromWindow},
    };
    // SAFETY: a stale handle just yields the nearest monitor.
    let monitor = unsafe { MonitorFromWindow(HWND(hwnd as *mut _), MONITOR_DEFAULTTONEAREST) };
    for a in capture::adapters()?.into_iter().filter(|a| !a.software) {
        if let Some(o) = capture::outputs(&a)?.into_iter().find(|o| o.monitor == monitor) {
            return Ok((a, o, false));
        }
    }
    pick_output_or_primary(display)
}

/// Result of a capture-source rebuild on the helper thread.
enum Reopen {
    /// New source; `true` if it fell back to the primary monitor.
    Ready(WgcSource, bool),
    /// Not possible yet (no monitor, transient error): try again later.
    Retry,
    /// The window exists but Windows would not let it be captured (e.g. it runs elevated).
    WindowFailed(isize, String),
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
    /// What the user asked to record, the target it resolves to now, and the watcher that
    /// re-resolves it every second (app starts, closes, goes to the background).
    source_cfg: config::SourceSettings,
    active: Target,
    watch: Watch,
    /// Stall watchdog: when the source last delivered a frame, when it may next be recreated
    /// for silence, and how many times in a row it was (the wait doubles each time).
    last_fresh_us: i64,
    stall_next_us: i64,
    stall_tries: u32,
    /// A window Windows refused to capture; not retried while it stays the same window.
    bad_window: Option<isize>,
    /// The stats' source text while the display is recorded.
    display_label: String,
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
        let found = game::detect();
        let game_fullscreen = found.as_ref().is_some_and(|g| g.mode.is_fullscreen());
        let first_target = resolve(&opts.source, None, found);
        let (adapter, output, fell_back) = match &first_target {
            Target::Window { hwnd, .. } => pick_output_for_window(*hwnd, opts.display)?,
            _ => pick_output_or_primary(opts.display)?,
        };
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
        let mut notes = Vec::new();
        let mut active = first_target.clone();
        let mut bad_window = None;
        let mut src = match &first_target {
            Target::Window { hwnd, exe, .. } => match WgcSource::for_window(&device, *hwnd, opts.cursor) {
                Ok(s) => s,
                Err(e) => {
                    notes.push(format!("Could not record {exe} ({e}); recording the display instead."));
                    bad_window = Some(*hwnd);
                    active = Target::Display;
                    WgcSource::for_monitor(&device, &output, opts.cursor)?
                }
            },
            // Waiting starts on the display for its first frame, then holds it.
            _ => WgcSource::for_monitor(&device, &output, opts.cursor)?,
        };
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
        let (first, source_dims) = match wait_first_frame(&mut src, Duration::from_secs(2)) {
            Ok(f) => {
                let dims = (f.width, f.height);
                (f, dims)
            }
            // An exclusive full-screen game can hold the monitor's frames back: start on black
            // instead of failing to arm; the stall watchdog recreates the source.
            Err(PipelineError::NoFirstFrame(_)) => (
                capture::Frame::black(&device)?,
                (output.width.max(1) as u32, output.height.max(1) as u32),
            ),
            Err(e) => return Err(e),
        };
        let last = Some(convert(&mut pre, &frames, &first)?);
        drop(first);
        trace.mark("first frame converted");
        let stats = VideoStats {
            encoder: encoder.name.clone(),
            codec: Some(codec),
            width: preset.width,
            height: preset.height,
            source_width: source_dims.0,
            source_height: source_dims.1,
            fps: preset.fps,
            source: format!(
                "WGC {} {}x{} @ {} Hz",
                output.device_name, output.width, output.height, output.refresh_hz
            ),
            fresh: 1,
            notes,
            ..Default::default()
        };
        let mut stats = stats;
        let display_label = stats.source.clone();
        describe(&mut stats, &active, &opts.source, &display_label);
        Ok(Self {
            last,
            src: Some(src),
            reopening: None,
            retry_at_us: 0,
            device,
            luid: adapter.luid,
            display: opts.display,
            cursor: opts.cursor,
            watch: Watch::start(opts.source.clone(), active.clone(), game_fullscreen),
            last_fresh_us: capture::qpc_now_us(),
            stall_next_us: 0,
            stall_tries: 0,
            source_cfg: opts.source.clone(),
            active,
            bad_window,
            display_label,
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

    /// Read the newest encoded-size frame back as RGBA (plan rule 1's one exception).
    pub fn screenshot(&self) -> std::result::Result<shot::Image, String> {
        let frame = self.last.as_ref().ok_or("no frame captured yet")?;
        let (tex, slice) = frame.texture();
        // SAFETY: the device is live; the immediate context is only used on this thread.
        let ctx = unsafe { self.device.GetImmediateContext() }.map_err(|e| e.to_string())?;
        shot::read_nv12(&self.device, &ctx, &tex, slice, self.stats.width, self.stats.height).map_err(|e| e.to_string())
    }

    /// Read a scaled preview of the newest encoded NV12 frame alongside live stats.
    pub fn preview(&self, max_w: u32) -> std::result::Result<(shot::Image, VideoStats), String> {
        let frame = self.last.as_ref().ok_or("no frame captured yet")?;
        let (tex, slice) = frame.texture();
        // SAFETY: the device is live; the immediate context is only used on this thread.
        let ctx = unsafe { self.device.GetImmediateContext() }.map_err(|e| e.to_string())?;
        let img = shot::read_nv12_scaled(
            &self.device,
            &ctx,
            &tex,
            slice,
            self.stats.width,
            self.stats.height,
            max_w,
        )
        .map_err(|e| e.to_string())?;
        Ok((img, self.stats.clone()))
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
                self.stats.source_width = f.width;
                self.stats.source_height = f.height;
                self.last = Some(convert(&mut self.pre, &self.frames, &f)?);
                self.stats.fresh += 1;
                self.last_fresh_us = tick.pts_us;
                self.stall_tries = 0;
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
        self.follow_target();
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
                Ok(Reopen::WindowFailed(hwnd, why)) => {
                    self.reopening = None;
                    self.bad_window = Some(hwnd);
                    if let Target::Window { exe, .. } = &self.active {
                        self.stats.notes.push(format!(
                            "Could not record {exe} ({why}); recording the display instead."
                        ));
                    }
                    self.active = Target::Display;
                    self.start_reopen(None);
                    return Ok(None);
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
        if self.active == Target::Waiting {
            return Ok(None); // holding the last frame until the app is back
        }
        if self.src.is_some() && self.stalled() {
            let old = self.src.take();
            self.start_reopen(old);
            return Ok(None);
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

    /// The source has sent nothing for a while although a game fills a monitor: a switch into
    /// or out of exclusive fullscreen (or a display mode change) can leave a capture session
    /// silent without closing it. Recreating it then is cheap and runs off the video thread; a
    /// still screen merely gets recreated less and less often (2 s, 4 s, ... up to 32 s).
    fn stalled(&mut self) -> bool {
        const STALL_US: i64 = 2_000_000;
        let now = capture::qpc_now_us();
        if now - self.last_fresh_us < STALL_US || now < self.stall_next_us || !self.watch.game_fullscreen() {
            return false;
        }
        self.stall_tries += 1;
        self.stall_next_us = now + (STALL_US << self.stall_tries.min(4));
        true
    }

    /// Move the source when what the setting means has changed: the app started or closed,
    /// or the full-screen game changed.
    fn follow_target(&mut self) {
        let mut want = self.watch.latest();
        if let Target::Window { hwnd, .. } = &want
            && self.bad_window == Some(*hwnd)
        {
            want = if self.source_cfg.fallback {
                Target::Display
            } else {
                Target::Waiting
            };
        }
        if want == self.active {
            return;
        }
        let was_window = matches!(self.active, Target::Window { .. });
        self.active = want;
        self.reopening = None; // a rebuild for the old target is no longer wanted
        let old = self.src.take();
        self.stats.notes.retain(|n| !n.starts_with("Could not record"));
        if let Target::Window { hwnd, .. } = &self.active
            && self.bad_window.is_some_and(|b| b != *hwnd)
        {
            self.bad_window = None; // a different window: worth trying
        }
        self.describe_active();
        if self.active == Target::Waiting {
            drop(old);
            self.stats.source_restarts += u64::from(was_window);
        } else {
            self.start_reopen(old);
        }
    }

    fn describe_active(&mut self) {
        describe(&mut self.stats, &self.active, &self.source_cfg, &self.display_label);
    }

    /// Drop `old` and build a new source for the active target on a helper thread.
    fn start_reopen(&mut self, old: Option<WgcSource>) {
        let (tx, rx) = crossbeam_channel::bounded(1);
        let device = capture::SendDevice(self.device.clone());
        let (display, cursor, luid) = (self.display, self.cursor, self.luid);
        let target = self.active.clone();
        let spawned = std::thread::Builder::new()
            .name("suicap-wgc-reopen".into())
            .spawn(move || {
                capture::com_init();
                drop(old);
                let result = match target {
                    Target::Window { hwnd, .. } => match WgcSource::for_window(&device.0, hwnd, cursor) {
                        Ok(src) => Reopen::Ready(src, false),
                        Err(_) if !targets::window_exists(hwnd) => Reopen::Retry,
                        Err(e) if e.is_recoverable() => Reopen::Retry,
                        Err(e) => Reopen::WindowFailed(hwnd, e.to_string()),
                    },
                    _ => match pick_output_or_primary(display) {
                        Ok((adapter, _, _)) if adapter.luid != luid => Reopen::Fatal(PipelineError::AdapterChanged),
                        Ok((_, output, fell_back)) => match WgcSource::for_monitor(&device.0, &output, cursor) {
                            Ok(src) => Reopen::Ready(src, fell_back),
                            Err(e) if e.is_recoverable() || matches!(e, CaptureError::Win(_)) => Reopen::Retry,
                            Err(e) => Reopen::Fatal(e.into()),
                        },
                        Err(PipelineError::NoOutput) => Reopen::Retry, // no monitor at all yet
                        Err(e) => Reopen::Fatal(e),
                    },
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
    pub mode: config::RecordingMode,
    pub continuous: config::ContinuousSettings,
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
    pub recording: bool,
    pub recording_paused: bool,
    pub waiting_for_source: bool,
    /// Active recording seconds across the current session (excluding paused time).
    pub recording_secs: f64,
    /// Active seconds and approximate encoded bytes of the current file segment.
    pub segment_secs: f64,
    pub segment_bytes: u64,
    /// Total encoded bytes across the active continuous session.
    pub session_bytes: u64,
    pub segment_index: u32,
    /// What the video source is pointed at now, and the notes about it (these change live).
    pub capture_kind: &'static str,
    pub capture_exe: Option<String>,
    pub capture_title: Option<String>,
    pub source_width: u32,
    pub source_height: u32,
    pub source: String,
    pub notes: Vec<String>,
}

#[derive(Debug, Clone)]
pub enum SessionEvent {
    /// The capture path is open and the buffer is filling.
    Armed(VideoStats),
    Status(SessionStatus),
    ClipSaved(ClipInfo),
    /// A save that produced no file (buffer still empty, or the write failed).
    SaveFailed(String),
    RecordingStarted,
    /// Pause state changed (manual or idle auto-pause).
    RecordingPaused(bool),
    /// Intermediate segment finished on auto-split, manual split, app change, or waited source closing.
    SegmentSaved(ClipInfo),
    RecordingSaved(ClipInfo),
    /// Recording ended automatically due to max duration limit or low disk space after saving.
    RecordingStoppedReason(String),
    RecordingFailed(String),
    ScreenshotTaken(PathBuf),
    ScreenshotSaved(PathBuf),
    ScreenshotFailed(String),
    /// The video path failed and is being rebuilt (plan §7); the buffer was cleared.
    Recovering(String),
    /// The session ended: `None` after `stop`, otherwise the fatal error.
    Stopped(Option<String>),
}

enum SessionCmd {
    /// `None` = the whole buffer.
    Save(Option<u32>),
    RecordStart,
    RecordStop,
    RecordTogglePause,
    RecordSplit,
    UpdateContinuous(config::ContinuousSettings),
    Preview(
        u32,
        crossbeam_channel::Sender<std::result::Result<(shot::Image, VideoStats), String>>,
    ),
    Screenshot,
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

    /// Start continuous recording to disk. Never blocks; a full command queue
    /// drops the request (plan rule 3).
    pub fn record_start(&self) -> bool {
        self.cmd.try_send(SessionCmd::RecordStart).is_ok()
    }

    /// Stop continuous recording and finalize the file. Never blocks; a full
    /// command queue drops the request.
    pub fn record_stop(&self) -> bool {
        self.cmd.try_send(SessionCmd::RecordStop).is_ok()
    }

    /// Toggle pause on an active recording. Never blocks.
    pub fn record_toggle_pause(&self) -> bool {
        self.cmd.try_send(SessionCmd::RecordTogglePause).is_ok()
    }

    /// Cut the current continuous segment at the next keyframe and start the next file. Never blocks.
    pub fn record_split(&self) -> bool {
        self.cmd.try_send(SessionCmd::RecordSplit).is_ok()
    }

    /// Apply updated continuous settings live without restarting capture. Never blocks.
    pub fn update_continuous(&self, continuous: config::ContinuousSettings) -> bool {
        self.cmd.try_send(SessionCmd::UpdateContinuous(continuous)).is_ok()
    }

    /// Save the newest frame as a PNG in the clips folder. Never blocks.
    pub fn screenshot(&self) -> bool {
        self.cmd.try_send(SessionCmd::Screenshot).is_ok()
    }

    /// Fetch a scaled preview of the newest captured frame from the running video thread.
    pub fn preview(&self, max_w: u32) -> std::result::Result<(shot::Image, VideoStats), String> {
        let (tx, rx) = crossbeam_channel::bounded(1);
        self.cmd
            .try_send(SessionCmd::Preview(max_w, tx))
            .map_err(|_| "preview busy".to_string())?;
        rx.recv_timeout(Duration::from_millis(450))
            .map_err(|_| "preview timed out".to_string())?
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
    while path.exists() || mux::temp_path(&path).is_ok_and(|t| t.exists()) {
        path = folder.join(format!("{stem}-{n}.{ext}"));
        n += 1;
    }
    Ok(path)
}

/// Output `(mkv_path, mp4_path)` for a continuous or manual recording segment, checking
/// collisions against both `.mkv` and `.mp4` targets and their `.tmp` files.
fn segment_clip_path(
    dir: &Path,
    app: &str,
    part: Option<u32>,
    container: config::Container,
) -> std::io::Result<(PathBuf, Option<PathBuf>)> {
    let app = mux::safe_app_name(app);
    let folder = dir.join(&app);
    std::fs::create_dir_all(&folder)?;
    let name = mux::segment_file_name(&app, mux::local_time(), part, "mkv");
    let stem = name.trim_end_matches(".mkv").to_owned();
    let tmp_root = (container == config::Container::Mp4).then(mux::temp_dir).flatten();
    if let Some(ref tr) = tmp_root {
        std::fs::create_dir_all(tr)?;
    }
    let busy = |s: &str| {
        let mkv = tmp_root.as_deref().unwrap_or(&folder).join(format!("{s}.mkv"));
        let mp4 = folder.join(format!("{s}.mp4"));
        mkv.exists()
            || mux::temp_path(&mkv).is_ok_and(|t| t.exists())
            || (container == config::Container::Mp4 && (mp4.exists() || mux::temp_path(&mp4).is_ok_and(|t| t.exists())))
    };
    let mut candidate = stem.clone();
    let mut n = 2;
    while busy(&candidate) {
        candidate = format!("{stem}-{n}");
        n += 1;
    }
    let mkv_path = tmp_root.as_deref().unwrap_or(&folder).join(format!("{candidate}.mkv"));
    let mp4_path = (container == config::Container::Mp4).then(|| folder.join(format!("{candidate}.mp4")));
    Ok((mkv_path, mp4_path))
}

/// Rebuild attempts after a runtime video failure before giving up (plan §7 row 2).
const REBUILD_ATTEMPTS: u32 = 6;
const NO_KEYFRAME_ERR: &str = "no video keyframe received before recording stopped";

type Workers = Vec<std::thread::JoinHandle<mux::Result<ClipInfo>>>;
type RecWorkers = Vec<(
    std::thread::JoinHandle<std::result::Result<ClipInfo, String>>,
    Option<String>,
    bool,
)>;

type Shots = Vec<std::thread::JoinHandle<std::result::Result<PathBuf, String>>>;

fn reap_shots(shots: &mut Shots, all: bool, on_event: &dyn Fn(SessionEvent)) {
    let mut i = 0;
    while i < shots.len() {
        if all || shots[i].is_finished() {
            match shots.swap_remove(i).join() {
                Ok(Ok(path)) => on_event(SessionEvent::ScreenshotSaved(path)),
                Ok(Err(e)) => on_event(SessionEvent::ScreenshotFailed(e)),
                Err(_) => on_event(SessionEvent::ScreenshotFailed("the screenshot worker crashed".into())),
            }
        } else {
            i += 1;
        }
    }
}

const RECORD_QUEUE_CAPACITY: usize = 512;

enum RecMsg {
    Packet(Packet),
    SetPaused(bool),
}

#[derive(Default)]
struct RecorderStats {
    active_us: AtomicI64,
    bytes: AtomicU64,
    packets: AtomicU64,
}

struct Recorder {
    tx: crossbeam_channel::Sender<RecMsg>,
    thread: std::thread::JoinHandle<std::result::Result<ClipInfo, String>>,
    stats: Arc<RecorderStats>,
    app: String,
    paused: bool,
    index: u32,
}

impl Recorder {
    fn active_us(&self) -> i64 {
        self.stats.active_us.load(Ordering::Relaxed)
    }

    fn bytes(&self) -> u64 {
        self.stats.bytes.load(Ordering::Relaxed)
    }

    fn packets(&self) -> u64 {
        self.stats.packets.load(Ordering::Relaxed)
    }
}

fn spawn_recorder(
    mkv_path: PathBuf,
    mp4_path: Option<PathBuf>,
    streams: Vec<MuxStream>,
    app: String,
    index: u32,
) -> std::io::Result<Recorder> {
    let (tx, rx) = crossbeam_channel::bounded::<RecMsg>(RECORD_QUEUE_CAPACITY);
    let stats = Arc::new(RecorderStats::default());
    let worker_stats = Arc::clone(&stats);
    let thread = std::thread::Builder::new()
        .name("suicap-record".into())
        .spawn(move || {
            let mut mux = Muxer::create(&mkv_path, &streams).map_err(|e| e.to_string())?;
            for msg in rx.iter() {
                match msg {
                    RecMsg::SetPaused(paused) => mux.set_paused(paused),
                    RecMsg::Packet(p) => mux.write(&p).map_err(|e| e.to_string())?,
                }
                worker_stats
                    .active_us
                    .store(mux.active_duration_us(), Ordering::Relaxed);
                worker_stats.bytes.store(mux.bytes, Ordering::Relaxed);
                worker_stats.packets.store(mux.packets, Ordering::Relaxed);
            }
            let packets = mux.packets;
            let active_us = mux.active_duration_us();
            if packets == 0 {
                drop(mux);
                return Err(NO_KEYFRAME_ERR.into());
            }
            let mkv = mux.finish().map_err(|e| e.to_string())?;
            let final_path = match mp4_path {
                Some(mp4) => {
                    if let Err(e) = mux::remux_to_mp4(&mkv, &mp4) {
                        // Keep the .mkv on failure so nothing recorded is lost.
                        return Err(format!("remux to mp4 failed (kept {}): {e}", mkv.display()));
                    }
                    let _ = std::fs::remove_file(&mkv);
                    mp4
                }
                None => mkv,
            };
            Ok(ClipInfo {
                path: final_path,
                packets,
                first_pts_us: 0,
                last_pts_us: active_us,
            })
        })?;
    Ok(Recorder {
        tx,
        thread,
        stats,
        app,
        paused: false,
        index,
    })
}

fn stop_recorder(
    recorder: &mut Option<Recorder>,
    rec_workers: &mut RecWorkers,
    err_override: Option<String>,
    is_split: bool,
) {
    if let Some(rec) = recorder.take() {
        drop(rec.tx);
        rec_workers.push((rec.thread, err_override, is_split));
    }
}

fn send_record_msg(recorder: &mut Option<Recorder>, rec_workers: &mut RecWorkers, msg: RecMsg, is_split: bool) {
    let Some(rec) = recorder.as_ref() else {
        return;
    };
    match rec.tx.try_send(msg) {
        Ok(()) => {}
        Err(crossbeam_channel::TrySendError::Full(_)) => {
            stop_recorder(
                recorder,
                rec_workers,
                Some("recording writer fell behind; stopped recording".into()),
                is_split,
            );
        }
        Err(crossbeam_channel::TrySendError::Disconnected(_)) => {
            stop_recorder(recorder, rec_workers, None, is_split);
        }
    }
}

fn send_record_packet(recorder: &mut Option<Recorder>, rec_workers: &mut RecWorkers, p: Packet) {
    if recorder.as_ref().is_some_and(|r| !r.paused) {
        send_record_msg(recorder, rec_workers, RecMsg::Packet(p), false);
    }
}

fn reap_recordings(workers: &mut RecWorkers, all: bool, on_event: &dyn Fn(SessionEvent)) {
    let mut i = 0;
    while i < workers.len() {
        if all || workers[i].0.is_finished() {
            let (handle, err_override, is_split) = workers.swap_remove(i);
            let res = handle.join();
            if let Some(msg) = err_override {
                on_event(SessionEvent::RecordingFailed(msg));
            } else {
                match res {
                    Ok(Ok(clip)) => {
                        if is_split {
                            on_event(SessionEvent::SegmentSaved(clip));
                        } else {
                            on_event(SessionEvent::RecordingSaved(clip));
                        }
                    }
                    Ok(Err(e)) if is_split && e == NO_KEYFRAME_ERR => {}
                    Ok(Err(e)) => on_event(SessionEvent::RecordingFailed(e)),
                    Err(_) => on_event(SessionEvent::RecordingFailed("recording worker panicked".into())),
                }
            }
        } else {
            i += 1;
        }
    }
}

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
            if opts.audio.as_ref().is_some_and(|o| o.muting) {
                notes.push(
                    "Muted apps are still heard: switching apps off needs per-app audio (Windows 10 version 2004 or newer).".into(),
                );
            }
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

fn app_stem(exe: &str) -> &str {
    exe.strip_suffix(".exe")
        .or_else(|| exe.strip_suffix(".EXE"))
        .unwrap_or(exe)
}

fn current_app(stats: &VideoStats) -> String {
    let raw = if stats.capture_kind == "window" {
        stats.capture_exe.as_deref().map(app_stem).map(str::to_owned)
    } else {
        crate::audio::foreground_app_name()
    };
    mux::safe_app_name(&raw.unwrap_or_default())
}

fn source_ready_for_continuous(source: &config::SourceSettings, wait_for_source: bool, capture_kind: &str) -> bool {
    if !wait_for_source {
        return true;
    }
    match source.kind {
        config::SourceKind::Display => true,
        config::SourceKind::App => capture_kind == "window" || (source.fallback && capture_kind == "display"),
        config::SourceKind::Auto => capture_kind == "window",
    }
}

fn idle_seconds() -> Option<u32> {
    use windows::Win32::System::SystemInformation::GetTickCount;
    use windows::Win32::UI::Input::KeyboardAndMouse::{GetLastInputInfo, LASTINPUTINFO};
    let mut lii = LASTINPUTINFO {
        cbSize: size_of::<LASTINPUTINFO>() as u32,
        dwTime: 0,
    };
    // SAFETY: lii.cbSize matches LASTINPUTINFO size.
    unsafe {
        if !GetLastInputInfo(&mut lii).as_bool() {
            return None;
        }
        Some(GetTickCount().wrapping_sub(lii.dwTime) / 1000)
    }
}

fn free_disk_bytes(dir: &Path) -> Option<u64> {
    use std::os::windows::ffi::OsStrExt;
    use windows::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
    let mut probe = Some(dir);
    let existing = loop {
        match probe {
            Some(p) if p.exists() => break p,
            Some(p) => probe = p.parent(),
            None => return None,
        }
    };
    let wide: Vec<u16> = existing.as_os_str().encode_wide().chain([0]).collect();
    let mut free_avail = 0u64;
    // SAFETY: wide is NUL-terminated UTF-16 pointing to an existing path; free_avail is valid output storage.
    unsafe { GetDiskFreeSpaceExW(windows::core::PCWSTR(wide.as_ptr()), Some(&mut free_avail), None, None) }.ok()?;
    Some(free_avail)
}

struct ContinuousRun {
    is_continuous: bool,
    continuous: config::ContinuousSettings,
    auto_record: bool,
    manual_paused: bool,
    idle_paused: bool,
    waiting_for_source: bool,
    prior_active_us: i64,
    prior_bytes: u64,
    segment_index: u32,
    split_pending: bool,
}

impl ContinuousRun {
    fn new(opts: &SessionOptions) -> Self {
        let is_continuous = opts.mode == config::RecordingMode::Continuous;
        Self {
            is_continuous,
            continuous: opts.continuous.clone(),
            auto_record: is_continuous,
            manual_paused: false,
            idle_paused: false,
            waiting_for_source: false,
            prior_active_us: 0,
            prior_bytes: 0,
            segment_index: 0,
            split_pending: false,
        }
    }

    fn keep_buffer(&self) -> bool {
        !self.is_continuous || self.continuous.keep_replay_buffer
    }

    fn effective_paused(&self) -> bool {
        self.manual_paused || self.idle_paused
    }

    fn accumulate_and_stop(&mut self, recorder: &mut Option<Recorder>, rec_workers: &mut RecWorkers, is_split: bool) {
        if let Some(rec) = recorder.as_ref() {
            self.prior_active_us += rec.active_us();
            self.prior_bytes += rec.bytes();
        }
        stop_recorder(recorder, rec_workers, None, is_split);
        self.split_pending = false;
    }

    fn check_disk_space(
        &mut self,
        clips_dir: &Path,
        recorder: &mut Option<Recorder>,
        rec_workers: &mut RecWorkers,
        on_event: &dyn Fn(SessionEvent),
    ) -> bool {
        if !self.is_continuous {
            return true;
        }
        let Some(free) = free_disk_bytes(clips_dir) else {
            return true;
        };
        let min_bytes = u64::from(self.continuous.min_free_gb) << 30;
        if free >= min_bytes {
            return true;
        }
        let had_rec = recorder.is_some();
        self.auto_record = false;
        self.manual_paused = false;
        self.idle_paused = false;
        self.waiting_for_source = false;
        if had_rec {
            self.accumulate_and_stop(recorder, rec_workers, false);
            on_event(SessionEvent::RecordingStoppedReason(format!(
                "Free disk space fell below {} GB; recording stopped and saved.",
                self.continuous.min_free_gb
            )));
        } else {
            on_event(SessionEvent::RecordingFailed(format!(
                "Free disk space is below the {} GB safety reserve.",
                self.continuous.min_free_gb
            )));
        }
        false
    }

    #[allow(clippy::too_many_arguments)]
    fn start_segment(
        &mut self,
        recorder: &mut Option<Recorder>,
        rec_workers: &mut RecWorkers,
        opts: &SessionOptions,
        streams: &[MuxStream],
        stats: &VideoStats,
        emit_started: bool,
        on_event: &dyn Fn(SessionEvent),
    ) {
        if !self.check_disk_space(&opts.clips_dir, recorder, rec_workers, on_event) {
            return;
        }
        let app = current_app(stats);
        let next_idx = self.segment_index + 1;
        let part = self.is_continuous.then_some(next_idx);
        match segment_clip_path(&opts.clips_dir, &app, part, opts.container) {
            Ok((mkv_path, mp4_path)) => match spawn_recorder(mkv_path, mp4_path, streams.to_vec(), app, next_idx) {
                Ok(rec) => {
                    self.segment_index = next_idx;
                    *recorder = Some(rec);
                    if self.effective_paused() {
                        apply_pause(recorder, rec_workers, true, on_event);
                    }
                    if emit_started {
                        on_event(SessionEvent::RecordingStarted);
                    }
                }
                Err(e) => {
                    self.auto_record = false;
                    on_event(SessionEvent::RecordingFailed(format!("recording thread: {e}")));
                }
            },
            Err(e) => {
                self.auto_record = false;
                on_event(SessionEvent::RecordingFailed(format!("clips folder: {e}")));
            }
        }
    }

    fn rotate_segment(
        &mut self,
        recorder: &mut Option<Recorder>,
        rec_workers: &mut RecWorkers,
        opts: &SessionOptions,
        streams: &[MuxStream],
        stats: &VideoStats,
        on_event: &dyn Fn(SessionEvent),
    ) {
        self.split_pending = false;
        self.accumulate_and_stop(recorder, rec_workers, true);
        self.start_segment(recorder, rec_workers, opts, streams, stats, false, on_event);
    }
}

fn apply_pause(
    recorder: &mut Option<Recorder>,
    rec_workers: &mut RecWorkers,
    paused: bool,
    on_event: &dyn Fn(SessionEvent),
) {
    if let Some(rec) = recorder.as_mut()
        && rec.paused != paused
    {
        rec.paused = paused;
        send_record_msg(recorder, rec_workers, RecMsg::SetPaused(paused), false);
        on_event(SessionEvent::RecordingPaused(paused));
    }
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
    let mut rec_workers: RecWorkers = Vec::new();
    let mut shots: Shots = Vec::new();
    let mut recorder: Option<Recorder> = None;
    let mut run = ContinuousRun::new(opts);
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
            if run.is_continuous && run.auto_record {
                let ready = source_ready_for_continuous(
                    &opts.video.source,
                    run.continuous.wait_for_source,
                    v.stats.capture_kind,
                );
                if !ready {
                    run.waiting_for_source = true;
                    if recorder.is_some() {
                        run.accumulate_and_stop(&mut recorder, &mut rec_workers, true);
                        run.manual_paused = false;
                        run.idle_paused = false;
                    }
                } else {
                    run.waiting_for_source = false;
                    if recorder.is_none() {
                        run.start_segment(
                            &mut recorder,
                            &mut rec_workers,
                            opts,
                            &streams,
                            &v.stats,
                            true,
                            on_event,
                        );
                    }
                }
            }
            for p in packets.drain(..) {
                if recorder.is_some() && !run.effective_paused() {
                    if run.is_continuous
                        && !run.split_pending
                        && let Some(rec) = recorder.as_ref().filter(|r| r.packets() > 0)
                    {
                        let hit_mins = run.continuous.segment_mins > 0
                            && rec.active_us() >= i64::from(run.continuous.segment_mins) * 60 * 1_000_000;
                        let hit_mb =
                            run.continuous.segment_mb > 0 && rec.bytes() >= u64::from(run.continuous.segment_mb) << 20;
                        if hit_mins || hit_mb {
                            run.split_pending = true;
                        }
                    }
                    if run.split_pending && p.stream == VIDEO_STREAM && p.key {
                        run.rotate_segment(&mut recorder, &mut rec_workers, opts, &streams, &v.stats, on_event);
                    }
                }
                if run.keep_buffer() {
                    if recorder.is_some() {
                        send_record_packet(&mut recorder, &mut rec_workers, p.clone());
                    }
                    buf.push(p);
                } else if recorder.is_some() {
                    send_record_packet(&mut recorder, &mut rec_workers, p);
                }
            }
            if let Err(e) = drain_audio(audio.as_ref(), |p| {
                if run.keep_buffer() {
                    if recorder.is_some() {
                        send_record_packet(&mut recorder, &mut rec_workers, p.clone());
                    }
                    buf.push(p);
                } else if recorder.is_some() {
                    send_record_packet(&mut recorder, &mut rec_workers, p);
                }
                Ok(())
            }) {
                break 'session Err(e);
            }
            match rx.try_recv() {
                Ok(SessionCmd::Save(secs)) => {
                    if !run.keep_buffer() {
                        on_event(SessionEvent::SaveFailed(
                            "the replay buffer is disabled while continuous mode is active".into(),
                        ));
                    } else {
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
                }
                Ok(SessionCmd::RecordStart) => {
                    run.auto_record = run.is_continuous;
                    run.manual_paused = false;
                    run.idle_paused = false;
                    run.split_pending = false;
                    if recorder.is_none() {
                        run.prior_active_us = 0;
                        run.prior_bytes = 0;
                        run.segment_index = 0;
                        let ready = !run.is_continuous
                            || source_ready_for_continuous(
                                &opts.video.source,
                                run.continuous.wait_for_source,
                                v.stats.capture_kind,
                            );
                        if ready {
                            run.waiting_for_source = false;
                            run.start_segment(
                                &mut recorder,
                                &mut rec_workers,
                                opts,
                                &streams,
                                &v.stats,
                                true,
                                on_event,
                            );
                        } else {
                            run.waiting_for_source = true;
                        }
                    }
                }
                Ok(SessionCmd::RecordStop) => {
                    run.auto_record = false;
                    run.manual_paused = false;
                    run.idle_paused = false;
                    run.waiting_for_source = false;
                    run.accumulate_and_stop(&mut recorder, &mut rec_workers, false);
                }
                Ok(SessionCmd::RecordTogglePause) => {
                    if recorder.is_some() {
                        run.manual_paused = !run.manual_paused;
                        if !run.manual_paused {
                            run.idle_paused = false;
                        }
                        apply_pause(&mut recorder, &mut rec_workers, run.effective_paused(), on_event);
                    }
                }
                Ok(SessionCmd::RecordSplit) => {
                    if recorder.as_ref().is_some_and(|r| r.packets() > 0) && !run.effective_paused() {
                        run.split_pending = true;
                    }
                }
                Ok(SessionCmd::UpdateContinuous(new_c)) => {
                    run.continuous = new_c;
                    if !run.keep_buffer() {
                        buf.flush();
                    }
                }
                Ok(SessionCmd::Preview(max_w, reply)) => {
                    let _ = reply.send(v.preview(max_w));
                }
                Ok(SessionCmd::Screenshot) => {
                    let started = clip_path(&opts.clips_dir, "png")
                        .map_err(|e| format!("clips folder: {e}"))
                        .and_then(|path| Ok((path, v.screenshot()?)));
                    match started {
                        // PNG encoding runs off the video thread; notify immediately on capture.
                        Ok((path, image)) => {
                            on_event(SessionEvent::ScreenshotTaken(path.clone()));
                            shots.push(std::thread::spawn(move || {
                                shot::write_png(&image, &path).map_err(|e| e.to_string())
                            }));
                        }
                        Err(e) => on_event(SessionEvent::ScreenshotFailed(e)),
                    }
                }
                Ok(SessionCmd::Stop) | Err(crossbeam_channel::TryRecvError::Disconnected) => break 'session Ok(()),
                Err(crossbeam_channel::TryRecvError::Empty) => {}
            }
            reap(&mut workers, false, on_event);
            reap_recordings(&mut rec_workers, false, on_event);
            reap_shots(&mut shots, false, on_event);
            if index % fps == 0 {
                if run.is_continuous {
                    if run.continuous.split_on_app_change
                        && !run.effective_paused()
                        && let Some(rec) = recorder.as_ref()
                        && rec.packets() > 0
                        && current_app(&v.stats) != rec.app
                    {
                        run.split_pending = true;
                    }
                    if run.continuous.idle_pause_mins > 0 {
                        let threshold_secs = run.continuous.idle_pause_mins * 60;
                        if let Some(idle) = idle_seconds() {
                            if !run.idle_paused && !run.manual_paused && idle >= threshold_secs && recorder.is_some() {
                                run.idle_paused = true;
                                apply_pause(&mut recorder, &mut rec_workers, run.effective_paused(), on_event);
                            } else if run.idle_paused && idle < threshold_secs {
                                run.idle_paused = false;
                                apply_pause(&mut recorder, &mut rec_workers, run.effective_paused(), on_event);
                            }
                        }
                    } else if run.idle_paused {
                        run.idle_paused = false;
                        apply_pause(&mut recorder, &mut rec_workers, run.effective_paused(), on_event);
                    }
                    if run.continuous.max_mins > 0 && recorder.is_some() {
                        let total_active_us = run.prior_active_us + recorder.as_ref().map_or(0, |r| r.active_us());
                        if total_active_us >= i64::from(run.continuous.max_mins) * 60 * 1_000_000 {
                            run.auto_record = false;
                            run.manual_paused = false;
                            run.idle_paused = false;
                            run.waiting_for_source = false;
                            run.accumulate_and_stop(&mut recorder, &mut rec_workers, false);
                            on_event(SessionEvent::RecordingStoppedReason(format!(
                                "Reached the {} minute continuous limit; recording stopped and saved.",
                                run.continuous.max_mins
                            )));
                        }
                    }
                    if (index / fps) % 5 == 0 && (recorder.is_some() || run.auto_record) {
                        run.check_disk_space(&opts.clips_dir, &mut recorder, &mut rec_workers, on_event);
                    }
                }
                let b = if run.keep_buffer() {
                    buf.stats()
                } else {
                    crate::buffer::Stats {
                        packets: 0,
                        bytes: 0,
                        span_us: 0,
                    }
                };
                let seg_us = recorder.as_ref().map_or(0, |r| r.active_us());
                let seg_bytes = recorder.as_ref().map_or(0, |r| r.bytes());
                let total_active_us = if run.is_continuous {
                    run.prior_active_us + seg_us
                } else {
                    seg_us
                };
                on_event(SessionEvent::Status(SessionStatus {
                    buffered_secs: b.span_us as f64 / 1e6,
                    buffered_bytes: b.bytes as u64,
                    frames: v.stats.frames,
                    dropped: v.stats.dropped,
                    buffer_estimate_mb: if run.keep_buffer() { estimate_mb } else { 0 },
                    recording: recorder.is_some(),
                    recording_paused: recorder.as_ref().is_some_and(|r| r.paused),
                    waiting_for_source: run.is_continuous && run.auto_record && run.waiting_for_source,
                    recording_secs: total_active_us as f64 / 1e6,
                    segment_secs: seg_us as f64 / 1e6,
                    segment_bytes: seg_bytes,
                    session_bytes: run.prior_bytes + seg_bytes,
                    segment_index: recorder.as_ref().map_or(0, |r| r.index),
                    capture_kind: v.stats.capture_kind,
                    capture_exe: v.stats.capture_exe.clone(),
                    capture_title: v.stats.capture_title.clone(),
                    source_width: v.stats.source_width,
                    source_height: v.stats.source_height,
                    source: v.stats.source.clone(),
                    notes: v.stats.notes.clone(),
                }));
            }
        };

        // Plan §7 row 2: rebuild device + encoder, flush the buffer (stream parameters
        // change), back off between attempts. Audio keeps running; its packets are
        // dropped by the empty buffer until the first new keyframe.
        on_event(SessionEvent::Recovering(failure.to_string()));
        run.accumulate_and_stop(&mut recorder, &mut rec_workers, run.is_continuous);
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
                    Ok(SessionCmd::RecordStart) => {
                        if run.is_continuous {
                            run.auto_record = true;
                        } else {
                            on_event(SessionEvent::RecordingFailed(
                                "capture is restarting; cannot start recording yet".into(),
                            ));
                        }
                    }
                    Ok(SessionCmd::RecordStop) => {
                        run.auto_record = false;
                        run.waiting_for_source = false;
                    }
                    Ok(SessionCmd::RecordTogglePause | SessionCmd::RecordSplit) => {}
                    Ok(SessionCmd::UpdateContinuous(new_c)) => run.continuous = new_c,
                    Ok(SessionCmd::Preview(_, reply)) => {
                        let _ = reply.send(Err("capture is restarting".into()));
                    }
                    Ok(SessionCmd::Screenshot) => {
                        on_event(SessionEvent::ScreenshotFailed(
                            "capture is restarting; try again".into(),
                        ));
                    }
                    Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
                }
                let _ = drain_audio(audio.as_ref(), |_| Ok(()));
                reap(&mut workers, false, on_event);
                reap_recordings(&mut rec_workers, false, on_event);
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
    // Saves and recordings already started still finish (they hold their own packet handles).
    stop_recorder(&mut recorder, &mut rec_workers, None, false);
    reap(&mut workers, true, on_event);
    reap_recordings(&mut rec_workers, true, on_event);
    reap_shots(&mut shots, true, on_event);
    let finished = video.map(|v| v.finish(&mut packets)).transpose();
    let stopped = stop_audio(audio, |_| Ok(()));
    result?;
    finished?;
    stopped?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Video source preview (Settings dashboard).

/// Snapshot and resolved specs of the active or drafted video source for the UI preview card.
#[derive(Debug, Clone, serde::Serialize)]
pub struct VideoPreview {
    /// PNG data URL (`data:image/png;base64,...`), or `None` when waiting with no fallback frame.
    pub frame: Option<String>,
    /// "display", "window", or "waiting".
    pub kind: String,
    pub exe: Option<String>,
    pub title: Option<String>,
    pub source_width: u32,
    pub source_height: u32,
    pub output_width: u32,
    pub output_height: u32,
    pub fps: u32,
    pub monitor_label: String,
    /// True when `kind` is `"display"` only because the target app/game is not running yet.
    pub fallback_active: bool,
    pub note: Option<String>,
}

struct PreviewCapturer {
    device: capture::SendDevice,
    luid: (u32, i32),
    target: Target,
    display: Option<u32>,
    cursor: bool,
    src: WgcSource,
    last: Option<(shot::Image, u32, u32)>,
}

static PREVIEW_CACHE: Mutex<Option<PreviewCapturer>> = Mutex::new(None);

/// Drop any cached disarmed preview WGC capture session (called on arm/disarm/pause/window close).
pub fn clear_preview_cache() {
    if let Ok(mut g) = PREVIEW_CACHE.lock() {
        *g = None;
    }
}

fn resolve_preview_output(s: &config::Settings, mon_w: u32, mon_h: u32, refresh_hz: u32) -> (u32, u32, u32) {
    let v = s.video();
    let out_w = if v.width == 0 {
        (mon_w.clamp(320, 7680)) & !1
    } else {
        v.width
    };
    let out_h = if v.height == 0 {
        (mon_h.clamp(320, 7680)) & !1
    } else {
        v.height
    };
    let fps = if refresh_hz > 0 { v.fps.min(refresh_hz) } else { v.fps };
    (out_w, out_h, fps)
}

/// Build a [`VideoPreview`] from the running capture session's newest frame.
pub fn build_armed_preview(
    img: &shot::Image,
    stats: &VideoStats,
    s: &config::Settings,
) -> std::result::Result<VideoPreview, String> {
    capture::com_init();
    let v = s.video();
    let (out_w, out_h, fps) = if v.width > 0 && v.height > 0 {
        let hz = pick_output_or_primary(s.display)
            .ok()
            .map(|(_, o, _)| o.refresh_hz)
            .unwrap_or(0);
        resolve_preview_output(s, v.width, v.height, hz)
    } else if let Ok((_, o, _)) = pick_output_or_primary(s.display) {
        resolve_preview_output(s, o.width.max(1) as u32, o.height.max(1) as u32, o.refresh_hz)
    } else {
        (stats.width, stats.height, v.fps.min(stats.fps.max(1)))
    };
    let frame = Some(shot::encode_png_data_url(img).map_err(|e| e.to_string())?);
    let source_width = if stats.source_width > 0 {
        stats.source_width
    } else {
        stats.width
    };
    let source_height = if stats.source_height > 0 {
        stats.source_height
    } else {
        stats.height
    };
    let fallback_active = s.source.kind != config::SourceKind::Display && stats.capture_kind == "display";
    Ok(VideoPreview {
        frame,
        kind: stats.capture_kind.to_owned(),
        exe: stats.capture_exe.clone(),
        title: stats.capture_title.clone(),
        source_width,
        source_height,
        output_width: out_w,
        output_height: out_h,
        fps,
        monitor_label: stats.source.clone(),
        fallback_active,
        note: None,
    })
}

fn read_preview_frame(
    device: &ID3D11Device,
    frame: &capture::Frame,
    max_w: u32,
) -> std::result::Result<(shot::Image, u32, u32), String> {
    // SAFETY: device is live; immediate context is used only on the calling thread under Mutex.
    let ctx = unsafe { device.GetImmediateContext() }.map_err(|e| e.to_string())?;
    shot::read_bgra_scaled(device, &ctx, &frame.tex, frame.width, frame.height, max_w).map_err(|e| e.to_string())
}

fn open_preview_capturer(
    adapter: &Adapter,
    output: &Output,
    active: &mut Target,
    display: Option<u32>,
    cursor: bool,
    max_w: u32,
    note: &mut Option<String>,
) -> std::result::Result<PreviewCapturer, String> {
    let (device, _ctx) = capture::create_device(adapter).map_err(|e| e.to_string())?;
    let mut src = match active {
        Target::Window { hwnd, exe, .. } => match WgcSource::for_window(&device, *hwnd, cursor) {
            Ok(s) => s,
            Err(e) => {
                *note = Some(format!("Could not capture {exe} ({e}); showing display instead."));
                *active = Target::Display;
                WgcSource::for_monitor(&device, output, cursor).map_err(|e| e.to_string())?
            }
        },
        _ => WgcSource::for_monitor(&device, output, cursor).map_err(|e| e.to_string())?,
    };
    let last = match wait_first_frame(&mut src, Duration::from_millis(350)) {
        Ok(f) => read_preview_frame(&device, &f, max_w).ok(),
        Err(_) => None,
    };
    Ok(PreviewCapturer {
        device: capture::SendDevice(device),
        luid: adapter.luid,
        target: active.clone(),
        display,
        cursor,
        src,
        last,
    })
}

/// Capture a lightweight preview frame matching draft settings without starting encoders or audio.
pub fn preview_source(s: &config::Settings, max_w: u32) -> std::result::Result<VideoPreview, String> {
    capture::com_init();
    let found = (s.source.kind == config::SourceKind::Auto).then(game::detect).flatten();
    let prev_target = PREVIEW_CACHE
        .lock()
        .ok()
        .and_then(|g| g.as_ref().map(|c| c.target.clone()));
    let mut active = resolve(&s.source, prev_target.as_ref(), found);

    let (adapter, output, fell_back) = match &active {
        Target::Window { hwnd, .. } => pick_output_for_window(*hwnd, s.display).map_err(|e| e.to_string())?,
        _ => pick_output_or_primary(s.display).map_err(|e| e.to_string())?,
    };
    let (out_w, out_h, fps) = resolve_preview_output(
        s,
        output.width.max(1) as u32,
        output.height.max(1) as u32,
        output.refresh_hz,
    );
    let monitor_label = format!("{}x{} @ {} Hz", output.width, output.height, output.refresh_hz);
    let mut note = fell_back.then(|| {
        format!(
            "Display {} was not found; previewing the main monitor.",
            s.display.unwrap_or(0)
        )
    });

    if active == Target::Waiting {
        clear_preview_cache();
        return Ok(VideoPreview {
            frame: None,
            kind: "waiting".into(),
            exe: (!s.source.exe.is_empty()).then(|| s.source.exe.clone()),
            title: (!s.source.title.is_empty()).then(|| s.source.title.clone()),
            source_width: 0,
            source_height: 0,
            output_width: out_w,
            output_height: out_h,
            fps,
            monitor_label,
            fallback_active: false,
            note,
        });
    }

    let mut guard = PREVIEW_CACHE.lock().map_err(|_| "preview lock poisoned".to_string())?;
    let reuse = guard.as_ref().is_some_and(|c| {
        c.luid == adapter.luid && c.target == active && c.display == s.display && c.cursor == s.cursor
    });
    if !reuse {
        *guard = None;
    }

    let mut cached_snapshot: Option<(shot::Image, u32, u32)> = None;
    if let Some(cap) = guard.as_mut() {
        match cap.src.next_frame(0) {
            Ok(Some(f)) => match read_preview_frame(&cap.device.0, &f, max_w) {
                Ok(img) => {
                    cap.last = Some(img.clone());
                    cached_snapshot = Some(img);
                }
                Err(_) => cached_snapshot = cap.last.clone(),
            },
            Ok(None) => cached_snapshot = cap.last.clone(),
            Err(_) => {
                *guard = None;
            }
        }
    }

    if guard.is_none() {
        let cap = open_preview_capturer(&adapter, &output, &mut active, s.display, s.cursor, max_w, &mut note)?;
        cached_snapshot = cap.last.clone();
        *guard = Some(cap);
    }
    drop(guard);

    let frame = cached_snapshot
        .as_ref()
        .map(|(img, _, _)| shot::encode_png_data_url(img))
        .transpose()
        .map_err(|e| e.to_string())?;
    let (source_width, source_height) = cached_snapshot.as_ref().map_or((0, 0), |(_, w, h)| (*w, *h));
    let (kind, exe, title) = match &active {
        Target::Window { exe, title, .. } => ("window".to_owned(), Some(exe.clone()), Some(title.clone())),
        Target::Waiting => (
            "waiting".to_owned(),
            (!s.source.exe.is_empty()).then(|| s.source.exe.clone()),
            None,
        ),
        Target::Display => ("display".to_owned(), None, None),
    };
    let fallback_active = s.source.kind != config::SourceKind::Display && matches!(active, Target::Display);

    Ok(VideoPreview {
        frame,
        kind,
        exe,
        title,
        source_width,
        source_height,
        output_width: out_w,
        output_height: out_h,
        fps,
        monitor_label,
        fallback_active,
        note,
    })
}

#[cfg(test)]
mod continuous_tests {
    use super::*;

    #[test]
    fn source_readiness_respects_wait_and_source_kind() {
        let app_no_fb = config::SourceSettings {
            kind: config::SourceKind::App,
            exe: "game.exe".into(),
            title: String::new(),
            fallback: false,
        };
        assert!(!source_ready_for_continuous(&app_no_fb, true, "waiting"));
        assert!(source_ready_for_continuous(&app_no_fb, true, "window"));
        assert!(source_ready_for_continuous(&app_no_fb, false, "waiting"));

        let auto = config::SourceSettings {
            kind: config::SourceKind::Auto,
            ..Default::default()
        };
        assert!(!source_ready_for_continuous(&auto, true, "display"));
        assert!(source_ready_for_continuous(&auto, true, "window"));
        assert!(source_ready_for_continuous(&auto, false, "display"));

        let display = config::SourceSettings::default();
        assert!(source_ready_for_continuous(&display, true, "display"));
    }

    #[test]
    fn segment_clip_path_avoids_mp4_and_tmp_collisions() {
        let dir = std::env::temp_dir().join(format!("suicap_seg_path_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let (mkv1, mp41) = segment_clip_path(&dir, "Game", Some(1), config::Container::Mp4).unwrap();
        let mp41 = mp41.unwrap();
        assert_eq!(mkv1.extension().unwrap(), "mkv");
        assert_eq!(mp41.extension().unwrap(), "mp4");
        std::fs::write(&mp41, b"done").unwrap();
        let (mkv2, mp42) = segment_clip_path(&dir, "Game", Some(1), config::Container::Mp4).unwrap();
        assert_ne!(mkv1, mkv2);
        assert_ne!(Some(&mp41), mp42.as_ref());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn resolve_preview_output_handles_high_and_refresh_clamp() {
        let mut s = config::Settings {
            preset: config::PresetName::High,
            ..Default::default()
        };
        assert_eq!(resolve_preview_output(&s, 2560, 1440, 144), (2560, 1440, 60));

        s.preset = config::PresetName::Custom;
        s.custom.width = 1920;
        s.custom.height = 1080;
        s.custom.fps = 240;
        assert_eq!(resolve_preview_output(&s, 2560, 1440, 144), (1920, 1080, 144));
    }

    #[test]
    fn build_armed_preview_marks_fallback_when_targeting_app_on_display() {
        let mut s = config::Settings {
            preset: config::PresetName::Custom,
            ..Default::default()
        };
        s.source.kind = config::SourceKind::App;
        s.source.exe = "game.exe".into();
        s.custom.width = 1280;
        s.custom.height = 720;
        s.custom.fps = 60;
        let stats = VideoStats {
            width: 1280,
            height: 720,
            source_width: 2560,
            source_height: 1440,
            fps: 60,
            capture_kind: "display",
            source: "WGC DISPLAY1".into(),
            ..Default::default()
        };
        let img = shot::Image {
            width: 4,
            height: 2,
            rgb: vec![128; 24],
        };
        let pv = build_armed_preview(&img, &stats, &s).unwrap();
        assert!(pv.frame.as_deref().unwrap().starts_with("data:image/png;base64,"));
        assert!(pv.fallback_active);
        assert_eq!(pv.source_width, 2560);
        assert_eq!(pv.output_width, 1280);
        assert_eq!(pv.output_height, 720);
    }
}
