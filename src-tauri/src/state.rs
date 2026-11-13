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

/// Everything the settings window and tray menu display.
#[derive(Debug, Clone, Serialize, Default)]
pub struct Status {
    pub phase: Phase,
    pub encoder: String,
    pub resolution: String,
    pub source: String,
    pub buffer_secs: u32,
    pub buffered_secs: f64,
    pub buffered_mb: f64,
    pub buffer_estimate_mb: u64,
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
}

pub struct AppState {
    settings: Mutex<Settings>,
    session: Mutex<Option<Session>>,
    status: Mutex<Status>,
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
        let _ = app.emit("status", &snapshot);
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
        let opts = SessionOptions {
            buffer_secs: s.buffer_secs,
            video: VideoOptions::from_settings(&s),
            audio: s.audio.enabled.then(|| AudioOptions::from_settings(&s.audio, codec)),
            clips_dir: self.clips_dir(),
            container: s.container,
        };
        self.update_status(app, |st| {
            *st = Status {
                phase: Phase::Starting,
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
        self.update_status(app, |st| {
            if st.phase != Phase::Error {
                st.phase = Phase::Idle;
            }
            st.buffered_secs = 0.0;
            st.buffered_mb = 0.0;
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
        }),
        SessionEvent::Recovering(msg) => state.update_status(app, |st| {
            if st.phase != Phase::Recovering {
                st.recoveries += 1;
            }
            st.phase = Phase::Recovering;
            st.error = Some(msg);
            st.buffered_secs = 0.0;
            st.buffered_mb = 0.0;
        }),
        SessionEvent::Status(s) => state.update_status(app, |st| {
            st.buffered_secs = s.buffered_secs;
            st.buffered_mb = s.buffered_bytes as f64 / 1e6;
            st.buffer_estimate_mb = s.buffer_estimate_mb;
            st.frames = s.frames;
            st.dropped = s.dropped;
        }),
        SessionEvent::ClipSaved(clip) => {
            let secs = (clip.last_pts_us - clip.first_pts_us).max(0) as f64 / 1e6;
            let name = clip
                .path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            state.update_status(app, |st| st.last_clip = Some(clip.path.to_string_lossy().into_owned()));
            let _ = app.emit("clip_saved", clip.path.to_string_lossy());
            feedback::clip_saved(app, &state.settings(), &name, secs);
        }
        SessionEvent::SaveFailed(msg) => {
            let _ = app.emit("error", &msg);
            feedback::notify(app, "Clip not saved", &msg);
        }
        SessionEvent::Stopped(err) => state.update_status(app, |st| match err {
            Some(msg) => {
                st.phase = Phase::Error;
                st.error = Some(msg.clone());
                let _ = app.emit("error", &msg);
            }
            None => st.phase = Phase::Idle,
        }),
    }
}
