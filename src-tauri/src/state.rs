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
    config::Settings,
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
    pub encoder: String,
    pub resolution: String,
    pub source: String,
    pub capture_target: CaptureTarget,
    pub buffer_secs: u32,
    pub buffered_secs: f64,
    pub buffered_mb: f64,
    pub buffer_estimate_mb: u64,
    pub recording: bool,
    pub recording_secs: f64,
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
    ) -> Self {
        Self {
            status: Mutex::new(Status {
                settings_warning: warning,
                buffer_secs: settings.buffer_secs,
                ..Default::default()
            }),
            settings: Mutex::new(settings),
            session: Mutex::new(None),
            live: Mutex::new(None),
            settings_path,
            default_clips_dir,
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

    pub fn clips_dir(&self) -> PathBuf {
        lock(&self.settings)
            .clips_dir
            .clone()
            .unwrap_or_else(|| self.default_clips_dir.clone())
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
            buffer_secs: s.buffer_secs,
            video: VideoOptions::from_settings(&s),
            audio,
            clips_dir: self.clips_dir(),
            container: s.container,
        };
        self.update_status(app, |st| {
            *st = Status {
                phase: Phase::Starting,
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
        *lock(&self.live) = None;
        self.update_status(app, |st| {
            st.audio_live = AudioLive::default();
            if st.phase != Phase::Error {
                st.phase = Phase::Idle;
            }
            st.buffered_secs = 0.0;
            st.buffered_mb = 0.0;
            st.recording = false;
            st.recording_secs = 0.0;
        });
    }

    /// Save the last `secs` seconds (`None` = whole buffer).
    pub fn save(&self, app: &AppHandle, secs: Option<u32>) {
        let queued = lock(&self.session)
            .as_ref()
            .filter(|s| s.is_running())
            .map(|s| s.save(secs));
        match queued {
            Some(true) => {}
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
            Some(true) => {}
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
                feedback::notify(app, "Replay buffer is off", "Arm SuiCap from the tray to record.");
                Err("Replay buffer is off".into())
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
            None => Err("Replay buffer is off".into()),
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

fn on_clip_saved(app: &AppHandle, state: &AppState, clip: ClipInfo, stop_recording: bool) {
    let secs = (clip.last_pts_us - clip.first_pts_us).max(0) as f64 / 1e6;
    let name = clip
        .path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    state.update_status(app, |st| {
        if stop_recording {
            st.recording = false;
            st.recording_secs = 0.0;
        }
        st.last_clip = Some(clip.path.to_string_lossy().into_owned());
    });
    let _ = app.emit("clip_saved", clip.path.to_string_lossy());
    let _ = app.emit("clip_flash", secs);
    let announced = crate::badge::announces_clips(app);
    feedback::clip_saved(app, &state.settings(), &name, secs, !announced);
    enforce_quota(app, state);
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
            st.recording = false;
            st.recording_secs = 0.0;
        }),
        SessionEvent::Status(s) => state.update_status(app, |st| {
            st.buffered_secs = s.buffered_secs;
            st.buffered_mb = s.buffered_bytes as f64 / 1e6;
            st.buffer_estimate_mb = s.buffer_estimate_mb;
            st.recording = s.recording;
            st.recording_secs = s.recording_secs;
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
        SessionEvent::ClipSaved(clip) => on_clip_saved(app, &state, clip, false),
        SessionEvent::SaveFailed(msg) => {
            let _ = app.emit("error", &msg);
            feedback::notify(app, "Clip not saved", &msg);
        }
        SessionEvent::RecordingStarted => {
            state.update_status(app, |st| {
                st.recording = true;
                st.recording_secs = 0.0;
            });
            if state.settings().save_sound {
                feedback::play_chime();
            }
        }
        SessionEvent::ScreenshotTaken(path) => {
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            let _ = app.emit("screenshot_flash", ());
            let announced = crate::badge::announces_clips(app);
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
        SessionEvent::RecordingSaved(clip) => on_clip_saved(app, &state, clip, true),
        SessionEvent::RecordingFailed(msg) => {
            state.update_status(app, |st| {
                st.recording = false;
                st.recording_secs = 0.0;
            });
            let _ = app.emit("error", &msg);
            feedback::notify(app, "Recording failed", &msg);
        }
        SessionEvent::Stopped(err) => state.update_status(app, |st| {
            st.recording = false;
            st.recording_secs = 0.0;
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
