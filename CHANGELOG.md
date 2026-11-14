# Changelog

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
