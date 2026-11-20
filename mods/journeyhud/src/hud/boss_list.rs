//! Spreads the game's boss health bars further apart, so the posture bar and the effect icons the
//! mod draws under a bar fit before the next one (`docs/boss_bar.md`). The bars are the clips
//! `BossList/Item_0_0` to `Item_2_0` of the HUD movie, 55 px apart: the movie is edited as the game
//! opens it (`hud::movie`).

use std::sync::atomic::{AtomicU8, AtomicU32, Ordering};

use shared::gfx;
use shared::logger;

use crate::settings;

const PATH: [&str; 1] = ["BossList"];
/// Pixels added to the gap between two boss bars (the game has 55).
const EXTRA_PX: f32 = 55.0;
/// Pixels `BossBar` moves the lowest bar down: its bottom edge goes from y 876 to 990 of the 1080
/// picture, to the height of the flask's name.
const DROP_PX: f32 = 114.0;
const TWIPS_PER_PX: f32 = 20.0;

/// What [edit] changed, as `f32` bits: the added gap and the drop, in pixels (0 when it did not run).
static EXTRA: AtomicU32 = AtomicU32::new(0);
static DROP: AtomicU32 = AtomicU32::new(0);
/// What `Target.BossBar` was when the movie was edited.
static EDITED_WITH: AtomicU8 = AtomicU8::new(NOT_EDITED);
const NOT_EDITED: u8 = 0;
const EDITED_OFF: u8 = 1;
const EDITED_ON: u8 = 2;

/// Warns when `Target.BossBar` is changed after the movie was edited: the bars only move, or move
/// back, the next time the game starts.
pub fn warn_if_stale(boss_bar: bool) {
    let edited_with = EDITED_WITH.load(Ordering::Relaxed);
    if edited_with != NOT_EDITED && (edited_with == EDITED_ON) != boss_bar {
        logger::warn(
            "Target.BossBar changed after the game started: the boss bars move only at the next start",
        );
    }
}

/// Pixels the game's boss bars are further apart than they were.
pub fn extra_px() -> f32 {
    f32::from_bits(EXTRA.load(Ordering::Relaxed))
}

/// Pixels the game's boss bars were moved down.
pub fn drop_px() -> f32 {
    f32::from_bits(DROP.load(Ordering::Relaxed))
}

pub fn edit(movie: &[u8]) -> Option<Vec<u8>> {
    let extra = if settings::boss_bar() { EXTRA_PX } else { 0.0 };
    let drop = if settings::boss_bar() { DROP_PX } else { 0.0 };
    EDITED_WITH.store(if settings::boss_bar() { EDITED_ON } else { EDITED_OFF }, Ordering::Relaxed);
    let twips = |px: f32| (px * TWIPS_PER_PX) as i32;
    let shifts = [
        ("Item_0_0", 0, twips(drop)),
        ("Item_1_0", 0, twips(drop - extra)),
        ("Item_2_0", 0, twips(drop - 2.0 * extra)),
    ];
    match gfx::shift_placements(movie, &PATH, &shifts) {
        Ok(edited) => {
            EXTRA.store(extra.to_bits(), Ordering::Relaxed);
            DROP.store(drop.to_bits(), Ordering::Relaxed);
            Some(edited)
        }
        Err(err) => {
            logger::warn(&format!(
                "BossList: movie edit failed ({err:?}), the bars stay as they are"
            ));
            None
        }
    }
}
