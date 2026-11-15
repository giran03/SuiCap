//! Parks a program by itself once it is playing sound and is on no list.
//!
//! Runs in the background (the settings window need not be open). The app goes to
//! "Not recorded", so nothing is captured that the user did not choose (they drag it to a track
//! to record it). Listed and forgotten apps are left alone, and so is the detected game, which
//! the Game track follows by itself, and SuiCap itself.

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

/// `s` with every new playing app placed on a track; `None` when there is nothing to add.
pub fn with_new_apps(s: &Settings, playing: &[String], game: Option<&str>) -> Option<Settings> {
    let mut next = s.clone();
    let mut changed = false;
    for exe in playing {
        let a = &mut next.audio;
        if listed(a, exe) || !valid(exe) {
            continue;
        }
        if game.is_some_and(|g| g.eq_ignore_ascii_case(exe)) {
            continue; // the Game track follows the detected game itself; never silence it
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
    fn listed_parked_and_forgotten_apps_are_left_alone() {
        let mut s = Settings::default();
        s.audio.ignored.push("a.exe".into());
        s.audio.dismissed.push("b.exe".into());
        assert!(with_new_apps(&s, &playing(&["A.EXE", "b.exe", "discord.exe"]), None).is_none());
    }

    #[test]
    fn the_detected_game_is_never_parked() {
        let s = Settings::default();
        assert!(with_new_apps(&s, &playing(&["halo.exe"]), Some("HALO.exe")).is_none());
    }
}
