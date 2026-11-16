//! Parks a program by itself once it is playing sound and is on no list, and adds any
//! detected game (and any program on the baked-in known-games list) to the Game audio track.
//!
//! Runs in the background (the settings window need not be open). Listed and forgotten apps
//! are left alone, and SuiCap itself is never added. Non-game apps go to "Not recorded",
//! while the detected game goes directly to the Game track.

use std::time::Duration;
use suicap_core::config::{AudioSettings, Settings};
use tauri::{AppHandle, Manager};

use crate::state::AppState;

const EVERY: Duration = Duration::from_secs(3);
const MAX_PARKED: usize = 64;

pub fn start(app: &AppHandle) {
    let app = app.clone();
    std::thread::spawn(move || {
        loop {
            std::thread::sleep(EVERY);
            let st = app.state::<AppState>();
            let s = st.settings();
            if !s.audio.enabled || !s.audio.auto_add {
                continue;
            }
            let playing: Vec<String> = suicap_core::apps::audio_apps()
                .into_iter()
                .filter(|a| a.playing)
                .map(|a| a.exe)
                .collect();
            let game = suicap_core::game::detect().map(|g| g.window.exe);
            let Some(next) = with_new_apps(&s, &playing, game.as_deref()) else {
                continue;
            };
            if let Err(e) = crate::commands::config_set(app.clone(), next) {
                eprintln!("auto-add: {e}");
            }
        }
    });
}

fn listed(a: &AudioSettings, exe: &str) -> bool {
    [&a.game, &a.voice, &a.media, &a.ignored, &a.dismissed]
        .iter()
        .any(|l| l.iter().any(|n| n.eq_ignore_ascii_case(exe)))
}

/// `s` with the detected game added to `game` and every new playing app parked under `ignored`;
/// `None` when there is nothing to add.
pub fn with_new_apps(s: &Settings, playing: &[String], game: Option<&str>) -> Option<Settings> {
    let mut next = s.clone();
    let mut changed = false;
    if let Some(g) = game
        && valid(g)
        && !listed(&next.audio, g)
        && next.audio.game.len() < suicap_core::config::MAX_APPS
    {
        next.audio.game.push(g.to_owned());
        changed = true;
    }
    for exe in playing {
        let a = &mut next.audio;
        if listed(a, exe) || !valid(exe) {
            continue;
        }
        // A known game goes to the Game track; anything else is parked by itself.
        if suicap_core::game_exes::is_game_exe(exe) {
            if a.game.len() < suicap_core::config::MAX_APPS {
                a.game.push(exe.clone());
                changed = true;
            }
            continue;
        }
        if a.ignored.len() >= MAX_PARKED {
            continue;
        }
        a.ignored.push(exe.clone());
        changed = true;
    }
    changed.then_some(next)
}

fn valid(exe: &str) -> bool {
    (5..=64).contains(&exe.len())
        && exe.to_ascii_lowercase().ends_with(".exe")
        && !exe.chars().any(|c| c.is_control() || r#"<>:"/\|?*"#.contains(c))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn playing(l: &[&str]) -> Vec<String> {
        l.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn a_new_playing_app_is_parked_under_not_recorded() {
        let s = Settings::default();
        let n = with_new_apps(&s, &playing(&["vlc.exe"]), None).unwrap();
        assert!(n.audio.ignored.iter().any(|m| m == "vlc.exe"));
        assert_eq!(n.audio.media, s.audio.media);
    }

    #[test]
    fn a_known_game_playing_sound_goes_to_game_unless_the_user_moved_it() {
        let s = Settings::default();
        let n = with_new_apps(&s, &playing(&["Overwatch.exe"]), None).unwrap();
        assert_eq!(n.audio.game, ["Overwatch.exe"]);
        assert!(!n.audio.ignored.iter().any(|m| m == "Overwatch.exe"));
        let mut moved = Settings::default();
        moved.audio.media.push("overwatch.exe".into());
        assert!(with_new_apps(&moved, &playing(&["Overwatch.exe"]), None).is_none());
        let mut forgotten = Settings::default();
        forgotten.audio.ignored.push("overwatch.exe".into());
        assert!(with_new_apps(&forgotten, &playing(&["Overwatch.exe"]), None).is_none());
    }

    #[test]
    fn listed_parked_and_forgotten_apps_are_left_alone() {
        let mut s = Settings::default();
        s.audio.ignored.push("a.exe".into());
        s.audio.dismissed.push("b.exe".into());
        assert!(with_new_apps(&s, &playing(&["A.EXE", "b.exe", "discord.exe"]), None).is_none());
    }

    #[test]
    fn the_detected_game_is_added_to_game_track() {
        let s = Settings::default();
        let n = with_new_apps(&s, &playing(&["halo.exe"]), Some("HALO.exe")).unwrap();
        assert_eq!(n.audio.game, ["HALO.exe"]);
        assert_eq!(n.audio.ignored, s.audio.ignored);
    }

    #[test]
    fn dismissed_or_listed_game_is_not_readded() {
        let mut s = Settings::default();
        s.audio.dismissed.push("halo.exe".into());
        assert!(with_new_apps(&s, &playing(&["halo.exe"]), Some("HALO.exe")).is_none());
    }
}
