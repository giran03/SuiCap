//! Video presets and the bitrate table (plan §4.3), and the persisted user settings
//! (plan §6.3: serde, validated on load).

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Codec {
    Av1,
    Hevc,
    H264,
}

impl Codec {
    /// Probe order (plan §4.3).
    pub const PREFERENCE: [Codec; 3] = [Codec::Av1, Codec::Hevc, Codec::H264];

    pub fn name(self) -> &'static str {
        match self {
            Codec::Av1 => "av1",
            Codec::Hevc => "hevc",
            Codec::H264 => "h264",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VideoPreset {
    /// 0 x 0 means "the captured monitor's size" (resolved when the path opens).
    pub width: u32,
    pub height: u32,
    pub fps: u32,
}

impl VideoPreset {
    pub const P1080_60: Self = Self {
        width: 1920,
        height: 1080,
        fps: 60,
    };

    /// H.264 is only a candidate up to 1440p120 (plan §4.3).
    pub fn allows_h264(&self) -> bool {
        u64::from(self.width) * u64::from(self.height) * u64::from(self.fps) <= 2560 * 1440 * 120
    }

    /// Starting max bitrate in Mbps, interpolated by pixel rate from the HEVC table,
    /// then scaled per codec (AV1 x0.8, H.264 x1.5).
    pub fn max_bitrate_mbps(&self, codec: Codec) -> u32 {
        // (pixels per second, HEVC Mbps)
        const TABLE: [(f64, f64); 5] = [
            (1920.0 * 1080.0 * 60.0, 20.0),
            (2560.0 * 1440.0 * 60.0, 30.0),
            (2560.0 * 1440.0 * 120.0, 45.0),
            (3840.0 * 2160.0 * 60.0, 50.0),
            (3840.0 * 2160.0 * 120.0, 80.0),
        ];
        let rate = f64::from(self.width) * f64::from(self.height) * f64::from(self.fps);
        let hevc = match TABLE.iter().position(|&(r, _)| rate <= r) {
            Some(0) => TABLE[0].1 * rate / TABLE[0].0,
            Some(i) => {
                let ((r0, b0), (r1, b1)) = (TABLE[i - 1], TABLE[i]);
                b0 + (b1 - b0) * (rate - r0) / (r1 - r0)
            }
            None => TABLE[4].1 * rate / TABLE[4].0,
        };
        let factor = match codec {
            Codec::Av1 => 0.8,
            Codec::Hevc => 1.0,
            Codec::H264 => 1.5,
        };
        (hevc * factor).round().max(1.0) as u32
    }
}

/// Replay-buffer RAM estimate in MB: Mbps x seconds / 8 (plan §4.3).
pub fn buffer_ram_estimate_mb(mbps: u32, secs: u32) -> u64 {
    u64::from(mbps) * u64::from(secs) / 8
}

// ---------------------------------------------------------------------------
// Persisted settings (plan §6.3). Every field has a default, so older or partial
// files load; anything that fails validation falls back to the defaults.

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("{0}")]
    Invalid(String),
    #[error("settings file: {0}")]
    Io(#[from] std::io::Error),
    #[error("settings file: {0}")]
    Json(#[from] serde_json::Error),
}

fn invalid<T>(msg: impl Into<String>) -> Result<T, ConfigError> {
    Err(ConfigError::Invalid(msg.into()))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum PresetName {
    /// H.264 1080p60: plays everywhere.
    Compat,
    /// Best available codec at 1080p60.
    #[default]
    Balanced,
    /// Best available codec at the monitor's own resolution, higher quality.
    High,
    Custom,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Container {
    Mp4,
    /// The default: it names every audio track (Mix, Game, Voice chat...) in players.
    #[default]
    Mkv,
}

impl Container {
    pub fn ext(self) -> &'static str {
        match self {
            Container::Mp4 => "mp4",
            Container::Mkv => "mkv",
        }
    }
}

/// The glyph at the start of the recording badge.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum BadgeIcon {
    #[default]
    Dot,
    Ring,
    Square,
    Play,
    Camera,
    Bolt,
    /// An image the user chose (`badge-icon.bin` next to the settings).
    Custom,
    None,
}

/// Which corner of the recorded monitor the recording badge sits in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum BadgeCorner {
    TopLeft,
    TopRight,
    BottomLeft,
    #[default]
    BottomRight,
}

/// How the app treats new releases.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum UpdateMode {
    /// Never check by itself.
    Off,
    /// Check and say so; the user chooses when to install.
    #[default]
    Notify,
    /// Download in the background and install while the PC is idle.
    Auto,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Theme {
    #[default]
    System,
    Light,
    Dark,
}

/// The video settings a preset resolves to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CustomVideo {
    /// 0 x 0 = the monitor's own size.
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    /// `None` = probe AV1 -> HEVC -> H.264.
    pub codec: Option<Codec>,
    /// Constant-quality level (lower is better), bounded by the max bitrate.
    pub quality: u8,
}

impl Default for CustomVideo {
    fn default() -> Self {
        Self {
            width: 1920,
            height: 1080,
            fps: 60,
            codec: None,
            quality: 23,
        }
    }
}

/// Default voice and media apps (plan §5).
pub const VOICE_APPS: [&str; 3] = ["discord.exe", "ts3client_win64.exe", "teamspeak.exe"];
pub const MEDIA_APPS: [&str; 6] = [
    "spotify.exe",
    "chrome.exe",
    "msedge.exe",
    "firefox.exe",
    "brave.exe",
    "opera.exe",
];
const MAX_APPS: usize = 32;
/// Parked apps are not recorded, so the list may hold more than a track does.
const MAX_IGNORED: usize = 64;

/// Which tracks are recorded. Game, Voice and Media off means **mute**: their apps are not
/// captured at all, so they are not in the Mix either. Mix off only drops the combined track.
/// (The Mic track follows `AudioSettings::mic`.)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AudioTracks {
    pub mix: bool,
    pub game: bool,
    pub voice: bool,
    pub media: bool,
}

impl Default for AudioTracks {
    fn default() -> Self {
        Self {
            mix: true,
            game: true,
            voice: true,
            media: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AudioSettings {
    pub enabled: bool,
    /// Record the microphone (and write the Mic track).
    pub mic: bool,
    /// Capture endpoint id of the chosen microphone; `None` = the system default input.
    pub mic_device: Option<String>,
    pub tracks: AudioTracks,
    /// Apps that are never recorded: they win over every track list.
    pub ignored: Vec<String>,
    /// Empty = the fullscreen app in the foreground when capture starts.
    pub game: Vec<String>,
    pub voice: Vec<String>,
    pub media: Vec<String>,
}

impl Default for AudioSettings {
    fn default() -> Self {
        let own = |a: &[&str]| a.iter().map(|s| (*s).to_owned()).collect();
        Self {
            enabled: true,
            mic: true,
            mic_device: None,
            tracks: AudioTracks::default(),
            ignored: Vec::new(),
            game: Vec::new(),
            voice: own(&VOICE_APPS),
            media: own(&MEDIA_APPS),
        }
    }
}

/// Global shortcuts as accelerator strings ("Ctrl+Alt+F10"); empty = off.
/// The shell parses them; here only shape and duplicates are checked.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Hotkeys {
    pub save_15: String,
    pub save_30: String,
    pub save_60: String,
    pub save_full: String,
    /// Save a PNG of the screen.
    pub screenshot: String,
    /// Switch between recording the app in front and the whole display.
    pub target_app: String,
}

impl Default for Hotkeys {
    fn default() -> Self {
        Self {
            save_15: "Ctrl+Alt+F9".into(),
            save_30: "Ctrl+Alt+F10".into(),
            save_60: "Ctrl+Alt+F11".into(),
            save_full: "Ctrl+Alt+F12".into(),
            screenshot: "Ctrl+Alt+F8".into(),
            target_app: "Ctrl+Alt+F7".into(),
        }
    }
}

impl Hotkeys {
    /// Every accelerator, for validation.
    pub fn all(&self) -> [&str; 6] {
        [
            &self.save_15,
            &self.save_30,
            &self.save_60,
            &self.save_full,
            &self.screenshot,
            &self.target_app,
        ]
    }

    /// `(seconds to save, accelerator)`; `None` seconds = the whole buffer.
    pub fn bindings(&self) -> [(Option<u32>, &str); 4] {
        [
            (Some(15), self.save_15.as_str()),
            (Some(30), self.save_30.as_str()),
            (Some(60), self.save_60.as_str()),
            (None, self.save_full.as_str()),
        ]
    }
}

/// What video capture records.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SourceKind {
    /// A whole monitor.
    #[default]
    Display,
    /// One application's window, chosen by program name.
    App,
    /// The full-screen game in front, else the display.
    Auto,
}

/// The video source: a display, one application, or automatic.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SourceSettings {
    pub kind: SourceKind,
    /// Program of the chosen app, e.g. `game.exe` (kind `App`).
    pub exe: String,
    /// Only the window whose title contains this; empty = any window of the app.
    pub title: String,
    /// While the app is not there, record the display instead of holding the last frame.
    pub fallback: bool,
}

impl Default for SourceSettings {
    fn default() -> Self {
        Self {
            kind: SourceKind::Display,
            exe: String::new(),
            title: String::new(),
            fallback: true,
        }
    }
}

pub const MAX_SOURCE_TITLE: usize = 200;

pub const MAX_QUOTA_GB: u32 = 100_000;
pub const BADGE_SCALE: std::ops::RangeInclusive<u32> = 60..=250;
pub const BADGE_OPACITY: std::ops::RangeInclusive<u32> = 20..=100;
pub const MAX_BADGE_MARGIN: u32 = 400;
pub const MAX_BADGE_LABEL: usize = 16;

/// `#rrggbb`, nothing else (the value ends up in CSS).
pub fn is_hex_color(c: &str) -> bool {
    c.len() == 7 && c.starts_with('#') && c[1..].bytes().all(|b| b.is_ascii_hexdigit())
}
pub const BUFFER_SECS: std::ops::RangeInclusive<u32> = 10..=600;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub preset: PresetName,
    /// Used when `preset` is `Custom`.
    pub custom: CustomVideo,
    pub buffer_secs: u32,
    pub container: Container,
    /// `None` = the user's Videos folder + `SuiCap` (resolved by the shell).
    pub clips_dir: Option<PathBuf>,
    pub cursor: bool,
    /// `\\.\DISPLAY<n>`; `None` = primary monitor.
    pub display: Option<u32>,
    /// Display, one application or automatic.
    pub source: SourceSettings,
    pub audio: AudioSettings,
    pub hotkeys: Hotkeys,
    pub save_sound: bool,
    pub save_toast: bool,
    pub autostart: bool,
    /// Start filling the replay buffer as soon as the app starts.
    pub arm_on_start: bool,
    pub theme: Theme,
    pub update_mode: UpdateMode,
    /// Delete the oldest clips once the library passes this many GB; 0 = never.
    pub clips_quota_gb: u32,
    /// Show a badge on the recorded monitor while recording (never captured).
    pub recording_badge: bool,
    pub badge_corner: BadgeCorner,
    /// Also show it while the replay buffer runs, not only during a recording.
    pub badge_during_replay: bool,
    /// Badge size in percent of the default.
    pub badge_scale: u32,
    /// Background opacity in percent.
    pub badge_opacity: u32,
    /// `#rrggbb` colours: background, text, the recording dot, the replay dot.
    pub badge_bg: String,
    pub badge_text: String,
    pub badge_rec_color: String,
    pub badge_replay_color: String,
    /// Distance from the monitor edge, in logical pixels.
    pub badge_margin: u32,
    pub badge_show_time: bool,
    pub badge_show_label: bool,
    /// Briefly show "Clip saved · N s" on the badge when a clip is saved, label or not.
    pub badge_clip_flash: bool,
    /// Text while recording and while only replay is on (at most `MAX_BADGE_LABEL` characters).
    pub badge_label_rec: String,
    pub badge_label_replay: String,
    pub badge_icon: BadgeIcon,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            preset: PresetName::default(),
            custom: CustomVideo::default(),
            buffer_secs: 120,
            container: Container::default(),
            clips_dir: None,
            cursor: true,
            display: None,
            source: SourceSettings::default(),
            audio: AudioSettings::default(),
            hotkeys: Hotkeys::default(),
            save_sound: true,
            save_toast: true,
            autostart: false,
            arm_on_start: true,
            theme: Theme::default(),
            update_mode: UpdateMode::default(),
            clips_quota_gb: 0,
            recording_badge: true,
            badge_corner: BadgeCorner::default(),
            badge_during_replay: true,
            badge_scale: 100,
            badge_opacity: 88,
            badge_bg: "#121317".into(),
            badge_text: "#f2f2f4".into(),
            badge_rec_color: "#ef4444".into(),
            badge_replay_color: "#3ba7ff".into(),
            badge_margin: 16,
            badge_show_time: true,
            badge_show_label: true,
            badge_clip_flash: true,
            badge_label_rec: "REC".into(),
            badge_label_replay: "REPLAY".into(),
            badge_icon: BadgeIcon::default(),
        }
    }
}

fn check_app(kind: &str, name: &str) -> Result<(), ConfigError> {
    let ok = (5..=64).contains(&name.len())
        && name.to_ascii_lowercase().ends_with(".exe")
        && !name.chars().any(|c| c.is_control() || r#"<>:"/\|?*"#.contains(c));
    if ok {
        Ok(())
    } else {
        invalid(format!("{kind} app \"{name}\" must be a file name like game.exe"))
    }
}

impl Settings {
    /// The video settings the chosen preset stands for.
    pub fn video(&self) -> CustomVideo {
        match self.preset {
            PresetName::Compat => CustomVideo {
                codec: Some(Codec::H264),
                ..CustomVideo::default()
            },
            PresetName::Balanced => CustomVideo::default(),
            PresetName::High => CustomVideo {
                width: 0,
                height: 0,
                quality: 20,
                ..CustomVideo::default()
            },
            PresetName::Custom => self.custom,
        }
    }

    pub fn validate(&self) -> Result<(), ConfigError> {
        let v = self.custom;
        let source = v.width == 0 && v.height == 0;
        let dim = |d: u32| (320..=7680).contains(&d) && d.is_multiple_of(2);
        if !(source || dim(v.width) && dim(v.height)) {
            return invalid("custom resolution must be even, 320 to 7680 pixels (or 0 x 0 for the monitor size)");
        }
        let src = &self.source;
        if src.kind == SourceKind::App
            && !(src.exe.len() >= 5
                && src.exe.len() <= 64
                && src.exe.to_ascii_lowercase().ends_with(".exe")
                && !src.exe.chars().any(|c| c.is_control() || r#"<>:"/\|?*"#.contains(c)))
        {
            return invalid("the application to record must be a program name like game.exe");
        }
        if src.title.chars().count() > MAX_SOURCE_TITLE || src.title.chars().any(char::is_control) {
            return invalid("the window title is not valid");
        }
        if !(10..=240).contains(&v.fps) {
            return invalid("frame rate must be 10 to 240");
        }
        if !(1..=51).contains(&v.quality) {
            return invalid("quality must be 1 to 51");
        }
        if !BUFFER_SECS.contains(&self.buffer_secs) {
            return invalid(format!(
                "replay buffer must be {} to {} seconds",
                BUFFER_SECS.start(),
                BUFFER_SECS.end()
            ));
        }
        if let Some(d) = &self.clips_dir {
            use std::path::{Component, Prefix};
            let is_local_disk = matches!(
                d.components().next(),
                Some(Component::Prefix(p)) if matches!(p.kind(), Prefix::Disk(_) | Prefix::VerbatimDisk(_))
            );
            let has_parent = d.components().any(|c| matches!(c, Component::ParentDir));
            if !d.is_absolute() || !is_local_disk || has_parent || d.as_os_str().len() > 200 {
                return invalid("clips folder must be a local drive path under 200 characters");
            }
        }
        if self.display.is_some_and(|n| !(1..=32).contains(&n)) {
            return invalid("display number must be 1 to 32");
        }
        for (kind, list) in [
            ("game", &self.audio.game),
            ("voice", &self.audio.voice),
            ("media", &self.audio.media),
        ] {
            if list.len() > MAX_APPS {
                return invalid(format!("at most {MAX_APPS} {kind} apps"));
            }
            for name in list {
                check_app(kind, name)?;
            }
        }
        if let Some(id) = &self.audio.mic_device
            && (id.is_empty() || id.len() > 200 || id.chars().any(char::is_control))
        {
            return invalid("the microphone id is not valid");
        }
        if self.audio.ignored.len() > MAX_IGNORED {
            return invalid(format!("at most {MAX_IGNORED} apps that are not recorded"));
        }
        for name in &self.audio.ignored {
            check_app("ignored", name)?;
        }
        if self.clips_quota_gb > MAX_QUOTA_GB {
            return invalid(format!("the clip storage limit is at most {MAX_QUOTA_GB} GB"));
        }
        if !BADGE_SCALE.contains(&self.badge_scale) || !BADGE_OPACITY.contains(&self.badge_opacity) {
            return invalid("the badge size or opacity is out of range");
        }
        if self.badge_margin > MAX_BADGE_MARGIN {
            return invalid(format!("the badge margin is at most {MAX_BADGE_MARGIN} px"));
        }
        for c in [
            &self.badge_bg,
            &self.badge_text,
            &self.badge_rec_color,
            &self.badge_replay_color,
        ] {
            if !is_hex_color(c) {
                return invalid(format!("\"{c}\" is not a colour like #3ba7ff"));
            }
        }
        for label in [&self.badge_label_rec, &self.badge_label_replay] {
            if label.chars().count() > MAX_BADGE_LABEL || label.chars().any(char::is_control) {
                return invalid(format!(
                    "a badge label is at most {MAX_BADGE_LABEL} characters, on one line"
                ));
            }
        }
        let mut seen: Vec<String> = Vec::new();
        for key in self.hotkeys.all() {
            if key.is_empty() {
                continue;
            }
            if key.len() > 40 || !key.chars().all(|c| c.is_ascii_graphic()) {
                return invalid(format!("shortcut \"{key}\" is not valid"));
            }
            let norm = key.to_ascii_lowercase();
            if seen.contains(&norm) {
                return invalid(format!("shortcut {key} is used twice"));
            }
            seen.push(norm);
        }
        Ok(())
    }

    /// Load and validate. A missing file gives the defaults; a broken or invalid one
    /// is kept as `<name>.bad`, the defaults are used and the reason is returned.
    pub fn load(path: &Path) -> (Settings, Option<String>) {
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return (Settings::default(), None),
            Err(e) => return (Settings::default(), Some(format!("could not read settings: {e}"))),
        };
        let parsed = serde_json::from_str::<Settings>(&text)
            .map_err(ConfigError::from)
            .and_then(|s| s.validate().map(|()| s));
        match parsed {
            Ok(s) => (s, None),
            Err(e) => {
                let _ = std::fs::rename(path, path.with_extension("json.bad"));
                (
                    Settings::default(),
                    Some(format!("settings were reset to defaults ({e})")),
                )
            }
        }
    }

    /// Validate, then write atomically (temp file + rename).
    pub fn save(&self, path: &Path) -> Result<(), ConfigError> {
        self.validate()?;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(self)?)?;
        if let Err(e) = crate::mux::replace_file(&tmp, path) {
            let _ = std::fs::remove_file(&tmp);
            return Err(e.into());
        }
        Ok(())
    }
}

/// `%APPDATA%\SuiCap\settings.json`.
pub fn settings_path() -> Option<PathBuf> {
    std::env::var_os("APPDATA").map(|d| PathBuf::from(d).join("SuiCap").join("settings.json"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bitrate_table_points() {
        let p = |w, h, fps| VideoPreset {
            width: w,
            height: h,
            fps,
        };
        assert_eq!(p(1920, 1080, 60).max_bitrate_mbps(Codec::Hevc), 20);
        assert_eq!(p(2560, 1440, 120).max_bitrate_mbps(Codec::Hevc), 45);
        assert_eq!(p(3840, 2160, 120).max_bitrate_mbps(Codec::Hevc), 80);
        assert_eq!(p(3840, 2160, 120).max_bitrate_mbps(Codec::Av1), 64);
        assert_eq!(p(1920, 1080, 60).max_bitrate_mbps(Codec::H264), 30);
    }

    #[test]
    fn h264_gate() {
        assert!(
            VideoPreset {
                width: 2560,
                height: 1440,
                fps: 120
            }
            .allows_h264()
        );
        assert!(
            !VideoPreset {
                width: 3840,
                height: 2160,
                fps: 60
            }
            .allows_h264()
        );
    }

    #[test]
    fn ram_estimate() {
        assert_eq!(buffer_ram_estimate_mb(80, 120), 1200);
    }

    #[test]
    fn settings_defaults_valid_and_round_trip() {
        let s = Settings::default();
        s.validate().unwrap();
        let text = serde_json::to_string(&s).unwrap();
        assert_eq!(serde_json::from_str::<Settings>(&text).unwrap(), s);
    }

    #[test]
    fn settings_partial_file_fills_defaults() {
        let s: Settings = serde_json::from_str(r#"{"preset":"high","buffer_secs":60,"unknown":1}"#).unwrap();
        assert_eq!(s.preset, PresetName::High);
        assert_eq!(s.buffer_secs, 60);
        assert_eq!(s.hotkeys, Hotkeys::default());
        assert_eq!(s.container, Container::Mkv);
        assert_eq!(s.video().width, 0);
    }

    #[test]
    fn settings_validation_rejects_bad_values() {
        let bad = |f: fn(&mut Settings)| {
            let mut s = Settings::default();
            f(&mut s);
            s.validate().is_err()
        };
        assert!(bad(|s| s.buffer_secs = 5));
        assert!(bad(|s| s.custom.width = 1921));
        assert!(bad(|s| s.custom.fps = 500));
        assert!(bad(|s| s.custom.quality = 0));
        assert!(bad(|s| s.clips_dir = Some("relative\\dir".into())));
        assert!(bad(|s| s.clips_dir = Some(r"\\attacker\share".into())));
        assert!(bad(|s| s.clips_dir = Some(r"C:\Clips\..\Windows".into())));
        assert!(bad(|s| s.audio.voice.push("..\\evil.exe".into())));
        assert!(bad(|s| s.audio.media.push("notepad".into())));
        assert!(bad(|s| s.audio.ignored.push("a/b.exe".into())));
        assert!(bad(|s| s.audio.mic_device = Some(String::new())));
        assert!(bad(|s| s.audio.mic_device = Some("x".repeat(201))));
        // a recorded application must be a plain program name
        assert!(bad(|s| (s.source.kind, s.source.exe) = (SourceKind::App, String::new())));
        assert!(bad(|s| (s.source.kind, s.source.exe) = (SourceKind::App, "a/b.exe".into())));
        assert!(bad(|s| (s.source.kind, s.source.exe) = (SourceKind::App, "game".into())));
        assert!(bad(|s| s.source.title = "x".repeat(201)));
        assert!(bad(
            |s| s.audio.ignored = (0..65).map(|i| format!("app{i}.exe")).collect()
        ));
        assert!(bad(|s| s.badge_bg = "red".into()));
        assert!(bad(|s| s.badge_text = "#12345".into()));
        assert!(bad(|s| s.badge_rec_color = "#12345g".into()));
        assert!(bad(|s| s.badge_rec_color = "#fff; x: y".into()));
        assert!(bad(|s| s.badge_scale = 10));
        assert!(bad(|s| s.badge_opacity = 5));
        assert!(bad(|s| s.badge_margin = 9999));
        assert!(bad(|s| s.badge_label_rec = "x".repeat(17)));
        assert!(bad(|s| s.badge_label_replay = "a\nb".into()));
        let ok = Settings {
            badge_label_rec: "Mação ●".into(), // any text, including empty or non-ASCII
            badge_label_replay: String::new(),
            ..Settings::default()
        };
        ok.validate().unwrap();
        assert!(bad(|s| s.hotkeys.save_15 = "ctrl+alt+f10".into())); // duplicate of save_30
        let mut s = Settings::default();
        s.custom.width = 0;
        s.custom.height = 0;
        s.hotkeys.save_full.clear();
        s.validate().unwrap();
    }

    #[test]
    fn settings_load_resets_broken_file() {
        let dir = std::env::temp_dir().join(format!("suicap-cfg-{}", std::process::id()));
        let path = dir.join("settings.json");
        let s = Settings {
            buffer_secs: 300,
            ..Default::default()
        };
        Settings::default().save(&path).unwrap();
        s.save(&path).unwrap(); // over an existing file
        assert_eq!(Settings::load(&path), (s, None));
        std::fs::write(&path, "{ not json").unwrap();
        let (loaded, warning) = Settings::load(&path);
        assert_eq!(loaded, Settings::default());
        assert!(warning.is_some());
        assert!(path.with_extension("json.bad").exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
