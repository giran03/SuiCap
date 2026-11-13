//! Audio (plan §5): WASAPI mic + per-process loopback -> QPC-anchored timeline ->
//! per-track AAC/Opus -> packets for the replay buffer or a file. FFI module.
//!
//! One capture thread per source (MMCSS "Pro Audio"), one mix+encode thread. Each
//! source is anchored at the QPC time of its first buffer and then advanced by sample
//! count; if it drifts more than 20 ms from QPC it is resynced (silence inserted or
//! samples dropped). The mix thread emits every track in lockstep, filling silence for
//! sources with no data, so all tracks stay gap-free and equal length.

use std::{
    collections::{HashMap, VecDeque},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};

use crossbeam_channel::{Receiver, RecvTimeoutError, Sender};
use windows::{
    Win32::{
        Foundation::{CloseHandle, HANDLE, HWND, RECT, WAIT_OBJECT_0},
        Graphics::Gdi::{GetMonitorInfoW, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromWindow},
        Media::Audio::{
            AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_BUFFERFLAGS_TIMESTAMP_ERROR, AUDCLNT_SHAREMODE_SHARED,
            AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM, AUDCLNT_STREAMFLAGS_EVENTCALLBACK, AUDCLNT_STREAMFLAGS_LOOPBACK,
            AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY, AUDIOCLIENT_ACTIVATION_PARAMS, AUDIOCLIENT_ACTIVATION_PARAMS_0,
            AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK, AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS,
            ActivateAudioInterfaceAsync, IActivateAudioInterfaceAsyncOperation,
            IActivateAudioInterfaceCompletionHandler, IActivateAudioInterfaceCompletionHandler_Impl,
            IAudioCaptureClient, IAudioClient, IMMDevice, IMMDeviceEnumerator, MMDeviceEnumerator,
            PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE, VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK, WAVEFORMATEX,
            eCapture, eConsole, eRender,
        },
        System::{
            Com::{
                BLOB, CLSCTX_ALL, CoCreateInstance, CoTaskMemFree, IAgileObject, IAgileObject_Impl,
                StructuredStorage::PROPVARIANT,
            },
            Diagnostics::ToolHelp::{
                CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
            },
            Threading::{AvSetMmThreadCharacteristicsW, CreateEventW, GetCurrentProcessId, WaitForSingleObject},
            Variant::VT_BLOB,
        },
        UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowRect, GetWindowThreadProcessId},
    },
    core::{HRESULT, IUnknown, Interface, Ref, implement, w},
};

use crate::{
    buffer::Packet,
    capture,
    encode::{AUDIO_RATE, AudioCodec, AudioEncoder, EncodeError, StreamParams},
};

#[derive(Debug, thiserror::Error)]
pub enum AudioError {
    #[error(transparent)]
    Win(#[from] windows::core::Error),
    #[error("encode: {0}")]
    Encode(#[from] EncodeError),
    #[error("process loopback activation timed out")]
    ActivationTimeout,
    #[error("audio mix thread panicked")]
    Panic,
}

impl AudioError {
    /// Device invalidation and the like are handled inside (silence fills the gap).
    pub fn is_recoverable(&self) -> bool {
        matches!(self, AudioError::Win(_))
    }
}

type Result<T> = std::result::Result<T, AudioError>;

const RATE: i64 = AUDIO_RATE as i64;
/// Resync a source once it drifts this far from QPC (20 ms).
const RESYNC_SAMPLES: i64 = RATE / 50;
/// The mix runs this far behind real time so late WASAPI buffers still land.
const LATENCY_SAMPLES: i64 = RATE / 10;
const RESCAN_EVERY: Duration = Duration::from_secs(2);

// ---------------------------------------------------------------------------
// Timeline: pure logic, unit-tested.

#[derive(Debug, Default)]
struct Line {
    /// Absolute sample index of the next sample this source will write.
    cursor: Option<i64>,
    /// Interleaved stereo samples starting at the timeline's `emit_pos`.
    pending: VecDeque<f32>,
    resyncs: u64,
    late_samples: u64,
}

/// Places source chunks on one sample clock anchored at `start_us` (QPC).
#[derive(Debug)]
pub struct Timeline {
    start_us: i64,
    emit_pos: i64,
    lines: Vec<Line>,
}

impl Timeline {
    pub fn new(start_us: i64) -> Self {
        Self {
            start_us,
            emit_pos: 0,
            lines: Vec::new(),
        }
    }

    pub fn add_line(&mut self) -> usize {
        self.lines.push(Line::default());
        self.lines.len() - 1
    }

    /// Make `line` free for a new source: forget its clock and pending samples.
    pub fn reset_line(&mut self, line: usize) {
        let l = &mut self.lines[line];
        l.cursor = None;
        l.pending = VecDeque::new();
    }

    pub fn lines(&self) -> usize {
        self.lines.len()
    }

    pub fn pos_of(&self, qpc_us: i64) -> i64 {
        (i128::from(qpc_us - self.start_us) * i128::from(RATE) / 1_000_000) as i64
    }

    /// Add `samples` (interleaved stereo) captured at `qpc_us` to `line`.
    pub fn push(&mut self, line: usize, qpc_us: i64, samples: &[f32]) {
        let frames = (samples.len() / 2) as i64;
        let expected = self.pos_of(qpc_us);
        let emit_pos = self.emit_pos;
        let l = &mut self.lines[line];
        let pos = match l.cursor {
            None => expected,
            Some(c) if (expected - c).abs() > RESYNC_SAMPLES => {
                l.resyncs += 1;
                expected
            }
            Some(c) => c,
        };
        l.cursor = Some(pos + frames);
        let skip = (emit_pos - pos).clamp(0, frames);
        l.late_samples += skip as u64;
        let off = ((pos + skip - emit_pos) * 2) as usize;
        if l.pending.len() < off {
            l.pending.resize(off, 0.0); // gap: silence
        }
        for (k, &s) in samples[(skip * 2) as usize..].iter().enumerate() {
            match l.pending.get_mut(off + k) {
                Some(slot) => *slot = s,
                None => l.pending.push_back(s),
            }
        }
    }

    /// True once a whole frame ending `LATENCY_SAMPLES` before `now_us` can be emitted.
    pub fn ready(&self, now_us: i64, frame: usize, latency: i64) -> bool {
        self.pos_of(now_us) - latency >= self.emit_pos + frame as i64
    }

    /// Take the next `frame` samples of every line (silence where a line has none).
    /// Returns the frame's start position and one interleaved buffer per line.
    pub fn pop(&mut self, frame: usize) -> (i64, Vec<Vec<f32>>) {
        let pos = self.emit_pos;
        let n = frame * 2;
        let out = self
            .lines
            .iter_mut()
            .map(|l| {
                let take = n.min(l.pending.len());
                let mut v: Vec<f32> = l.pending.drain(..take).collect();
                v.resize(n, 0.0);
                v
            })
            .collect();
        self.emit_pos += frame as i64;
        (pos, out)
    }

    pub fn resyncs(&self) -> u64 {
        self.lines.iter().map(|l| l.resyncs).sum()
    }

    pub fn late_samples(&self) -> u64 {
        self.lines.iter().map(|l| l.late_samples).sum()
    }
}

/// Soft limiter for the mix: linear to 0.8, then a tanh knee that never exceeds 1.0.
pub fn soft_limit(x: f32) -> f32 {
    const KNEE: f32 = 0.8;
    let a = x.abs();
    if a <= KNEE {
        x
    } else {
        x.signum() * (KNEE + (1.0 - KNEE) * ((a - KNEE) / (1.0 - KNEE)).tanh())
    }
}

// ---------------------------------------------------------------------------
// Tracks and targets.

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TrackKind {
    Mix,
    Mic,
    Game,
    Voice,
    Media,
    /// Fallback when process loopback is unavailable: whole default render endpoint.
    Desktop,
}

impl TrackKind {
    pub fn title(self) -> &'static str {
        match self {
            TrackKind::Mix => "Mix",
            TrackKind::Mic => "Mic",
            TrackKind::Game => "Game",
            TrackKind::Voice => "Voice",
            TrackKind::Media => "Media",
            TrackKind::Desktop => "Desktop",
        }
    }
}

/// A process to capture: by PID (with its tree) or every root process of an exe name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    Pid(u32),
    Exe(String),
}

#[derive(Debug, Clone)]
pub struct AudioOptions {
    pub codec: AudioCodec,
    pub mic: bool,
    pub game: Vec<Target>,
    pub voice: Vec<Target>,
    pub media: Vec<Target>,
}

impl AudioOptions {
    /// Plan §5 defaults: game = foreground fullscreen app, voice/media by exe name.
    pub fn defaults(codec: AudioCodec) -> Self {
        Self::from_settings(&crate::config::AudioSettings::default(), codec)
    }

    /// From persisted settings; an empty game list means the foreground fullscreen app.
    pub fn from_settings(s: &crate::config::AudioSettings, codec: AudioCodec) -> Self {
        let exes = |names: &[String]| names.iter().map(|n| Target::Exe(n.clone())).collect();
        let game = if s.game.is_empty() {
            foreground_fullscreen_pid().map(Target::Pid).into_iter().collect()
        } else {
            exes(&s.game)
        };
        Self {
            codec,
            mic: s.mic,
            game,
            voice: exes(&s.voice),
            media: exes(&s.media),
        }
    }
}

/// Executable name without `.exe` of the foreground fullscreen app, for clip folders
/// (plan §6.2); `None` on the desktop.
pub fn foreground_app_name() -> Option<String> {
    let pid = foreground_fullscreen_pid()?;
    let exe = processes().into_iter().find(|p| p.pid == pid)?.exe;
    let stem = exe
        .strip_suffix(".exe")
        .or_else(|| exe.strip_suffix(".EXE"))
        .unwrap_or(&exe);
    (!stem.is_empty()).then(|| stem.to_owned())
}

#[derive(Debug, Clone)]
struct Proc {
    pid: u32,
    parent: u32,
    exe: String,
}

fn processes() -> Vec<Proc> {
    let mut list = Vec::new();
    // SAFETY: snapshot handle closed below; entry is a sized local.
    unsafe {
        let Ok(snap) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else {
            return list;
        };
        let mut e = PROCESSENTRY32W {
            dwSize: size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        let mut ok = Process32FirstW(snap, &mut e).is_ok();
        while ok {
            let end = e.szExeFile.iter().position(|&c| c == 0).unwrap_or(e.szExeFile.len());
            list.push(Proc {
                pid: e.th32ProcessID,
                parent: e.th32ParentProcessID,
                exe: String::from_utf16_lossy(&e.szExeFile[..end]),
            });
            ok = Process32NextW(snap, &mut e).is_ok();
        }
        let _ = CloseHandle(snap);
    }
    list
}

/// PIDs to loop back for `targets`: explicit PIDs, plus each *root* process of a named
/// exe (its tree is included by the loopback mode, so children are not repeated).
fn resolve(targets: &[Target], procs: &[Proc]) -> Vec<u32> {
    let alive: HashMap<u32, &Proc> = procs.iter().map(|p| (p.pid, p)).collect();
    let mut pids = Vec::new();
    for t in targets {
        match t {
            Target::Pid(pid) if alive.contains_key(pid) => pids.push(*pid),
            Target::Pid(_) => {}
            Target::Exe(name) => {
                for p in procs.iter().filter(|p| p.exe.eq_ignore_ascii_case(name)) {
                    let parent_same = alive.get(&p.parent).is_some_and(|pp| pp.exe.eq_ignore_ascii_case(name));
                    if !parent_same {
                        pids.push(p.pid);
                    }
                }
            }
        }
    }
    pids.sort_unstable();
    pids.dedup();
    pids
}

/// Foreground window's process if it covers its whole monitor (plan §5 Game default).
pub fn foreground_fullscreen_pid() -> Option<u32> {
    // SAFETY: plain Win32 queries with locals as out-params.
    unsafe {
        let hwnd: HWND = GetForegroundWindow();
        if hwnd.is_invalid() {
            return None;
        }
        let mut rect = RECT::default();
        GetWindowRect(hwnd, &mut rect).ok()?;
        let mut mi = MONITORINFO {
            cbSize: size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        if !GetMonitorInfoW(MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST), &mut mi).as_bool() {
            return None;
        }
        let m = mi.rcMonitor;
        let full = rect.left <= m.left && rect.top <= m.top && rect.right >= m.right && rect.bottom >= m.bottom;
        let mut pid = 0;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        (full && pid != 0 && pid != GetCurrentProcessId()).then_some(pid)
    }
}

// ---------------------------------------------------------------------------
// WASAPI sources.

fn wave_format() -> WAVEFORMATEX {
    WAVEFORMATEX {
        wFormatTag: 3, // WAVE_FORMAT_IEEE_FLOAT
        nChannels: 2,
        nSamplesPerSec: AUDIO_RATE as u32,
        nAvgBytesPerSec: AUDIO_RATE as u32 * 8,
        nBlockAlign: 8,
        wBitsPerSample: 32,
        cbSize: 0,
    }
}

fn default_endpoint(loopback: bool) -> Result<IMMDevice> {
    // SAFETY: COM creation with owned results.
    unsafe {
        let en: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
        Ok(en.GetDefaultAudioEndpoint(if loopback { eRender } else { eCapture }, eConsole)?)
    }
}

/// ID of the current default device (render for loopback, else capture).
fn default_endpoint_id(loopback: bool) -> Result<String> {
    let dev = default_endpoint(loopback)?;
    // SAFETY: GetId returns a CoTaskMem string that we free after copying it.
    unsafe {
        let id = dev.GetId()?;
        let text = id.to_string().unwrap_or_default();
        CoTaskMemFree(Some(id.0 as *const _));
        Ok(text)
    }
}

fn endpoint_client(loopback: bool) -> Result<(IAudioClient, String)> {
    let dev = default_endpoint(loopback)?;
    // SAFETY: GetId's CoTaskMem string is freed after copying; activation on a live device.
    unsafe {
        let raw = dev.GetId()?;
        let id = raw.to_string().unwrap_or_default();
        CoTaskMemFree(Some(raw.0 as *const _));
        Ok((dev.Activate::<IAudioClient>(CLSCTX_ALL, None)?, id))
    }
}

#[implement(IActivateAudioInterfaceCompletionHandler, IAgileObject)]
struct ActivationDone(Mutex<Option<mpsc::Sender<()>>>);

// Marker only: tells COM the handler may be called from any thread.
impl IAgileObject_Impl for ActivationDone_Impl {}

impl IActivateAudioInterfaceCompletionHandler_Impl for ActivationDone_Impl {
    fn ActivateCompleted(&self, _op: Ref<IActivateAudioInterfaceAsyncOperation>) -> windows::core::Result<()> {
        if let Some(tx) = self.0.lock().ok().and_then(|mut g| g.take()) {
            let _ = tx.send(());
        }
        Ok(())
    }
}

/// `IAudioClient` capturing `pid` and its child processes (Windows 10 2004+).
fn process_loopback_client(pid: u32) -> Result<IAudioClient> {
    let params = AUDIOCLIENT_ACTIVATION_PARAMS {
        ActivationType: AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK,
        Anonymous: AUDIOCLIENT_ACTIVATION_PARAMS_0 {
            ProcessLoopbackParams: AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS {
                TargetProcessId: pid,
                ProcessLoopbackMode: PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE,
            },
        },
    };
    // ManuallyDrop: the blob points at `params` on our stack and must never be freed
    // by PROPVARIANT's destructor.
    let mut pv = std::mem::ManuallyDrop::new(PROPVARIANT::default());
    // SAFETY: we fill the VT_BLOB arm of the union; `params` outlives the activation
    // call below (we block until it completes).
    unsafe {
        let inner = &mut pv.Anonymous.Anonymous;
        inner.vt = VT_BLOB;
        inner.Anonymous.blob = BLOB {
            cbSize: size_of::<AUDIOCLIENT_ACTIVATION_PARAMS>() as u32,
            pBlobData: (&raw const params) as *mut u8,
        };
    }
    let (tx, rx) = mpsc::channel();
    let handler: IActivateAudioInterfaceCompletionHandler = ActivationDone(Mutex::new(Some(tx))).into();
    // SAFETY: all pointers are valid for the call; the operation is kept until done.
    let op = unsafe {
        ActivateAudioInterfaceAsync(
            VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK,
            &IAudioClient::IID,
            Some(&*pv),
            &handler,
        )?
    };
    rx.recv_timeout(Duration::from_secs(5))
        .map_err(|_| AudioError::ActivationTimeout)?;
    let mut hr = HRESULT(0);
    let mut unk: Option<IUnknown> = None;
    // SAFETY: completed operation; out-params are locals.
    unsafe { op.GetActivateResult(&mut hr, &mut unk)? };
    hr.ok()?;
    Ok(unk.ok_or(AudioError::ActivationTimeout)?.cast()?)
}

/// Initialise a shared-mode, event-driven client for 48 kHz stereo float.
fn init_client(client: &IAudioClient, loopback: bool) -> Result<()> {
    let mut flags = AUDCLNT_STREAMFLAGS_EVENTCALLBACK
        | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM
        | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY;
    if loopback {
        flags |= AUDCLNT_STREAMFLAGS_LOOPBACK;
    }
    let fmt = wave_format();
    // SAFETY: fmt is a valid local; 200 ms shared buffer.
    unsafe { client.Initialize(AUDCLNT_SHAREMODE_SHARED, flags, 2_000_000, 0, &fmt, None)? };
    Ok(())
}

struct Chunk {
    line: usize,
    qpc_us: i64,
    samples: Vec<f32>,
}

/// What a capture source records, to find it again on rescan.
#[derive(Debug, Clone, PartialEq, Eq)]
enum SourceKey {
    /// Default mic (`loopback: false`) or speaker loopback, on device `id`.
    Endpoint {
        loopback: bool,
        id: String,
    },
    Process {
        track: usize,
        pid: u32,
    },
}

struct Source {
    key: SourceKey,
    line: usize,
    stop: Arc<AtomicBool>,
    thread: JoinHandle<()>,
}

/// Capture thread: event-driven reads, timestamped with the buffer's QPC position.
fn spawn_source(client: IAudioClient, key: SourceKey, line: usize, tx: Sender<Chunk>) -> Result<Source> {
    // SAFETY: auto-reset event, closed by the thread.
    let event: HANDLE = unsafe { CreateEventW(None, false, false, None)? };
    // SAFETY: valid event handle; service query on an initialised client.
    let capture: IAudioCaptureClient = unsafe {
        client.SetEventHandle(event)?;
        client.GetService()?
    };
    let stop = Arc::new(AtomicBool::new(false));
    let flag = stop.clone();
    // COM pointers are MTA-agile; moving them to the capture thread is fine.
    let (client, capture) = (SendCom(client), SendCom(capture));
    let event = SendCom(event);
    let thread = std::thread::Builder::new()
        .name("suicap-audio-src".into())
        .spawn(move || {
            let (client, capture, event) = (client.into_inner(), capture.into_inner(), event.into_inner());
            capture::com_init();
            let mut task = 0u32;
            // SAFETY: MMCSS registration for this thread; failure only costs priority.
            let _ = unsafe { AvSetMmThreadCharacteristicsW(w!("Pro Audio"), &mut task) };
            // SAFETY: start/stop on a live client; buffers released right after copy.
            unsafe {
                if client.Start().is_err() {
                    let _ = CloseHandle(event);
                    return;
                }
                while !flag.load(Ordering::Acquire) {
                    if WaitForSingleObject(event, 100) != WAIT_OBJECT_0 {
                        continue;
                    }
                    loop {
                        match capture.GetNextPacketSize() {
                            Ok(0) => break,
                            Ok(_) => {}
                            Err(_) => {
                                flag.store(true, Ordering::Release); // device invalidated: end
                                break;
                            }
                        }
                        let (mut data, mut frames, mut flags, mut qpc) = (std::ptr::null_mut(), 0u32, 0u32, 0u64);
                        if capture
                            .GetBuffer(&mut data, &mut frames, &mut flags, None, Some(&mut qpc))
                            .is_err()
                        {
                            flag.store(true, Ordering::Release);
                            break;
                        }
                        let n = frames as usize * 2;
                        let samples = if flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0 || data.is_null() {
                            vec![0.0; n]
                        } else {
                            std::slice::from_raw_parts(data as *const f32, n).to_vec()
                        };
                        let _ = capture.ReleaseBuffer(frames);
                        let qpc_us = if qpc == 0 || flags & AUDCLNT_BUFFERFLAGS_TIMESTAMP_ERROR.0 as u32 != 0 {
                            capture::qpc_now_us() - i64::from(frames) * 1_000_000 / RATE
                        } else {
                            (qpc / 10) as i64 // 100 ns units on the QPC timeline
                        };
                        if tx.send(Chunk { line, qpc_us, samples }).is_err() {
                            flag.store(true, Ordering::Release);
                            break;
                        }
                    }
                }
                let _ = client.Stop();
                let _ = CloseHandle(event);
            }
        })
        .expect("spawning an audio capture thread");
    Ok(Source {
        key,
        line,
        stop,
        thread,
    })
}

/// Wrapper to move COM pointers and handles into the capture thread.
struct SendCom<T>(T);
// SAFETY: WASAPI interfaces are free-threaded (MTA) and the event handle is a kernel
// object; each is used by exactly one thread after the move.
unsafe impl<T> Send for SendCom<T> {}

impl<T> SendCom<T> {
    /// Take the value out (a method, so closures capture the whole Send wrapper).
    fn into_inner(self) -> T {
        self.0
    }
}

// ---------------------------------------------------------------------------
// AudioPath: the mix+encode thread and its sources.

/// One output stream the muxer needs.
pub struct AudioStream {
    pub title: &'static str,
    pub params: Arc<StreamParams>,
    pub default: bool,
}

#[derive(Debug, Clone, Default)]
pub struct AudioStats {
    pub tracks: Vec<&'static str>,
    pub process_loopback: bool,
    /// Loopback sources opened over the run (PIDs).
    pub sources: Vec<(TrackKind, u32)>,
    pub mic: bool,
    pub resyncs: u64,
    pub late_samples: u64,
    pub frames_per_track: u64,
    /// Mic/speaker sources reopened after a device was removed or the default changed.
    pub endpoint_reopens: u64,
    /// Timeline lines allocated (lines of ended sources are reused).
    pub lines: usize,
}

pub struct AudioPath {
    pub rx: Receiver<Packet>,
    pub streams: Vec<AudioStream>,
    /// Per-process tracks available (else one Desktop track, plan §5 fallback).
    pub process_loopback: bool,
    /// The microphone opened at start.
    pub mic_open: bool,
    stop: Arc<AtomicBool>,
    thread: JoinHandle<Result<AudioStats>>,
}

struct Mixer {
    tracks: Vec<TrackKind>,
    encoders: Vec<AudioEncoder>,
    first_stream: u8,
    timeline: Timeline,
    /// line index -> track index
    line_track: Vec<usize>,
    sources: Vec<Source>,
    /// Lines of ended sources, ready for reuse.
    free_lines: Vec<usize>,
    /// Wanted endpoint sources: (track, loopback).
    endpoints: Vec<(usize, bool)>,
    targets: Vec<(usize, Vec<Target>)>,
    chunk_tx: Sender<Chunk>,
    stats: AudioStats,
}

impl AudioPath {
    /// Open sources and encoders, start the mix thread. Audio packets use stream
    /// indices `first_stream..`; PTS are QPC microseconds like video.
    pub fn start(opts: &AudioOptions, first_stream: u8) -> Result<Self> {
        capture::com_init();
        let origin_us = capture::qpc_now_us();
        let (chunk_tx, chunk_rx) = crossbeam_channel::unbounded::<Chunk>();
        let (pkt_tx, pkt_rx) = crossbeam_channel::unbounded::<Packet>();

        // Probe process loopback once on our own PID; fall back to endpoint loopback.
        let process_loopback = process_loopback_client(std::process::id()).is_ok();
        let tracks: Vec<TrackKind> = if process_loopback {
            vec![
                TrackKind::Mix,
                TrackKind::Mic,
                TrackKind::Game,
                TrackKind::Voice,
                TrackKind::Media,
            ]
        } else {
            vec![TrackKind::Mix, TrackKind::Mic, TrackKind::Desktop]
        };
        let encoders = tracks
            .iter()
            .map(|_| AudioEncoder::open(opts.codec, origin_us).map_err(AudioError::from))
            .collect::<Result<Vec<_>>>()?;
        let streams = tracks
            .iter()
            .zip(&encoders)
            .map(|(t, e)| {
                Ok(AudioStream {
                    title: t.title(),
                    params: Arc::new(e.stream_params()?),
                    default: *t == TrackKind::Mix,
                })
            })
            .collect::<Result<Vec<_>>>()?;

        let mut mixer = Mixer {
            tracks: tracks.clone(),
            encoders,
            first_stream,
            timeline: Timeline::new(origin_us),
            line_track: Vec::new(),
            sources: Vec::new(),
            free_lines: Vec::new(),
            endpoints: Vec::new(),
            targets: Vec::new(),
            chunk_tx,
            stats: AudioStats {
                tracks: tracks.iter().map(|t| t.title()).collect(),
                process_loopback,
                ..Default::default()
            },
        };
        let idx = |k: TrackKind| tracks.iter().position(|t| *t == k);
        if let Some(ti) = idx(TrackKind::Mic).filter(|_| opts.mic) {
            mixer.endpoints.push((ti, false));
        }
        if let Some(ti) = idx(TrackKind::Desktop) {
            mixer.endpoints.push((ti, true));
        }
        for (kind, targets) in [
            (TrackKind::Game, &opts.game),
            (TrackKind::Voice, &opts.voice),
            (TrackKind::Media, &opts.media),
        ] {
            if let Some(ti) = idx(kind) {
                mixer.targets.push((ti, targets.clone()));
            }
        }
        mixer.rescan();
        mixer.stats.endpoint_reopens = 0; // the first opens are not reopens
        mixer.stats.mic = mixer.has_endpoint(false);
        let mic_open = mixer.stats.mic;

        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let thread = std::thread::Builder::new()
            .name("suicap-audio-mix".into())
            .spawn(move || mixer.run(chunk_rx, pkt_tx, flag))
            .expect("spawning the audio mix thread");
        Ok(Self {
            rx: pkt_rx,
            streams,
            process_loopback,
            mic_open,
            stop,
            thread,
        })
    }

    /// Stop capture, emit audio up to now, flush encoders. Remaining packets stay in `rx`.
    pub fn stop(self) -> Result<(AudioStats, Receiver<Packet>)> {
        self.stop.store(true, Ordering::Release);
        let stats = self.thread.join().map_err(|_| AudioError::Panic)??;
        Ok((stats, self.rx))
    }
}

impl Mixer {
    /// A line for a new source on `track`: a freed one if any.
    fn take_line(&mut self, track: usize) -> usize {
        match self.free_lines.pop() {
            Some(l) => {
                self.line_track[l] = track;
                l
            }
            None => {
                self.line_track.push(track);
                self.timeline.add_line()
            }
        }
    }

    fn start_source(&mut self, client: IAudioClient, key: SourceKey, track: usize) -> Result<()> {
        let loopback = !matches!(key, SourceKey::Endpoint { loopback: false, .. });
        init_client(&client, loopback)?;
        let line = self.take_line(track);
        match spawn_source(client, key, line, self.chunk_tx.clone()) {
            Ok(src) => {
                self.sources.push(src);
                Ok(())
            }
            Err(e) => {
                self.free_lines.push(line);
                Err(e)
            }
        }
    }

    /// Index of a running source (not stopping) whose key matches.
    fn live(&self, matches: impl Fn(&SourceKey) -> bool) -> Option<usize> {
        self.sources
            .iter()
            .position(|s| matches(&s.key) && !s.stop.load(Ordering::Acquire) && !s.thread.is_finished())
    }

    fn has_endpoint(&self, loopback: bool) -> bool {
        self.live(|k| matches!(k, SourceKey::Endpoint { loopback: l, .. } if *l == loopback))
            .is_some()
    }

    /// Plan §5/§7 rebind: start loopback sources for target processes that appeared,
    /// follow the default mic/speakers (device removed or default changed), and
    /// recycle the lines of sources that ended. Their tracks continue with silence.
    fn rescan(&mut self) {
        let (done, live): (Vec<_>, Vec<_>) = self.sources.drain(..).partition(|s| s.thread.is_finished());
        self.sources = live;
        for s in done {
            let _ = s.thread.join();
            self.timeline.reset_line(s.line);
            self.free_lines.push(s.line);
        }

        for (ti, loopback) in self.endpoints.clone() {
            let want = default_endpoint_id(loopback).ok();
            let current = self.live(|k| matches!(k, SourceKey::Endpoint { loopback: l, .. } if *l == loopback));
            if let Some(i) = current {
                if let SourceKey::Endpoint { id, .. } = &self.sources[i].key
                    && want.as_deref() == Some(id.as_str())
                {
                    continue; // still on the default device
                }
                self.sources[i].stop.store(true, Ordering::Release); // reaped on a later rescan
            }
            if want.is_some()
                && let Ok((client, id)) = endpoint_client(loopback)
                && self
                    .start_source(client, SourceKey::Endpoint { loopback, id }, ti)
                    .is_ok()
            {
                self.stats.endpoint_reopens += 1;
            }
        }

        let procs = processes();
        let me = std::process::id();
        let mut wanted = Vec::new();
        for (ti, targets) in &self.targets {
            for pid in resolve(targets, &procs).into_iter().filter(|&p| p != me) {
                wanted.push(SourceKey::Process { track: *ti, pid });
            }
        }
        // Process gone (or no longer a target root): stop its source.
        for s in &self.sources {
            if matches!(s.key, SourceKey::Process { .. }) && !wanted.contains(&s.key) {
                s.stop.store(true, Ordering::Release);
            }
        }
        for key in wanted {
            let SourceKey::Process { track: ti, pid } = key else {
                continue;
            };
            if self.live(|k| *k == key).is_some() {
                continue;
            }
            let opened = process_loopback_client(pid).and_then(|c| self.start_source(c, key, ti));
            if opened.is_ok() {
                self.stats.sources.push((self.tracks[ti], pid));
            }
        }
        self.stats.lines = self.timeline.lines();
    }

    fn run(mut self, rx: Receiver<Chunk>, out: Sender<Packet>, stop: Arc<AtomicBool>) -> Result<AudioStats> {
        capture::com_init();
        let frame = self.encoders[0].frame_size;
        let mut last_scan = Instant::now();
        let mut packets = Vec::new();
        loop {
            let stopping = stop.load(Ordering::Acquire);
            match rx.recv_timeout(Duration::from_millis(10)) {
                Ok(c) => {
                    self.timeline.push(c.line, c.qpc_us, &c.samples);
                    while let Ok(c) = rx.try_recv() {
                        self.timeline.push(c.line, c.qpc_us, &c.samples);
                    }
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }
            let latency = if stopping { 0 } else { LATENCY_SAMPLES };
            while self.timeline.ready(capture::qpc_now_us(), frame, latency) {
                self.emit_frame(frame, &mut packets)?;
            }
            for p in packets.drain(..) {
                let _ = out.send(p);
            }
            if stopping {
                break;
            }
            if last_scan.elapsed() >= RESCAN_EVERY {
                self.rescan();
                last_scan = Instant::now();
            }
        }
        for s in &self.sources {
            s.stop.store(true, Ordering::Release);
        }
        for s in self.sources.drain(..) {
            let _ = s.thread.join();
        }
        for (i, e) in self.encoders.iter_mut().enumerate() {
            e.flush(self.first_stream + i as u8, &mut packets)?;
        }
        for p in packets.drain(..) {
            let _ = out.send(p);
        }
        self.stats.resyncs = self.timeline.resyncs();
        self.stats.late_samples = self.timeline.late_samples();
        Ok(self.stats)
    }

    /// Sum lines into tracks, build the limited Mix, encode every track at the same PTS.
    fn emit_frame(&mut self, frame: usize, out: &mut Vec<Packet>) -> Result<()> {
        let (pos, lines) = self.timeline.pop(frame);
        let n = frame * 2;
        let mut tracks = vec![vec![0.0f32; n]; self.tracks.len()];
        for (li, samples) in lines.iter().enumerate() {
            let t = &mut tracks[self.line_track[li]];
            for (d, s) in t.iter_mut().zip(samples) {
                *d += s;
            }
        }
        if let Some(mix_i) = self.tracks.iter().position(|t| *t == TrackKind::Mix) {
            let mut mix = vec![0.0f32; n];
            for (_, t) in tracks.iter().enumerate().filter(|(i, _)| *i != mix_i) {
                for (d, s) in mix.iter_mut().zip(t) {
                    *d += s;
                }
            }
            mix.iter_mut().for_each(|s| *s = soft_limit(*s));
            tracks[mix_i] = mix;
        }
        for (i, (enc, samples)) in self.encoders.iter_mut().zip(&tracks).enumerate() {
            enc.encode(samples, pos, self.first_stream + i as u8, out)?;
        }
        self.stats.frames_per_track += 1;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const START: i64 = 1_000_000_000;

    /// QPC time of sample `samples` (rounded up, so `pos_of` maps back exactly).
    fn us(samples: i64) -> i64 {
        START + (samples * 1_000_000 + RATE - 1) / RATE
    }

    fn chunk(frames: usize, v: f32) -> Vec<f32> {
        vec![v; frames * 2]
    }

    #[test]
    fn contiguous_chunks_follow_sample_count_not_jittery_qpc() {
        let mut t = Timeline::new(START);
        let l = t.add_line();
        t.push(l, us(0), &chunk(480, 1.0));
        // 5 ms of QPC jitter (< 20 ms): still appended contiguously.
        t.push(l, us(480) + 5_000, &chunk(480, 2.0));
        let (pos, out) = t.pop(960);
        assert_eq!(pos, 0);
        assert!(out[0][..960].iter().all(|&s| s == 1.0));
        assert!(out[0][960..].iter().all(|&s| s == 2.0));
        assert_eq!(t.resyncs(), 0);
    }

    #[test]
    fn forward_drift_over_20ms_inserts_silence() {
        let mut t = Timeline::new(START);
        let l = t.add_line();
        t.push(l, us(0), &chunk(480, 1.0));
        t.push(l, us(480 + 1_440), &chunk(480, 1.0)); // 30 ms late
        let (_, out) = t.pop(480 + 1_440 + 480);
        let s = &out[0];
        assert!(s[..960].iter().all(|&x| x == 1.0));
        assert!(s[960..960 + 2_880].iter().all(|&x| x == 0.0));
        assert!(s[960 + 2_880..].iter().all(|&x| x == 1.0));
        assert_eq!(t.resyncs(), 1);
    }

    #[test]
    fn backward_drift_over_20ms_drops_overlap() {
        let mut t = Timeline::new(START);
        let l = t.add_line();
        t.push(l, us(0), &chunk(4_800, 1.0)); // cursor 4800
        t.push(l, us(4_800 - 1_440), &chunk(480, 2.0)); // QPC says 30 ms earlier
        let (_, out) = t.pop(4_800);
        assert!(out[0][(4_800 - 1_440) * 2..(4_800 - 960) * 2].iter().all(|&x| x == 2.0));
        assert_eq!(t.resyncs(), 1);
    }

    #[test]
    fn late_samples_before_emit_are_dropped_and_lines_stay_equal_length() {
        let mut t = Timeline::new(START);
        let a = t.add_line();
        let b = t.add_line(); // silent source: never pushes
        t.push(a, us(0), &chunk(960, 1.0));
        let (_, out) = t.pop(960);
        assert_eq!(out[a].len(), out[b].len());
        assert!(out[b].iter().all(|&x| x == 0.0));
        t.push(a, us(480), &chunk(960, 3.0)); // half of it is already emitted
        assert_eq!(t.late_samples(), 0, "contiguous cursor wins over small jitter");
        let (pos, out) = t.pop(960);
        assert_eq!(pos, 960);
        assert!(out[a].iter().all(|&x| x == 3.0));
    }

    #[test]
    fn source_starting_late_is_placed_by_qpc() {
        let mut t = Timeline::new(START);
        let a = t.add_line();
        t.push(a, us(960), &chunk(960, 1.0));
        let (_, out) = t.pop(1_920);
        assert!(out[a][..1_920].iter().all(|&x| x == 0.0));
        assert!(out[a][1_920..].iter().all(|&x| x == 1.0));
    }

    #[test]
    fn reset_line_is_reused_by_a_new_source_at_its_own_time() {
        let mut t = Timeline::new(START);
        let a = t.add_line();
        t.push(a, us(0), &chunk(960, 1.0));
        t.push(a, us(960), &chunk(960, 1.0)); // left pending: the source then ends
        let _ = t.pop(960);
        t.reset_line(a);
        // A new source on the same line starts 1 s later; nothing from the old one remains.
        t.push(a, us(48_000), &chunk(960, 2.0));
        assert_eq!(t.lines(), 1);
        let (pos, out) = t.pop(960);
        assert_eq!(pos, 960);
        assert!(out[a].iter().all(|&x| x == 0.0), "old samples were dropped");
        let _ = t.pop(48_000 - 1_920);
        let (_, out) = t.pop(960);
        assert!(out[a].iter().all(|&x| x == 2.0));
    }

    #[test]
    fn ready_respects_latency() {
        let t = Timeline::new(START);
        assert!(!t.ready(us(1_024 + LATENCY_SAMPLES - 1), 1_024, LATENCY_SAMPLES));
        assert!(t.ready(us(1_024 + LATENCY_SAMPLES), 1_024, LATENCY_SAMPLES));
    }

    #[test]
    fn soft_limit_is_bounded_and_linear_below_knee() {
        assert_eq!(soft_limit(0.5), 0.5);
        assert_eq!(soft_limit(-0.8), -0.8);
        for x in [1.0f32, 2.0, 10.0, -3.0] {
            assert!(soft_limit(x).abs() <= 1.0);
        }
        assert!(soft_limit(0.9) > soft_limit(0.85));
    }

    #[test]
    fn resolve_picks_root_processes_of_an_exe() {
        let p = |pid, parent, exe: &str| Proc {
            pid,
            parent,
            exe: exe.into(),
        };
        let procs = vec![
            p(1, 0, "explorer.exe"),
            p(10, 1, "chrome.exe"),
            p(11, 10, "chrome.exe"),
            p(12, 10, "chrome.exe"),
            p(20, 1, "Spotify.exe"),
        ];
        let t = vec![
            Target::Exe("chrome.exe".into()),
            Target::Exe("spotify.exe".into()),
            Target::Pid(99),
        ];
        assert_eq!(resolve(&t, &procs), vec![10, 20]);
    }
}
