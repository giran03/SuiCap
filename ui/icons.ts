// App icons come from the shell as PNG data URLs. One lookup per program, shared by every
// chip that shows it; programs whose icon cannot be found are retried after a minute.
import { api } from "./api";

const found = new Map<string, string>();
const pending = new Map<string, Promise<string | null>>();
const missedAt = new Map<string, number>();
const RETRY_MS = 60_000;

export function cachedIcon(exe: string): string | null {
  return found.get(exe.toLowerCase()) ?? null;
}

export function iconFor(exe: string): Promise<string | null> {
  const key = exe.toLowerCase();
  const hit = found.get(key);
  if (hit) return Promise.resolve(hit);
  const missed = missedAt.get(key);
  if (missed && Date.now() - missed < RETRY_MS) return Promise.resolve(null);
  let p = pending.get(key);
  if (!p) {
    p = api
      .appIcon(key)
      .catch(() => null)
      .then((url) => {
        if (url) found.set(key, url);
        else missedAt.set(key, Date.now());
        pending.delete(key);
        return url;
      });
    pending.set(key, p);
  }
  return p;
}

// Names the audio sessions reported, so a chip shows "Spotify Premium" rather than "spotify".
const names = new Map<string, string>();
export function rememberNames(apps: { exe: string; name: string }[]) {
  for (const a of apps) names.set(a.exe.toLowerCase(), a.name);
}

/** "discord.exe" -> "Discord", or the name its audio session reported. */
export function friendlyName(exe: string): string {
  const seen = names.get(exe.toLowerCase());
  const stem = seen ?? exe.replace(/\.exe$/i, "");
  return stem.charAt(0).toUpperCase() + stem.slice(1);
}
