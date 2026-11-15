# Changelog

## 0.1.3 (in development)

## 0.1.2 (2026-11-15)

### Added
- **Continuous recording mode** alongside Replay on the dashboard, saving directly to disk until stopped. Includes automatic gapless segment splitting by time (5 to 240 minutes) or file size (500 MB to 10 GB), maximum session length, pause and resume (manual or automatic while idle), manual segment splitting, waiting for the chosen game or application before recording, automatic split when switching applications, minimum free disk space protection, and an option to keep the replay buffer active at the same time.
- **Live video preview** above the Video panel showing how the selected source (Display, Application, or Auto) will frame inside the chosen output resolution, including letterboxing, fallback status, an optional corner badge overlay preview, and live/refresh controls.
- **Storage tab in Settings** to select, open, or reset both the **Clips folder** and the **Temp folder** (used for in-progress recordings and MP4 remuxing), choose `MP4` or `MKV`, and adjust the library storage limit and minimum free disk space reserve.
- **Continuous recording shortcuts and tray controls** to start or stop recording (default `Ctrl+Alt+F6`), pause or resume, and split segments from the tray menu or dashboard.
- **Recording badge lifecycle flashes** showing distinct icons and messages when a recording starts, pauses, resumes, splits into a new segment, finishes saving, or stops due to a limit.
- **Color-coded format tags** (`MP4`, `MKV`, `PNG`) on clip and screenshot cards in the Clips library.

### Fixed
- Interrupted or orphaned temporary recording files left behind by a crash or forced shutdown are now cleaned up automatically when SuiCap launches.

## 0.1.1 (2026-11-15)

### Added
- Recording badge on the monitor being captured, in any corner (default bottom-right), hidden from the recording. It shows while replay is on or you record, with a live preview in settings, a timer, a label with your own text, icon presets or your own image, and size, opacity, edge distance and color controls.
- Clip size tile on the Replay card: the most a clip of the whole buffer can weigh with your current settings.
- Apps that play sound and sit on no list are parked under Not recorded automatically (switch in the audio settings). Apps you remove by hand stay removed.
- Codecs your GPU cannot encode are greyed out, with a note naming the GPU.
- Styled tooltips across the app.

### Changed
- Game detection now finds games in exclusive, borderless and windowed fullscreen, and games Windows knows about even when they run in a window. Ordinary maximised apps are no longer mistaken for games.
- With an empty Game list, the Game track now follows the game while replay runs, so a game started after replay was turned on is recorded too. The detected game shows in the Game column.
- Listed apps (Spotify, Chrome and so on) only show in a track while they play sound; the rest stay hidden until they do.
- Moving apps between tracks, parking or releasing them, and muting or unmuting a track now take effect within about a second while recording, with no restart and no cleared buffer. The app's sound resumes on its new track.
- Quitting from the tray now closes every window and the tray icon at once and never leaves a SuiCap process behind.

### Fixed
- The tray menu now opens when the settings window is closed, and no longer shows shadow lines around its border.
- The app no longer occasionally closes while replay is on (often around saving a clip) when some audio reaches it late.
- A game that was already running full screen when SuiCap started is now detected, even with the SuiCap window in front of it.
- Display recording no longer stays frozen after a game switches into or out of exclusive fullscreen or changes the display mode.
- Turning replay on while an exclusive full-screen game holds the screen no longer fails; recording starts and picks up the game's picture as soon as it is available.
- Codec buttons keep an equal size.

## 0.1.0 (2026-11-14)

### Added
- Replay buffer that keeps the last 10 seconds to 10 minutes of your screen and audio in memory.
- Shortcuts to save the last 15, 30 or 60 seconds or the whole buffer, working in full-screen games (default Ctrl+Alt+F9 to F12).
- Clips are saved in a folder per game, with a chime, on-screen badge flash, and a Windows notification when a clip or screenshot is ready.
- Tray icon that shows whether replay is on; right-click opens a menu to save clips, turn replay on or off, open the clips folder, open settings or quit.
- Settings window with sidebar navigation, video presets (Compatible, Balanced, High, Custom), resolution tags (`720p`, `1080p`, `1440p`, `4K`), monitor or application capture, mouse pointer, buffer length, MP4 or MKV, clips folder, audio sources, shortcuts, and light or dark theme.
- Separate audio tracks for microphone, game (with automatic captured-game detection), voice chat and music, plus a mixed track that plays by default.
- Option to start with Windows in the tray and to turn replay on at start.
- Clips page: thumbnails, multi-select and batch delete to the Recycle Bin, lossless trim (saved as a new copy, the original is kept), and an optional storage limit that removes the oldest clips.
- Screenshot button and shortcut (default Ctrl+Alt+F8) that saves a PNG of the screen next to your clips with instant sound and badge feedback.
- About page with developer and support links, and an Updates page that checks GitHub releases on app launch, verifies each installer against its checksum, and can install automatically while you are away (default: notify only).
