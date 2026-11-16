//! App state: settings, the armed session and the status the UI shows.
//!
//! Lock order: `session` before `status`. Session callbacks run on the video thread
//! and only take `status` / `settings`, never `session` (dropping a `Session` joins
//! that thread).

use std::{
    path::PathBuf,
    sync::{Mutex, MutexGuard},
};

use serde::Serialize;
use suicap_core::{
    audio::AudioOptions,
    config::{RecordingMode, Settings},
    encode::AudioCodec,
    mux::ClipInfo,
    pipeline::{Session, SessionEvent, SessionOptions, VideoOptions},
};
use tauri::{AppHandle, Emitter, Manager};

use crate::{feedback, tray};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Phase {
    #[default]
    Idle,
    Starting,
    Armed,
    /// The video path failed and is being rebuilt (plan §7); the buffer restarts empty.
    Recovering,
    Error,
}

/// Which audio tracks the running session has. A track in the session can be muted and
/// unmuted live; one that is missing needs a capture restart to appear.
#[derive(Debug, Clone, Copy, Serialize, Default, PartialEq, Eq)]
pub struct AudioLive {
    pub mix: bool,
    pub mic: bool,
    pub game: bool,
    pub voice: bool,
    pub media: bool,
}

/// What the video source records right now.
#[derive(Debug, Clone, Serialize)]
pub struct CaptureTarget {
    /// "display", "window" or "waiting" (the last frame is held until the app is back).
    pub kind: String,
    pub exe: Option<String>,
    pub title: Option<String>,
}

impl Default for CaptureTarget {
    fn default() -> Self {
        Self {
            kind: "display".into(),
            exe: None,
            title: None,
        }
    }
}

/// Everything the settings window and tray menu display.
#[derive(Debug, Clone, Serialize, Default)]
pub struct Status {
    pub phase: Phase,
    pub mode: RecordingMode,
    pub encoder: String,
    pub resolution: String,
    pub source: String,
    pub capture_target: CaptureTarget,
    pub buffer_secs: u32,
    pub buffered_secs: f64,
    pub buffered_mb: f64,
    pub buffer_estimate_mb: u64,
    pub recording: bool,
    pub recording_paused: bool,
    pub waiting_for_source: bool,
    pub recording_secs: f64,
    pub segment_secs: f64,
    pub segment_bytes: u64,
    pub session_bytes: u64,
    pub segment_index: u32,
    pub frames: u64,
    pub dropped: u64,
    pub error: Option<String>,
    /// Degraded features (fallback monitor, no per-app audio, capture border...).
    pub notes: Vec<String>,
    /// Times the video path was rebuilt since this session started.
    pub recoveries: u64,
    pub last_clip: Option<String>,
    /// Shortcuts that could not be registered (taken by another app).
    pub hotkey_errors: Vec<String>,
    /// Set when the settings file was unreadable and defaults were used.
    pub settings_warning: Option<String>,
    /// Audio tracks of the running session (all false while capture is off).
    pub audio_live: AudioLive,
}

pub struct AppState {
    settings: Mutex<Settings>,
    session: Mutex<Option<Session>>,
    status: Mutex<Status>,
    /// Mute switches of the running session's mixer.
    live: Mutex<Option<std::sync::Arc<suicap_core::audio::LiveTracks>>>,
    pub settings_path: PathBuf,
    pub default_clips_dir: PathBuf,
    pub default_temp_dir: PathBuf,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl AppState {
    pub fn new(
        settings: Settings,
        warning: Option<String>,
        settings_path: PathBuf,
        default_clips_dir: PathBuf,
        default_temp_dir: PathBuf,
    ) -> Self {
        Self {
            status: Mutex::new(Status {
                settings_warning: warning,
                mode: settings.mode,
                buffer_secs: settings.buffer_secs,
                ..Default::default()
            }),
            settings: Mutex::new(settings),
            session: Mutex::new(None),
            live: Mutex::new(None),
            settings_path,
            default_clips_dir,
            default_temp_dir,
        }
    }

    pub fn settings(&self) -> Settings {
        lock(&self.settings).clone()
    }

    pub fn set_settings(&self, s: Settings) {
        *lock(&self.settings) = s;
    }

    pub fn status(&self) -> Status {
        lock(&self.status).clone()
    }

    /// Mute or unmute the running session's tracks to match `a`, without a restart.
    pub fn apply_live(&self, a: &suicap_core::config::AudioSettings) {
        if let Some(live) = lock(&self.live).as_ref() {
            live.apply(a);
        }
    }

    /// Push updated continuous recording options into the running session live.
    pub fn update_continuous(&self, continuous: suicap_core::config::ContinuousSettings) {
        if let Some(s) = lock(&self.session).as_ref().filter(|s| s.is_running()) {
            s.update_continuous(continuous);
        }
    }

    pub fn clips_dir(&self) -> PathBuf {
        lock(&self.settings)
            .clips_dir
            .clone()
            .unwrap_or_else(|| self.default_clips_dir.clone())
    }

    pub fn temp_dir(&self) -> PathBuf {
        lock(&self.settings)
            .temp_dir
            .clone()
            .unwrap_or_else(|| self.default_temp_dir.clone())
    }

    pub fn is_armed(&self) -> bool {
        lock(&self.session).as_ref().is_some_and(Session::is_running)
    }

    pub fn update_status(&self, app: &AppHandle, f: impl FnOnce(&mut Status)) {
        let snapshot = {
            let mut st = lock(&self.status);
            f(&mut st);
            st.clone()
        };
        tray::refresh(app, &snapshot);
        crate::badge::sync(app);
        let has_ui = app.get_webview_window(crate::tray::SETTINGS).is_some();
        let has_badge = app.get_webview_window(crate::badge::BADGE).is_some();
        if has_ui || has_badge {
            let _ = app.emit("status", &snapshot);
        }
    }

    /// Start capture with the current settings. A session already running is kept.
    pub fn arm(&self, app: &AppHandle) {
        let mut slot = lock(&self.session);
        if slot.as_ref().is_some_and(Session::is_running) {
            return;
        }
        drop(slot.take()); // a finished session: joins at once
        suicap_core::pipeline::clear_preview_cache();
        let s = self.settings();
        let codec = AudioCodec::for_extension(s.container.ext());
        let audio = s.audio.enabled.then(|| {
            AudioOptions::for_app(
                &s.audio,
                codec,
                (s.source.kind == suicap_core::config::SourceKind::App).then_some(s.source.exe.as_str()),
            )
        });
        *lock(&self.live) = audio.as_ref().map(|a| a.live.clone());
        let audio_live = if audio.is_some() {
            AudioLive {
                mix: s.audio.tracks.mix,
                mic: s.audio.mic,
                game: s.audio.tracks.game,
                voice: s.audio.tracks.voice,
                media: s.audio.tracks.media,
            }
        } else {
            AudioLive::default()
        };
        let opts = SessionOptions {
            mode: s.mode,
            continuous: s.continuous.clone(),
            buffer_secs: s.buffer_secs,
            video: VideoOptions::from_settings(&s),
            audio,
            clips_dir: self.clips_dir(),
            container: s.container,
        };
        self.update_status(app, |st| {
            *st = Status {
                phase: Phase::Starting,
                mode: s.mode,
                audio_live,
                buffer_secs: s.buffer_secs,
                last_clip: st.last_clip.take(),
                hotkey_errors: std::mem::take(&mut st.hotkey_errors),
                settings_warning: st.settings_warning.take(),
                ..Default::default()
            };
        });
        let handle = app.clone();
        match Session::start(opts, move |e| on_session_event(&handle, e)) {
            Ok(session) => *slot = Some(session),
            Err(e) => self.update_status(app, |st| {
                st.phase = Phase::Error;
                st.error = Some(format!("could not start the capture thread: {e}"));
            }),
        }
    }

    /// Stop capture; waits for saves in progress. Call off the main thread.
    pub fn disarm(&self, app: &AppHandle) {
        let session = lock(&self.session).take();
        if let Some(s) = session {
            s.stop();
        }
        suicap_core::pipeline::clear_preview_cache();
        *lock(&self.live) = None;
        self.update_status(app, |st| {
            st.audio_live = AudioLive::default();
            if st.phase != Phase::Error {
                st.phase = Phase::Idle;
            }
            st.buffered_secs = 0.0;
            st.buffered_mb = 0.0;
            st.recording = false;
            st.recording_paused = false;
            st.waiting_for_source = false;
            st.recording_secs = 0.0;
            st.segment_secs = 0.0;
            st.segment_bytes = 0;
            st.session_bytes = 0;
            st.segment_index = 0;
        });
    }

    /// Instant feedback for a queued save/screenshot: the chime and a badge flash, before
    /// the capture thread has even picked the request up.
    fn acknowledge(&self, app: &AppHandle, kind: &'static str, label: &'static str) {
        if self.settings().save_sound {
            feedback::play_chime();
        }
        emit_badge_flash(
            app,
            BadgeFlash {
                kind,
                label,
                secs: None,
                tone: "ok",
                duration_ms: 1200,
            },
        );
    }

    /// Save the last `secs` seconds (`None` = whole buffer).
    pub fn save(&self, app: &AppHandle, secs: Option<u32>) {
        let queued = lock(&self.session)
            .as_ref()
            .filter(|s| s.is_running())
            .map(|s| s.save(secs));
        match queued {
            // Acknowledge the key press now; the file is written a moment later.
            Some(true) => self.acknowledge(app, "clip_saving", "SAVING CLIP"),
            Some(false) => feedback::notify(app, "Save skipped", "Saves are queued too quickly; try again."),
            None => feedback::notify(app, "Replay buffer is off", "Arm SuiCap from the tray to save clips."),
        }
    }

    /// Save a PNG of the screen.
    pub fn screenshot(&self, app: &AppHandle) {
        let queued = lock(&self.session)
            .as_ref()
            .filter(|s| s.is_running())
            .map(|s| s.screenshot());
        match queued {
            Some(true) => self.acknowledge(app, "screenshot_saving", "SCREENSHOT"),
            Some(false) => feedback::notify(app, "Screenshot skipped", "Try again in a moment."),
            None => feedback::notify(
                app,
                "Replay buffer is off",
                "Arm SuiCap from the tray to take screenshots.",
            ),
        }
    }

    /// Start continuous recording on the active session.
    pub fn record_start(&self, app: &AppHandle) -> Result<(), String> {
        let queued = lock(&self.session)
            .as_ref()
            .filter(|s| s.is_running())
            .map(|s| s.record_start());
        match queued {
            Some(true) => Ok(()),
            Some(false) => Err("Recording could not be started right now; try again.".into()),
            None => {
                feedback::notify(app, "Capture is off", "Arm SuiCap from the tray to record.");
                Err("Capture is off".into())
            }
        }
    }

    /// Stop continuous recording on the active session.
    pub fn record_stop(&self) -> Result<(), String> {
        let queued = lock(&self.session)
            .as_ref()
            .filter(|s| s.is_running())
            .map(|s| s.record_stop());
        match queued {
            Some(true) => Ok(()),
            Some(false) => Err("Recording could not be stopped right now; try again.".into()),
            None => Err("Capture is off".into()),
        }
    }

    /// Start/stop recording (arming first in continuous mode if capture is off).
    pub fn record_toggle(&self, app: &AppHandle) -> Result<(), String> {
        if !self.is_armed() {
            if self.settings().mode == RecordingMode::Continuous {
                self.arm(app);
                return Ok(());
            }
            feedback::notify(app, "Replay buffer is off", "Turn replay on before recording.");
            return Err("Replay buffer is off".into());
        }
        let st = self.status();
        if st.recording || st.waiting_for_source {
            self.record_stop()
        } else {
            self.record_start(app)
        }
    }

    /// Toggle pause on the active recording.
    pub fn record_pause(&self) -> Result<(), String> {
        let queued = lock(&self.session)
            .as_ref()
            .filter(|s| s.is_running())
            .map(|s| s.record_toggle_pause());
        match queued {
            Some(true) => Ok(()),
            Some(false) => Err("Recording pause could not be queued; try again.".into()),
            None => Err("Capture is off".into()),
        }
    }

    /// Fetch a scaled preview of the running session's newest frame, if armed.
    pub fn session_preview(
        &self,
        max_w: u32,
    ) -> Option<Result<(suicap_core::shot::Image, suicap_core::pipeline::VideoStats), String>> {
        lock(&self.session)
            .as_ref()
            .filter(|s| s.is_running())
            .map(|s| s.preview(max_w))
    }

    /// Cut the current continuous segment at the next keyframe and start a new file.
    pub fn record_split(&self) -> Result<(), String> {
        let queued = lock(&self.session)
            .as_ref()
            .filter(|s| s.is_running())
            .map(|s| s.record_split());
        match queued {
            Some(true) => Ok(()),
            Some(false) => Err("Segment split could not be queued; try again.".into()),
            None => Err("Capture is off".into()),
        }
    }
}

/// Delete the oldest clips when the library is over the user's limit (off at 0).
fn enforce_quota(app: &AppHandle, state: &AppState) {
    let gb = state.settings().clips_quota_gb;
    if gb == 0 {
        return;
    }
    let (app, dir) = (app.clone(), state.clips_dir());
    std::thread::spawn(move || {
        if !suicap_core::clips::enforce_quota(&dir, u64::from(gb) << 30).is_empty() {
            let _ = app.emit("clips_changed", ());
        }
    });
}

#[derive(Debug, Clone, Serialize)]
pub struct BadgeFlash {
    pub kind: &'static str,
    pub label: &'static str,
    pub secs: Option<f64>,
    pub tone: &'static str,
    pub duration_ms: u32,
}

fn emit_badge_flash(app: &AppHandle, flash: BadgeFlash) {
    crate::badge::hold_for_flash(app, std::time::Duration::from_millis(u64::from(flash.duration_ms)));
    let _ = app.emit("badge_flash", &flash);
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SaveKind {
    ReplayClip,
    SegmentSplit,
    RecordingFinal,
}

fn on_clip_saved(app: &AppHandle, state: &AppState, clip: ClipInfo, kind: SaveKind) {
    let secs = (clip.last_pts_us - clip.first_pts_us).max(0) as f64 / 1e6;
    let name = clip
        .path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let settings = state.settings();
    let announce = kind != SaveKind::SegmentSplit || settings.continuous.notify_on_split;
    let announced = crate::badge::announces_clips(app);
    let (flash_kind, flash_label) = match kind {
        SaveKind::ReplayClip => ("clip_saved", "CLIP SAVED"),
        SaveKind::SegmentSplit => ("segment_saved", "SEGMENT SAVED"),
        SaveKind::RecordingFinal => ("recording_saved", "RECORDING SAVED"),
    };
    emit_badge_flash(
        app,
        BadgeFlash {
            kind: flash_kind,
            label: flash_label,
            secs: Some(secs),
            tone: "ok",
            duration_ms: 3500,
        },
    );
    state.update_status(app, |st| {
        if kind == SaveKind::RecordingFinal {
            reset_recording_status(st);
        }
        st.last_clip = Some(clip.path.to_string_lossy().into_owned());
    });
    let _ = app.emit("clip_saved", clip.path.to_string_lossy());
    if announce {
        let _ = app.emit("clip_flash", secs);
        match kind {
            SaveKind::ReplayClip => feedback::clip_saved(app, &settings, &name, secs, !announced),
            SaveKind::SegmentSplit => {
                feedback::media_saved(app, &settings, "Segment saved", &name, secs, true, !announced)
            }
            SaveKind::RecordingFinal => {
                feedback::media_saved(app, &settings, "Recording saved", &name, secs, true, !announced)
            }
        }
    }
    enforce_quota(app, state);
}

fn reset_recording_status(st: &mut Status) {
    st.recording = false;
    st.recording_paused = false;
    st.waiting_for_source = false;
    st.recording_secs = 0.0;
    st.segment_secs = 0.0;
    st.segment_bytes = 0;
    st.session_bytes = 0;
    st.segment_index = 0;
}

fn on_session_event(app: &AppHandle, e: SessionEvent) {
    let state = app.state::<AppState>();
    match e {
        SessionEvent::Armed(v) => state.update_status(app, |st| {
            st.phase = Phase::Armed;
            st.error = None;
            st.encoder = v.encoder;
            st.resolution = format!("{}x{} @ {} fps", v.width, v.height, v.fps);
            st.source = v.source;
            st.notes = v.notes;
            st.capture_target = CaptureTarget {
                kind: v.capture_kind.into(),
                exe: v.capture_exe,
                title: v.capture_title,
            };
        }),
        SessionEvent::Recovering(msg) => state.update_status(app, |st| {
            if st.phase != Phase::Recovering {
                st.recoveries += 1;
            }
            st.phase = Phase::Recovering;
            st.error = Some(msg);
            st.buffered_secs = 0.0;
            st.buffered_mb = 0.0;
            reset_recording_status(st);
        }),
        SessionEvent::Status(s) => state.update_status(app, |st| {
            st.buffered_secs = s.buffered_secs;
            st.buffered_mb = s.buffered_bytes as f64 / 1e6;
            st.buffer_estimate_mb = s.buffer_estimate_mb;
            st.recording = s.recording;
            st.recording_paused = s.recording_paused;
            st.waiting_for_source = s.waiting_for_source;
            st.recording_secs = s.recording_secs;
            st.segment_secs = s.segment_secs;
            st.segment_bytes = s.segment_bytes;
            st.session_bytes = s.session_bytes;
            st.segment_index = s.segment_index;
            st.frames = s.frames;
            st.dropped = s.dropped;
            st.source = s.source;
            st.notes = s.notes;
            st.capture_target = CaptureTarget {
                kind: s.capture_kind.into(),
                exe: s.capture_exe,
                title: s.capture_title,
            };
        }),
        SessionEvent::ClipSaved(clip) => on_clip_saved(app, &state, clip, SaveKind::ReplayClip),
        SessionEvent::SaveFailed(msg) => {
            emit_badge_flash(
                app,
                BadgeFlash {
                    kind: "save_failed",
                    label: "SAVE FAILED",
                    secs: None,
                    tone: "err",
                    duration_ms: 3500,
                },
            );
            let _ = app.emit("error", &msg);
            feedback::notify(app, "Clip not saved", &msg);
        }
        SessionEvent::RecordingStarted => {
            state.update_status(app, |st| {
                st.recording = true;
                st.recording_paused = false;
                st.waiting_for_source = false;
            });
            emit_badge_flash(
                app,
                BadgeFlash {
                    kind: "recording_started",
                    label: "RECORDING STARTED",
                    secs: None,
                    tone: "rec",
                    duration_ms: 2200,
                },
            );
            if state.settings().save_sound {
                feedback::play_chime();
            }
        }
        SessionEvent::RecordingPaused(paused) => {
            state.update_status(app, |st| {
                st.recording_paused = paused;
            });
            emit_badge_flash(
                app,
                if paused {
                    BadgeFlash {
                        kind: "recording_paused",
                        label: "RECORDING PAUSED",
                        secs: None,
                        tone: "paused",
                        duration_ms: 2500,
                    }
                } else {
                    BadgeFlash {
                        kind: "recording_resumed",
                        label: "RECORDING RESUMED",
                        secs: None,
                        tone: "rec",
                        duration_ms: 2200,
                    }
                },
            );
        }
        SessionEvent::SegmentSaved(clip) => on_clip_saved(app, &state, clip, SaveKind::SegmentSplit),
        SessionEvent::ScreenshotTaken(path) => {
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            let announced = crate::badge::announces_clips(app);
            emit_badge_flash(
                app,
                BadgeFlash {
                    kind: "screenshot_saved",
                    label: "SCREENSHOT SAVED",
                    secs: None,
                    tone: "ok",
                    duration_ms: 3500,
                },
            );
            let _ = app.emit("screenshot_flash", ());
            feedback::screenshot_saved(app, &state.settings(), &name, !announced);
        }
        SessionEvent::ScreenshotSaved(path) => {
            let _ = app.emit("clip_saved", path.to_string_lossy());
            enforce_quota(app, &state);
        }
        SessionEvent::ScreenshotFailed(msg) => {
            let _ = app.emit("error", &msg);
            feedback::notify(app, "Screenshot failed", &msg);
        }
        SessionEvent::RecordingSaved(clip) => on_clip_saved(app, &state, clip, SaveKind::RecordingFinal),
        SessionEvent::RecordingStoppedReason(reason) => {
            emit_badge_flash(
                app,
                BadgeFlash {
                    kind: "recording_stopped",
                    label: "RECORDING STOPPED",
                    secs: None,
                    tone: "paused",
                    duration_ms: 3500,
                },
            );
            state.update_status(app, reset_recording_status);
            feedback::notify(app, "Continuous recording stopped", &reason);
        }
        SessionEvent::RecordingFailed(msg) => {
            emit_badge_flash(
                app,
                BadgeFlash {
                    kind: "recording_failed",
                    label: "RECORDING FAILED",
                    secs: None,
                    tone: "err",
                    duration_ms: 3500,
                },
            );
            state.update_status(app, reset_recording_status);
            let _ = app.emit("error", &msg);
            feedback::notify(app, "Recording failed", &msg);
        }
        SessionEvent::Stopped(err) => state.update_status(app, |st| {
            reset_recording_status(st);
            st.capture_target = CaptureTarget::default();
            match err {
                Some(msg) => {
                    st.phase = Phase::Error;
                    st.error = Some(msg.clone());
                    let _ = app.emit("error", &msg);
                }
                None => st.phase = Phase::Idle,
            }
        }),
    }
}
