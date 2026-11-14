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
        atomic::{AtomicBool, AtomicU64, Ordering},
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
            ActivateAudioInterfaceAsync, DEVICE_STATE_ACTIVE, IActivateAudioInterfaceAsyncOperation,
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
    core::{HRESULT, IUnknown, Interface, PCWSTR, Ref, implement, w},
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
const RESCAN_EVERY: Duration = Duration::from_secs(1);

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

/// Moves a track's level from `*gain` to `target` across one frame of interleaved stereo
/// (a linear fade, so muting and unmuting do not click). Settled at 0 it is exact silence.
fn ramp(samples: &mut [f32], gain: &mut f32, target: f32) {
    if *gain == target {
        if target == 0.0 {
            samples.fill(0.0);
        }
        return;
    }
    let n = (samples.len() / 2).max(1) as f32;
    let start = *gain;
    for (k, frame) in samples.chunks_exact_mut(2).enumerate() {
        let g = start + (target - start) * (k as f32 + 1.0) / n;
        frame[0] *= g;
        frame[1] *= g;
    }
    *gain = target;
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
    /// The track's name in the clip: the same words the settings page uses.
    pub fn title(self) -> &'static str {
        match self {
            TrackKind::Mix => "Mix",
            TrackKind::Mic => "Microphone",
            TrackKind::Game => "Game",
            TrackKind::Voice => "Voice chat",
            TrackKind::Media => "Music and media",
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

/// Mute switches shared with the running mixer. Flipping one silences that track at once, with
/// a short fade and without restarting capture; the track also leaves the Mix. A track that
/// was off when capture started has no stream to unmute: that still needs a restart.
#[derive(Debug)]
pub struct LiveTracks {
    on: [AtomicBool; 5],
    /// The chosen microphone (endpoint id); `None` = the system default.
    mic_device: Mutex<Option<String>>,
    /// The latest settings, so the mixer can re-route apps while it runs.
    settings: Mutex<Option<crate::config::AudioSettings>>,
    /// Bumped whenever the app routing or a track switch changed; the mixer re-routes on it.
    routes_rev: AtomicU64,
}

impl LiveTracks {
    /// Everything audible.
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            on: std::array::from_fn(|_| AtomicBool::new(true)),
            mic_device: Mutex::new(None),
            settings: Mutex::new(None),
            routes_rev: AtomicU64::new(0),
        })
    }

    fn slot(kind: TrackKind) -> Option<usize> {
        match kind {
            TrackKind::Mix => Some(0),
            TrackKind::Mic => Some(1),
            TrackKind::Game => Some(2),
            TrackKind::Voice => Some(3),
            TrackKind::Media => Some(4),
            TrackKind::Desktop => None, // the whole desktop: nothing to mute by app
        }
    }

    pub fn set(&self, kind: TrackKind, audible: bool) {
        if let Some(i) = Self::slot(kind) {
            self.on[i].store(audible, Ordering::Release);
        }
    }

    pub fn audible(&self, kind: TrackKind) -> bool {
        Self::slot(kind).is_none_or(|i| self.on[i].load(Ordering::Acquire))
    }

    /// Switch microphones; the mixer moves over within a couple of seconds.
    pub fn set_mic_device(&self, id: Option<String>) {
        if let Ok(mut g) = self.mic_device.lock() {
            *g = id;
        }
    }

    pub fn mic_device(&self) -> Option<String> {
        self.mic_device.lock().ok().and_then(|g| g.clone())
    }

    /// The settings the session started with (not a change: no re-route is triggered).
    fn remember(&self, s: &crate::config::AudioSettings) {
        if let Ok(mut g) = self.settings.lock() {
            *g = Some(s.clone());
        }
    }

    fn settings(&self) -> Option<crate::config::AudioSettings> {
        self.settings.lock().ok().and_then(|g| g.clone())
    }

    fn routes_rev(&self) -> u64 {
        self.routes_rev.load(Ordering::Acquire)
    }

    /// Follow the settings: a track that is switched off is muted, the microphone chosen, and
    /// apps moved between tracks (or parked / released) are re-routed within about a second,
    /// with no restart: each moved app's sound resumes on its new track.
    pub fn apply(&self, s: &crate::config::AudioSettings) {
        if let Ok(mut g) = self.settings.lock() {
            let changed = g.as_ref().is_none_or(|old| {
                old.game != s.game
                    || old.voice != s.voice
                    || old.media != s.media
                    || old.ignored != s.ignored
                    || old.tracks != s.tracks
            });
            *g = Some(s.clone());
            if changed {
                self.routes_rev.fetch_add(1, Ordering::AcqRel);
            }
        }
        self.set_mic_device(s.mic_device.clone());
        self.set(TrackKind::Mix, s.tracks.mix);
        self.set(TrackKind::Mic, s.mic);
        self.set(TrackKind::Game, s.tracks.game);
        self.set(TrackKind::Voice, s.tracks.voice);
        self.set(TrackKind::Media, s.tracks.media);
    }
}

#[derive(Debug, Clone)]
pub struct AudioOptions {
    pub codec: AudioCodec,
    pub mic: bool,
    /// Tracks recorded (Mic follows `mic`). Game, Voice and Media off = muted: no sources are
    /// opened for them, so their apps are not in the Mix either. Mix off drops only the Mix.
    pub mix: bool,
    pub track_game: bool,
    pub track_voice: bool,
    pub track_media: bool,
    /// Something is muted (a track is off, or apps are parked as not recorded). Only per-app
    /// capture can honour that, so the session says so when it is not available.
    pub muting: bool,
    /// Live mute switches; keep a clone to mute tracks while capture runs.
    pub live: Arc<LiveTracks>,
    pub game: Vec<Target>,
    pub voice: Vec<Target>,
    pub media: Vec<Target>,
    /// The full-screen app an empty Game list stood for when capture started. Kept for the whole
    /// session, so re-routing while the settings window is in front does not lose the game.
    foreground: Option<(u32, String)>,
}

impl AudioOptions {
    /// Plan §5 defaults: game = foreground fullscreen app, voice/media by exe name.
    pub fn defaults(codec: AudioCodec) -> Self {
        Self::from_settings(&crate::config::AudioSettings::default(), codec)
    }

    /// From persisted settings; an empty game list means the foreground fullscreen app.
    pub fn from_settings(s: &crate::config::AudioSettings, codec: AudioCodec) -> Self {
        Self::for_app(s, codec, None)
    }

    /// Like `from_settings`, but an empty game list follows `app` (the application chosen as the
    /// video source, running or not) instead of the full-screen app.
    pub fn for_app(s: &crate::config::AudioSettings, codec: AudioCodec, app: Option<&str>) -> Self {
        // The foreground app only matters when the Game track records it.
        let foreground = (s.tracks.game && s.game.is_empty())
            .then(|| match app {
                Some(exe) => Some(
                    processes()
                        .into_iter()
                        .find(|p| p.exe.eq_ignore_ascii_case(exe))
                        .map_or((0, exe.to_owned()), |p| (p.pid, p.exe)),
                ),
                None => foreground_fullscreen_pid()
                    .and_then(|pid| processes().into_iter().find(|p| p.pid == pid).map(|p| (pid, p.exe))),
            })
            .flatten();
        let routes = route(s, foreground.clone());
        Self {
            codec,
            mic: s.mic,
            mix: s.tracks.mix,
            track_game: s.tracks.game,
            track_voice: s.tracks.voice,
            track_media: s.tracks.media,
            muting: !(s.tracks.game && s.tracks.voice && s.tracks.media && s.ignored.is_empty()),
            live: {
                let live = LiveTracks::new();
                live.set_mic_device(s.mic_device.clone());
                live.remember(s);
                live
            },
            game: routes.game,
            voice: routes.voice,
            media: routes.media,
            foreground,
        }
    }
}

/// Which apps are captured for each track.
#[derive(Debug, Default, PartialEq, Eq)]
struct Routes {
    game: Vec<Target>,
    voice: Vec<Target>,
    media: Vec<Target>,
}

/// Decides what is recorded. An app is **muted** (captured nowhere, so it is not in the Mix
/// either) when it is parked as not recorded or sits on a track that is switched off. The
/// rest go to their track, once: an app listed twice stays on the first of Game, Voice, Media.
/// `foreground` is the full-screen app (pid, exe) that an empty Game list stands for.
fn route(s: &crate::config::AudioSettings, foreground: Option<(u32, String)>) -> Routes {
    let t = &s.tracks;
    let eq = |a: &str, b: &str| a.eq_ignore_ascii_case(b);
    let mut muted: Vec<&str> = s.ignored.iter().map(String::as_str).collect();
    for (on, list) in [(t.game, &s.game), (t.voice, &s.voice), (t.media, &s.media)] {
        if !on {
            muted.extend(list.iter().map(String::as_str));
        }
    }
    let is_muted = |n: &str| muted.iter().any(|m| eq(m, n));
    let mut claimed: Vec<String> = Vec::new();
    let take = |on: bool, list: &[String], claimed: &mut Vec<String>| -> Vec<Target> {
        let mut out = Vec::new();
        if on {
            for n in list.iter().filter(|n| !is_muted(n)) {
                if !claimed.iter().any(|c| eq(c, n)) {
                    claimed.push(n.clone());
                    out.push(Target::Exe(n.clone()));
                }
            }
        }
        out
    };
    let mut game = take(t.game, &s.game, &mut claimed);
    if t.game
        && s.game.is_empty()
        && let Some((pid, exe)) = foreground
        && !is_muted(&exe)
    {
        // pid 0 = a chosen app that is not running yet: follow it by name
        game.push(if pid == 0 { Target::Exe(exe.clone()) } else { Target::Pid(pid) });
        claimed.push(exe);
    }
    let voice = take(t.voice, &s.voice, &mut claimed);
    let media = take(t.media, &s.media, &mut claimed);
    Routes { game, voice, media }
}

/// The apps to capture for each track that has a stream, from the current settings. Tracks
/// that were off when capture started have no stream and stay without sources.
fn live_targets(
    s: &crate::config::AudioSettings,
    foreground: Option<(u32, String)>,
    tracks: &[TrackKind],
    written: &[bool],
) -> Vec<(usize, Vec<Target>)> {
    let r = route(s, foreground);
    [
        (TrackKind::Game, r.game),
        (TrackKind::Voice, r.voice),
        (TrackKind::Media, r.media),
    ]
    .into_iter()
    .filter_map(|(kind, targets)| {
        let i = tracks.iter().position(|t| *t == kind)?;
        written.get(i).copied()?.then_some((i, targets))
    })
    .collect()
}

/// Executable name without `.exe` of the foreground fullscreen app, for clip folders
/// (plan §6.2); `None` on the desktop.
pub fn foreground_app_name() -> Option<String> {
    let exe = match crate::targets::active_app() {
        Some(exe) => exe, // recording one application: its name, full screen or not
        None => {
            let pid = foreground_fullscreen_pid()?;
            processes().into_iter().find(|p| p.pid == pid)?.exe
        }
    };
    let stem = exe
        .strip_suffix(".exe")
        .or_else(|| exe.strip_suffix(".EXE"))
        .unwrap_or(&exe);
    (!stem.is_empty()).then(|| stem.to_owned())
}

/// Exe name (with `.exe`) of the full-screen app in front, for the status bar. The desktop
/// shell covers the screen too, so it is not a game.
pub fn foreground_game_exe() -> Option<String> {
    let pid = foreground_fullscreen_pid()?;
    let exe = processes().into_iter().find(|p| p.pid == pid)?.exe;
    const SHELL: [&str; 4] = [
        "explorer.exe",
        "searchhost.exe",
        "shellexperiencehost.exe",
        "applicationframehost.exe",
    ];
    (!exe.is_empty() && !SHELL.iter().any(|s| exe.eq_ignore_ascii_case(s))).then_some(exe)
}

#[derive(Debug, Clone)]
struct Proc {
    pid: u32,
    parent: u32,
    exe: String,
}

/// (pid, exe name) of every running process.
pub fn process_list() -> Vec<(u32, String)> {
    processes().into_iter().map(|p| (p.pid, p.exe)).collect()
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

/// One source per process tree. Loopback of a process includes its children, so a process
/// that sits under another wanted one, or is wanted by two tracks, would be heard twice in
/// the Mix. The first track wins a shared process; an ancestor wins over its descendants.
fn dedupe_wanted(wanted: Vec<(usize, u32)>, procs: &[Proc]) -> Vec<(usize, u32)> {
    let parent: HashMap<u32, u32> = procs.iter().map(|p| (p.pid, p.parent)).collect();
    let all: Vec<u32> = wanted.iter().map(|w| w.1).collect();
    let under_another = |pid: u32| {
        let mut cur = parent.get(&pid).copied();
        for _ in 0..64 {
            match cur {
                Some(p) if p != 0 && p != pid => {
                    if all.contains(&p) {
                        return true;
                    }
                    cur = parent.get(&p).copied();
                }
                _ => return false,
            }
        }
        false
    };
    let mut seen = Vec::new();
    wanted
        .into_iter()
        .filter(|&(_, pid)| {
            !under_another(pid) && !seen.contains(&pid) && {
                seen.push(pid);
                true
            }
        })
        .collect()
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

/// The chosen capture device if it is plugged in and active, else the system default. Only
/// microphones are chosen: loopback always follows the default playback device.
fn endpoint_for(loopback: bool, selected: Option<&str>) -> Result<IMMDevice> {
    if !loopback && let Some(id) = selected.filter(|s| !s.is_empty()) {
        let wide: Vec<u16> = id.encode_utf16().chain(Some(0)).collect();
        // SAFETY: COM calls on interfaces we own; `wide` outlives GetDevice.
        unsafe {
            if let Ok(en) = CoCreateInstance::<_, IMMDeviceEnumerator>(&MMDeviceEnumerator, None, CLSCTX_ALL)
                && let Ok(dev) = en.GetDevice(PCWSTR(wide.as_ptr()))
                && dev.GetState().is_ok_and(|s| s == DEVICE_STATE_ACTIVE)
            {
                return Ok(dev);
            }
        }
    }
    default_endpoint(loopback)
}

/// ID of the device capture should use now (see `endpoint_for`).
fn default_endpoint_id(loopback: bool, selected: Option<&str>) -> Result<String> {
    let dev = endpoint_for(loopback, selected)?;
    // SAFETY: GetId returns a CoTaskMem string that we free after copying it.
    unsafe {
        let id = dev.GetId()?;
        let text = id.to_string().unwrap_or_default();
        CoTaskMemFree(Some(id.0 as *const _));
        Ok(text)
    }
}

fn endpoint_client(loopback: bool, selected: Option<&str>) -> Result<(IAudioClient, String)> {
    let dev = endpoint_for(loopback, selected)?;
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
    /// One per track; `None` = the track is not written (its sound still feeds the Mix).
    encoders: Vec<Option<AudioEncoder>>,
    /// Muxer stream index per track (meaningful where the encoder is `Some`).
    stream_ix: Vec<u8>,
    frame: usize,
    /// Live mute switches and each track's current level (faded between 0 and 1).
    live: Arc<LiveTracks>,
    gains: Vec<f32>,
    timeline: Timeline,
    /// line index -> track index
    line_track: Vec<usize>,
    sources: Vec<Source>,
    /// Lines of ended sources, ready for reuse.
    free_lines: Vec<usize>,
    /// Wanted endpoint sources: (track, loopback).
    endpoints: Vec<(usize, bool)>,
    targets: Vec<(usize, Vec<Target>)>,
    /// Sticky foreground app (see `AudioOptions::foreground`) and the routing revision applied.
    foreground: Option<(u32, String)>,
    routes_rev: u64,
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
        let written = |t: &TrackKind| match t {
            TrackKind::Mix => opts.mix,
            TrackKind::Mic => opts.mic,
            TrackKind::Game => opts.track_game,
            TrackKind::Voice => opts.track_voice,
            TrackKind::Media => opts.track_media,
            TrackKind::Desktop => true,
        };
        let encoders = tracks
            .iter()
            .map(|t| {
                written(t)
                    .then(|| AudioEncoder::open(opts.codec, origin_us).map_err(AudioError::from))
                    .transpose()
            })
            .collect::<Result<Vec<_>>>()?;
        let is_written: Vec<bool> = encoders.iter().map(Option::is_some).collect();
        let mut stream_ix = Vec::with_capacity(tracks.len());
        let mut streams = Vec::new();
        for (t, e) in tracks.iter().zip(&encoders) {
            stream_ix.push(first_stream + streams.len() as u8);
            if let Some(e) = e {
                streams.push(AudioStream {
                    title: t.title(),
                    params: Arc::new(e.stream_params()?),
                    default: false,
                });
            }
        }
        // The Mix plays by default; without it, the first written track does.
        let default_ix = tracks
            .iter()
            .zip(&is_written)
            .filter(|(_, w)| **w)
            .position(|(t, _)| *t == TrackKind::Mix)
            .or((!streams.is_empty()).then_some(0));
        if let Some(i) = default_ix {
            streams[i].default = true;
        }
        let frame = encoders.iter().flatten().next().map_or(1024, |e| e.frame_size);

        let mut mixer = Mixer {
            tracks: tracks.clone(),
            encoders,
            stream_ix,
            frame,
            live: opts.live.clone(),
            gains: vec![1.0; tracks.len()],
            timeline: Timeline::new(origin_us),
            line_track: Vec::new(),
            sources: Vec::new(),
            free_lines: Vec::new(),
            endpoints: Vec::new(),
            targets: Vec::new(),
            foreground: opts.foreground.clone(),
            routes_rev: opts.live.routes_rev(),
            chunk_tx,
            stats: AudioStats {
                tracks: tracks
                    .iter()
                    .zip(&is_written)
                    .filter(|(_, w)| **w)
                    .map(|(t, _)| t.title())
                    .collect(),
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
            // A track that is off opens no sources at all, so its apps are not in the Mix.
            if let Some(ti) = idx(kind).filter(|_| written(&kind)) {
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

        let mic = self.live.mic_device();
        for (ti, loopback) in self.endpoints.clone() {
            let chosen = if loopback { None } else { mic.as_deref() };
            let want = default_endpoint_id(loopback, chosen).ok();
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
                && let Ok((client, id)) = endpoint_client(loopback, chosen)
                && self
                    .start_source(client, SourceKey::Endpoint { loopback, id }, ti)
                    .is_ok()
            {
                self.stats.endpoint_reopens += 1;
            }
        }

        let procs = processes();
        let me = std::process::id();
        let mut pairs = Vec::new();
        for (ti, targets) in &self.targets {
            for pid in resolve(targets, &procs).into_iter().filter(|&p| p != me) {
                pairs.push((*ti, pid));
            }
        }
        let wanted: Vec<SourceKey> = dedupe_wanted(pairs, &procs)
            .into_iter()
            .map(|(track, pid)| SourceKey::Process { track, pid })
            .collect();
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

    /// Take the apps per track from the latest settings. `rescan` then stops the sources that
    /// no longer belong and starts the ones that now do.
    fn apply_routes(&mut self) {
        let Some(s) = self.live.settings() else { return };
        let written: Vec<bool> = self.encoders.iter().map(Option::is_some).collect();
        self.targets = live_targets(&s, self.foreground.clone(), &self.tracks, &written);
    }

    fn run(mut self, rx: Receiver<Chunk>, out: Sender<Packet>, stop: Arc<AtomicBool>) -> Result<AudioStats> {
        capture::com_init();
        let frame = self.frame;
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
            let rev = self.live.routes_rev();
            if rev != self.routes_rev {
                // Apps moved, parked or released: re-route now instead of waiting for the next scan.
                self.routes_rev = rev;
                self.apply_routes();
                self.rescan();
                last_scan = Instant::now();
            } else if last_scan.elapsed() >= RESCAN_EVERY {
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
            if let Some(e) = e {
                e.flush(self.stream_ix[i], &mut packets)?;
            }
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
        // Live mute: fade each track to silent (or back) before the Mix is summed from them,
        // so a muted app is gone from its own track and from the Mix.
        for (i, kind) in self.tracks.iter().enumerate() {
            if !matches!(kind, TrackKind::Mix | TrackKind::Desktop) {
                ramp(&mut tracks[i], &mut self.gains[i], f32::from(self.live.audible(*kind)));
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
            ramp(
                &mut mix,
                &mut self.gains[mix_i],
                f32::from(self.live.audible(TrackKind::Mix)),
            );
            tracks[mix_i] = mix;
        }
        for (i, (enc, samples)) in self.encoders.iter_mut().zip(&tracks).enumerate() {
            if let Some(enc) = enc {
                enc.encode(samples, pos, self.stream_ix[i], out)?;
            }
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

    /// Real stack: a chosen input is used; a missing one falls back to the default input.
    #[test]
    fn chosen_microphone_is_used_and_a_missing_one_falls_back() {
        capture::com_init();
        let Ok(default) = default_endpoint_id(false, None) else {
            return; // no input device on this machine
        };
        let devices = crate::apps::input_devices();
        if let Some(other) = devices.iter().find(|d| d.id != default) {
            assert_eq!(default_endpoint_id(false, Some(&other.id)).unwrap(), other.id);
        }
        let gone = "{0.0.1.00000000}.{00000000-0000-0000-0000-000000000000}";
        assert_eq!(default_endpoint_id(false, Some(gone)).unwrap(), default);
        // loopback ignores the choice: it always follows the default playback device
        if let (Ok(a), Ok(b)) = (default_endpoint_id(true, None), default_endpoint_id(true, Some(gone))) {
            assert_eq!(a, b);
        }
    }

    #[test]
    fn ramp_fades_to_exact_silence_and_back() {
        let mut gain = 1.0;
        let mut a = vec![1.0f32; 8]; // 4 stereo frames
        ramp(&mut a, &mut gain, 0.0);
        assert_eq!(gain, 0.0);
        assert!(a.windows(2).all(|w| w[1] <= w[0]), "monotonic fade");
        assert_eq!((a[0], a[1]), (0.75, 0.75));
        assert_eq!((a[6], a[7]), (0.0, 0.0));
        // settled at 0: whatever arrives is silenced exactly
        let mut b = vec![0.5f32; 8];
        ramp(&mut b, &mut gain, 0.0);
        assert!(b.iter().all(|&x| x == 0.0));
        // and back up to 1
        let mut c = vec![1.0f32; 8];
        ramp(&mut c, &mut gain, 1.0);
        assert_eq!((c[0], c[7]), (0.25, 1.0));
        // unity is untouched
        let mut d = vec![0.3f32; 8];
        ramp(&mut d, &mut gain, 1.0);
        assert!(d.iter().all(|&x| x == 0.3));
    }

    #[test]
    fn live_switches_follow_settings_and_never_mute_the_desktop_fallback() {
        let live = LiveTracks::new();
        assert!(live.audible(TrackKind::Voice));
        let s = settings(|s| {
            s.tracks.voice = false;
            s.mic = false;
        });
        live.apply(&s);
        assert!(!live.audible(TrackKind::Voice));
        assert!(!live.audible(TrackKind::Mic));
        assert!(live.audible(TrackKind::Game) && live.audible(TrackKind::Mix));
        assert!(live.audible(TrackKind::Desktop));
        live.set(TrackKind::Voice, true);
        assert!(live.audible(TrackKind::Voice));
    }

    fn settings(f: impl FnOnce(&mut crate::config::AudioSettings)) -> crate::config::AudioSettings {
        let mut s = crate::config::AudioSettings {
            game: vec!["game.exe".into()],
            voice: vec!["discord.exe".into()],
            media: vec!["spotify.exe".into(), "chrome.exe".into()],
            ..Default::default()
        };
        f(&mut s);
        s
    }
    fn exes(names: &[&str]) -> Vec<Target> {
        names.iter().map(|n| Target::Exe((*n).into())).collect()
    }

    #[test]
    fn live_targets_follow_moved_apps_and_skip_tracks_without_a_stream() {
        use TrackKind::*;
        let tracks = [Mix, Mic, Game, Voice, Media];
        let written = [true, true, true, true, false]; // Media was off at start: no stream
        let t = live_targets(&settings(|_| {}), None, &tracks, &written);
        assert_eq!(t, vec![(2, exes(&["game.exe"])), (3, exes(&["discord.exe"]))]);
        // discord.exe moved from Voice to Game: it now feeds the Game track.
        let moved = settings(|s| {
            s.voice.clear();
            s.game.push("discord.exe".into());
        });
        let t = live_targets(&moved, None, &tracks, &written);
        assert_eq!(t, vec![(2, exes(&["game.exe", "discord.exe"])), (3, vec![])]);
        // Parked apps are captured nowhere.
        let parked = settings(|s| s.ignored = vec!["game.exe".into()]);
        assert_eq!(live_targets(&parked, None, &tracks, &written)[0], (2, vec![]));
    }

    #[test]
    fn live_targets_keep_the_game_found_at_start_when_the_list_is_empty() {
        use TrackKind::*;
        let tracks = [Mix, Mic, Game, Voice, Media];
        let all = [true; 5];
        let s = settings(|s| s.game.clear());
        let t = live_targets(&s, Some((4242, "halo.exe".into())), &tracks, &all);
        assert_eq!(t[0], (2, vec![Target::Pid(4242)]));
        // Without the remembered app (the settings window in front), nothing would be left.
        assert_eq!(live_targets(&s, None, &tracks, &all)[0], (2, vec![]));
    }

    #[test]
    fn changing_routes_bumps_the_revision_but_mic_or_identical_settings_do_not() {
        let live = LiveTracks::new();
        let base = settings(|_| {});
        live.remember(&base);
        let r0 = live.routes_rev();
        live.apply(&base);
        assert_eq!(live.routes_rev(), r0, "same routing: nothing to redo");
        live.apply(&settings(|s| s.mic_device = Some("x".into())));
        assert_eq!(live.routes_rev(), r0, "a microphone switch is not a re-route");
        live.apply(&settings(|s| s.media.push("vlc.exe".into())));
        assert_eq!(live.routes_rev(), r0 + 1);
        live.apply(&settings(|s| s.tracks.voice = false));
        assert_eq!(live.routes_rev(), r0 + 2);
    }

    #[test]
    fn route_sends_each_app_to_its_track() {
        let r = route(&settings(|_| {}), None);
        assert_eq!(r.game, exes(&["game.exe"]));
        assert_eq!(r.voice, exes(&["discord.exe"]));
        assert_eq!(r.media, exes(&["spotify.exe", "chrome.exe"]));
    }

    #[test]
    fn a_track_that_is_off_mutes_its_apps_entirely() {
        let r = route(&settings(|s| s.tracks.voice = false), None);
        assert!(r.voice.is_empty());
        // ...and they are not smuggled in through another track
        let r = route(
            &settings(|s| {
                s.tracks.voice = false;
                s.media.push("discord.exe".into());
            }),
            None,
        );
        assert!(r.voice.is_empty());
        assert_eq!(r.media, exes(&["spotify.exe", "chrome.exe"]));
    }

    #[test]
    fn parked_apps_are_recorded_nowhere() {
        let r = route(
            &settings(|s| {
                s.ignored = vec!["Chrome.EXE".into(), "game.exe".into()];
            }),
            None,
        );
        assert!(r.game.is_empty());
        assert_eq!(r.media, exes(&["spotify.exe"]));
    }

    #[test]
    fn an_empty_game_list_follows_the_foreground_app_unless_muted() {
        let fg = || Some((42, "eldenring.exe".to_string()));
        let open = settings(|s| s.game.clear());
        assert_eq!(route(&open, fg()).game, vec![Target::Pid(42)]);
        let off = settings(|s| {
            s.game.clear();
            s.tracks.game = false;
        });
        assert!(route(&off, fg()).game.is_empty());
        let parked = settings(|s| {
            s.game.clear();
            s.ignored = vec!["eldenring.exe".into()];
        });
        assert!(route(&parked, fg()).game.is_empty());
    }

    #[test]
    fn an_app_listed_twice_is_captured_once() {
        let r = route(&settings(|s| s.media.push("game.exe".into())), None);
        assert_eq!(r.game, exes(&["game.exe"]));
        assert_eq!(r.media, exes(&["spotify.exe", "chrome.exe"]));
        // the foreground app is already claimed by an entry of the same name
        let fg = Some((7, "chrome.exe".to_string()));
        let r = route(&settings(|s| s.game.clear()), fg);
        assert_eq!(r.game, vec![Target::Pid(7)]);
        assert_eq!(r.media, exes(&["spotify.exe"]));
    }

    #[test]
    fn dedupe_keeps_one_source_per_process_tree() {
        let p = |pid, parent, exe: &str| Proc {
            pid,
            parent,
            exe: exe.into(),
        };
        let procs = vec![
            p(1, 0, "explorer.exe"),
            p(10, 1, "launcher.exe"),
            p(11, 10, "game.exe"),
            p(20, 1, "x.exe"),
        ];
        // game (track 0) sits under the launcher (track 2): the launcher's tree already has it
        assert_eq!(
            dedupe_wanted(vec![(0, 11), (2, 10), (1, 20)], &procs),
            vec![(2, 10), (1, 20)]
        );
        // one process wanted by two tracks: the first track keeps it
        assert_eq!(dedupe_wanted(vec![(0, 20), (2, 20)], &procs), vec![(0, 20)]);
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
