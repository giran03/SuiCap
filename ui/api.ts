// Typed wrappers for the shell's commands and events (src-tauri/src/commands.rs).
import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";

export type Codec = "av1" | "hevc" | "h264";
export type PresetName = "compat" | "balanced" | "high" | "custom";
export type Theme = "system" | "light" | "dark";
export type UpdateMode = "off" | "notify" | "auto";
export type BadgeIcon = "dot" | "ring" | "square" | "play" | "camera" | "bolt" | "custom" | "none";
export type BadgeCorner = "top-left" | "top-right" | "bottom-left" | "bottom-right";

/** What the video records: the whole display, one application's window, or the full-screen game. */
export interface SourceSettings {
  kind: "display" | "app" | "auto";
  /** Program of the chosen application, e.g. "game.exe"; empty for none. */
  exe: string;
  /** Empty = any window of the app, else part of the window title it must contain. */
  title: string;
  /** Record the display while the app or window is missing. */
  fallback: boolean;
}

/** A window that can be captured. */
export interface WindowInfo {
  pid: number;
  exe: string;
  title: string;
  class: string;
  width: number;
  height: number;
}

export type RecordingMode = "replay" | "continuous";

export interface ContinuousSettings {
  segment_mins: number;
  segment_mb: number;
  max_mins: number;
  wait_for_source: boolean;
  split_on_app_change: boolean;
  idle_pause_mins: number;
  min_free_gb: number;
  keep_replay_buffer: boolean;
  notify_on_split: boolean;
}

export interface Settings {
  preset: PresetName;
  custom: { width: number; height: number; fps: number; codec: Codec | null; quality: number };
  mode: RecordingMode;
  continuous: ContinuousSettings;
  buffer_secs: number;
  container: "mp4" | "mkv";
  clips_dir: string | null;
  temp_dir: string | null;
  cursor: boolean;
  display: number | null;
  source: SourceSettings;
  audio: {
    enabled: boolean;
    mic: boolean;
    /** Endpoint id of the chosen microphone; null = the system default input. */
    mic_device: string | null;
    /** Game, voice and media off = muted (not captured, not in the Mix); mix off drops only the Mix. Mic follows `mic`. */
    tracks: { mix: boolean; game: boolean; voice: boolean; media: boolean };
    /** Apps that are never recorded; they win over every track list. */
    ignored: string[];
    game: string[];
    voice: string[];
    media: string[];
    /** Put an app that plays sound and is on no list onto a track by itself. */
    auto_add: boolean;
    /** Apps removed on purpose: never added automatically again. */
    dismissed: string[];
  };
  hotkeys: {
    save_15: string;
    save_30: string;
    save_60: string;
    save_full: string;
    screenshot: string;
    target_app: string;
    record_toggle: string;
    record_pause: string;
    arm_toggle: string;
  };
  save_sound: boolean;
  save_toast: boolean;
  autostart: boolean;
  arm_on_start: boolean;
  theme: Theme;
  update_mode: UpdateMode;
  /** Delete the oldest clips past this many GB; 0 = never. */
  clips_quota_gb: number;
  /** Badge on the recorded monitor while recording; never captured. */
  recording_badge: boolean;
  badge_corner: BadgeCorner;
  /** Also show the badge while replay is on, not only while recording. */
  badge_during_replay: boolean;
  /** Size in percent, background opacity in percent, #rrggbb colours, edge distance in px. */
  badge_scale: number;
  badge_opacity: number;
  badge_bg: string;
  badge_text: string;
  badge_rec_color: string;
  badge_replay_color: string;
  badge_margin: number;
  badge_show_time: boolean;
  badge_show_label: boolean;
  /** Briefly show "Clip saved" and its length on the badge when a clip is saved. */
  badge_clip_flash: boolean;
  /** Text while recording and while only replay is on (max 16 characters). */
  badge_label_rec: string;
  badge_label_replay: string;
  badge_icon: BadgeIcon;
}

export type Phase = "idle" | "starting" | "armed" | "recovering" | "error";

export interface Status {
  phase: Phase;
  mode: RecordingMode;
  encoder: string;
  resolution: string;
  source: string;
  buffer_secs: number;
  buffered_secs: number;
  buffered_mb: number;
  buffer_estimate_mb: number;
  recording: boolean;
  recording_paused: boolean;
  waiting_for_source: boolean;
  recording_secs: number;
  segment_secs: number;
  segment_bytes: number;
  session_bytes: number;
  segment_index: number;
  frames: number;
  dropped: number;
  error: string | null;
  notes: string[];
  recoveries: number;
  last_clip: string | null;
  hotkey_errors: string[];
  settings_warning: string | null;
  /** Audio tracks the running session has: those can be muted and unmuted live, with no restart. */
  /** What is being captured right now. */
  capture_target: { kind: "display" | "window" | "waiting"; exe: string | null; title: string | null };
  audio_live: { mix: boolean; mic: boolean; game: boolean; voice: boolean; media: boolean };
}

export interface AudioApp {
  exe: string;
  /** Full path of the running program. */
  path: string;
  /** The audio session's own name, else the exe name. */
  name: string;
  playing: boolean;
}

export interface InputDevice {
  id: string;
  name: string;
  /** The system default input right now. */
  is_default: boolean;
}

export interface Display {
  number: number;
  label: string;
  primary: boolean;
}

export type UpdateState =
  | { phase: "idle" | "checking" | "up_to_date" }
  | { phase: "available" | "downloading" | "ready" | "installing"; version: string }
  | { phase: "failed"; message: string; version: string | null };

export interface UpdateInfo {
  current: string;
  state: UpdateState;
  /** Unix seconds of the last finished check. */
  last_checked: number | null;
}

export interface AppInfo {
  version: string;
  developer: string;
  email: string;
}

export interface ClipEntry {
  path: string;
  /** The per-app folder name ("" when directly in the clips folder). */
  app: string;
  name: string;
  kind: "video" | "image";
  size: number;
  /** Unix seconds. */
  modified: number;
}

export interface CodecSupport {
  gpu: string;
  /** Codecs the GPU can encode, best first. */
  codecs: Codec[];
  problem: string | null;
}

export interface VideoPreview {
  frame: string | null;
  kind: "display" | "window" | "waiting";
  exe: string | null;
  title: string | null;
  source_width: number;
  source_height: number;
  output_width: number;
  output_height: number;
  fps: number;
  monitor_label: string;
  fallback_active: boolean;
  note: string | null;
}

let audioAppsInflight: Promise<AudioApp[]> | null = null;
let audioAppsCache: { at: number; data: AudioApp[] } | null = null;

function fetchAudioApps(): Promise<AudioApp[]> {
  if (audioAppsCache && Date.now() - audioAppsCache.at < 2000) {
    return Promise.resolve(audioAppsCache.data);
  }
  if (!audioAppsInflight) {
    audioAppsInflight = invoke<AudioApp[]>("audio_apps")
      .then((data) => {
        audioAppsCache = { at: Date.now(), data };
        return data;
      })
      .finally(() => {
        audioAppsInflight = null;
      });
  }
  return audioAppsInflight;
}

export const api = {
  status: () => invoke<Status>("status"),
  arm: () => invoke<void>("arm"),
  disarm: () => invoke<void>("disarm"),
  saveClip: (secs: number | null) => invoke<void>("save_clip", { secs }),
  recordStart: () => invoke<void>("record_start"),
  recordStop: () => invoke<void>("record_stop"),
  recordPause: () => invoke<void>("record_pause"),
  recordSplit: () => invoke<void>("record_split"),
  configGet: () => invoke<Settings>("config_get"),
  configSet: (settings: Settings) => invoke<Status>("config_set", { settings }),
  defaultClipsDir: () => invoke<string>("default_clips_dir"),
  defaultTempDir: () => invoke<string>("default_temp_dir"),
  displays: () => invoke<Display[]>("displays"),
  estimateMb: (settings: Settings) => invoke<number>("estimate_mb", { settings }),
  openClipsFolder: () => invoke<void>("open_clips_folder"),
  openTempFolder: () => invoke<void>("open_temp_folder"),
  videoPreview: (settings: Settings) => invoke<VideoPreview>("video_preview", { settings }),
  closeVideoPreview: () => invoke<void>("close_video_preview"),
  revealLastClip: () => invoke<void>("reveal_last_clip"),
  pickClipsDir: () => invoke<string | null>("pick_clips_dir"),
  pickTempDir: () => invoke<string | null>("pick_temp_dir"),
  foregroundGame: () => invoke<string | null>("foreground_game"),
  captureWindows: () => invoke<WindowInfo[]>("capture_windows"),
  audioApps: fetchAudioApps,
  presentApps: (exes: string[]) => invoke<string[]>("present_apps", { exes }),
  revealApp: (exe: string) => invoke<void>("reveal_app", { exe }),
  audioInputs: () => invoke<InputDevice[]>("audio_inputs"),
  appIcon: (exe: string) => invoke<string | null>("app_icon", { exe }),
  openSettings: () => invoke<void>("open_settings"),
  quit: () => invoke<void>("quit"),
  /** What the GPU behind the chosen monitor can encode (probed once, then cached). */
  codecSupport: (refresh = false) => invoke<CodecSupport>("codec_support", { refresh }),
  pickBadgeIcon: () => invoke<boolean>("pick_badge_icon"),
  removeBadgeIcon: () => invoke<void>("remove_badge_icon"),
  /** The chosen badge image as a data URL, or null. */
  badgeIconData: () => invoke<string | null>("badge_icon_data"),
  badgeResize: (width: number, height: number) => invoke<void>("badge_resize", { width, height }),
  screenshot: () => invoke<void>("screenshot"),
  clipsList: () => invoke<ClipEntry[]>("clips_list"),
  /** Base64 PNG, or null when Windows has no thumbnail for the file. */
  clipThumbnail: (path: string) => invoke<string | null>("clip_thumbnail", { path }),
  clipDuration: (path: string) => invoke<number | null>("clip_duration", { path }),
  /** Lossless copy of [start, end] seconds as a new file; returns its path. */
  clipTrim: (path: string, start: number, end: number) => invoke<string>("clip_trim", { path, start, end }),
  /** To the Recycle Bin, or for good when `permanent`. */
  clipDelete: (path: string, permanent: boolean) => invoke<void>("clip_delete", { path, permanent }),
  clipOpen: (path: string) => invoke<void>("clip_open", { path }),
  clipReveal: (path: string) => invoke<void>("clip_reveal", { path }),
  appInfo: () => invoke<AppInfo>("app_info"),
  openLink: (kind: "coffee" | "support" | "releases") => invoke<void>("open_link", { kind }),
  updateInfo: () => invoke<UpdateInfo>("update_info"),
  updateCheck: () => invoke<void>("update_check"),
  updateDownload: () => invoke<void>("update_download"),
  updateInstall: () => invoke<void>("update_install"),
};

export function onStatus(f: (s: Status) => void): Promise<UnlistenFn> {
  return listen<Status>("status", (e) => f(e.payload));
}

export function onSettings(f: (s: Settings) => void): Promise<UnlistenFn> {
  return listen<Settings>("settings", (e) => f(e.payload));
}

export function onUpdate(f: (u: UpdateInfo) => void): Promise<UnlistenFn> {
  return listen<UpdateInfo>("update", (e) => f(e.payload));
}

/** A clip or screenshot was saved, or old files were cleaned up. */
export async function onClipsChanged(f: () => void): Promise<UnlistenFn> {
  const offs = await Promise.all([listen("clip_saved", f), listen("clips_changed", f)]);
  return () => offs.forEach((off) => off());
}

export interface BadgeFlash {
  kind: string;
  label: string;
  secs: number | null;
  tone: "ok" | "rec" | "paused" | "err";
  duration_ms: number;
}

/** A clip was saved; the payload is its length in seconds. */
export function onClipFlash(f: (secs: number) => void): Promise<UnlistenFn> {
  return listen<number>("clip_flash", (e) => f(e.payload));
}

export function onBadgeFlash(f: (flash: BadgeFlash) => void): Promise<UnlistenFn> {
  return listen<BadgeFlash>("badge_flash", (e) => f(e.payload));
}

export function onScreenshotFlash(f: () => void): Promise<UnlistenFn> {
  return listen("screenshot_flash", () => f());
}

export function onBadgeIcon(f: () => void): Promise<UnlistenFn> {
  return listen("badge_icon", f);
}

export function errorText(e: unknown): string {
  return typeof e === "string" ? e : e instanceof Error ? e.message : JSON.stringify(e);
}
