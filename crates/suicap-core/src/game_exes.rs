//! Program names of known games, baked in from Discord's public detectable-games list
//! (`scripts/update_game_exes.py` rebuilds `data/game_exes.txt`). A program on this list that
//! plays sound is sent to the Game track unless the user already placed it somewhere.

use std::{collections::HashSet, sync::OnceLock};

static NAMES: &str = include_str!("../data/game_exes.txt");

/// True when `exe` (a file name like `Overwatch.exe`, any case) is a known game program.
pub fn is_game_exe(exe: &str) -> bool {
    static SET: OnceLock<HashSet<&'static str>> = OnceLock::new();
    let set = SET.get_or_init(|| NAMES.lines().collect());
    set.contains(exe.to_ascii_lowercase().as_str())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_any_case() {
        assert!(is_game_exe("Overwatch.exe"));
        assert!(is_game_exe("overwatch.exe"));
    }

    #[test]
    fn leaves_ordinary_apps_and_generic_names_out() {
        for n in ["chrome.exe", "discord.exe", "vlc.exe", "game.exe", "client.exe"] {
            assert!(!is_game_exe(n), "{n}");
        }
    }
}
