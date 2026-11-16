"""Rebuild crates/suicap-core/data/game_exes.txt from Discord's public detectable-games list.

Run: python scripts/update_game_exes.py   (needs network; no auth)
Keeps Windows, non-launcher executable names, lowercased; drops names shared by many games
(game.exe, client.exe ...) and a few generic or non-game names, which would mis-route apps.
"""
import collections, json, pathlib, urllib.request

URL = "https://discord.com/api/v10/applications/detectable"
OUT = pathlib.Path(__file__).resolve().parent.parent / "crates/suicap-core/data/game_exes.txt"
SHARED_MAX = 7  # a name used by more games than this is generic
KEEP = {"hl2.exe"}
GENERIC = {
    "client.exe", "launcher.exe", "engine.exe", "build.exe", "udk.exe", "castle.exe", "fate.exe",
    "metro.exe", "setup.exe", "install.exe", "start.exe", "run.exe", "main.exe", "app.exe",
    "chrome.exe", "msedge.exe", "firefox.exe", "discord.exe", "steam.exe", "spotify.exe",
    "explorer.exe", "java.exe", "javaw.exe", "python.exe", "node.exe", "electron.exe",
}

req = urllib.request.Request(URL, headers={"User-Agent": "game-exe-list"})
games = json.load(urllib.request.urlopen(req, timeout=120))
count = collections.Counter()
for g in games:
    seen = set()
    for e in g.get("executables") or []:
        if e.get("os") != "win32" or e.get("is_launcher"):
            continue
        name = e["name"].split("/")[-1].lstrip(">").strip().lower()
        if name not in seen:
            seen.add(name)
            count[name] += 1

def ok(n):
    return (
        5 <= len(n) <= 64 and n.endswith(".exe") and n not in GENERIC
        and not any(ord(c) < 32 or c in '<>:"/\|?*' for c in n)
        and (count[n] <= SHARED_MAX or n in KEEP)
    )

names = sorted(n for n in count if ok(n))
OUT.parent.mkdir(parents=True, exist_ok=True)
OUT.write_text("\n".join(names) + "\n", encoding="utf8")
print(f"{len(names)} executables -> {OUT}")
