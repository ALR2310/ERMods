//! The edits to the game's HUD movie (`01_000_fe.gfx`). `shared::gfx::hook` allows one hook per
//! DLL, so one function runs every edit, each on the result of the one before.

use shared::gfx::hook;
use shared::logger;

use crate::settings;

use super::{boss_list, enemy_tags};

const MOVIE: &str = "01_000_fe.gfx";

fn edit(movie: &[u8]) -> Option<Vec<u8>> {
    crate::state_icons::read_root(movie);
    let mut edited: Option<Vec<u8>> = None;
    for step in [boss_list::edit, enemy_tags::edit] {
        if let Some(next) = step(edited.as_deref().unwrap_or(movie)) {
            edited = Some(next);
        }
    }
    edited
}

/// Hooks the movie open (on a thread of its own: it waits for the game's code). Only when an
/// option that edits it is on, read when the mod starts.
pub fn install() {
    if !settings::boss_bar()
        && settings::enemy_hp_bar()
        && settings::enemy_damage()
        && !settings::player_buildup()
    {
        return;
    }
    std::thread::spawn(|| {
        if hook::install(MOVIE, b"EnemyTag0", edit) {
            logger::log("HudMovie: hooked");
        }
    });
}
