// Typed wrappers for the shell's commands and events (src-tauri/src/commands.rs).
import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";

export type Codec = "av1" | "hevc" | "h264";
export type PresetName = "compat" | "balanced" | "high" | "custom";
export type Theme = "system" | "light" | "dark";

export interface Settings {
  preset: PresetName;
  custom: { width: number; height: number; fps: number; codec: Codec | null; quality: number };
  buffer_secs: number;
  container: "mp4" | "mkv";
  clips_dir: string | null;
  cursor: boolean;
  display: number | null;
  audio: { enabled: boolean; mic: boolean; game: string[]; voice: string[]; media: string[] };
  hotkeys: { save_15: string; save_30: string; save_60: string; save_full: string };
  save_sound: boolean;
  save_toast: boolean;
  autostart: boolean;
  arm_on_start: boolean;
  theme: Theme;
}

export type Phase = "idle" | "starting" | "armed" | "recovering" | "error";

export interface Status {
  phase: Phase;
  encoder: string;
  resolution: string;
  source: string;
  buffer_secs: number;
  buffered_secs: number;
  buffered_mb: number;
  buffer_estimate_mb: number;
  frames: number;
  dropped: number;
  error: string | null;
  notes: string[];
  recoveries: number;
  last_clip: string | null;
  hotkey_errors: string[];
  settings_warning: string | null;
}

export interface Display {
  number: number;
  label: string;
  primary: boolean;
}

export const api = {
  status: () => invoke<Status>("status"),
  arm: () => invoke<void>("arm"),
  disarm: () => invoke<void>("disarm"),
  saveClip: (secs: number | null) => invoke<void>("save_clip", { secs }),
  configGet: () => invoke<Settings>("config_get"),
  configSet: (settings: Settings) => invoke<Status>("config_set", { settings }),
  defaultClipsDir: () => invoke<string>("default_clips_dir"),
  displays: () => invoke<Display[]>("displays"),
  estimateMb: (settings: Settings) => invoke<number>("estimate_mb", { settings }),
  openClipsFolder: () => invoke<void>("open_clips_folder"),
  revealLastClip: () => invoke<void>("reveal_last_clip"),
  pickClipsDir: () => invoke<string | null>("pick_clips_dir"),
  openSettings: () => invoke<void>("open_settings"),
  menuReady: (width: number, height: number) => invoke<void>("menu_ready", { width, height }),
  quit: () => invoke<void>("quit"),
};

export function onStatus(f: (s: Status) => void): Promise<UnlistenFn> {
  return listen<Status>("status", (e) => f(e.payload));
}

export function onSettings(f: (s: Settings) => void): Promise<UnlistenFn> {
  return listen<Settings>("settings", (e) => f(e.payload));
}

export function errorText(e: unknown): string {
  return typeof e === "string" ? e : e instanceof Error ? e.message : JSON.stringify(e);
}
