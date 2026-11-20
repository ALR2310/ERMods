//! The HP the game's effects take from a character each frame: poison, scarlet rot, the bleed and frost
//! bursts, burns. The game sums them in one function (`sub_1404F8BB0` on 2.7.2.0) that also finds who
//! the loss is credited to and the `spAttribute` of the effect behind it. The hook stands in for the one
//! call of that function, runs it and copies what it wrote; the frame task reads the copies.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Instant;

use shared::{codepatch, logger, memscan};

use super::ring::Ring;

/// The call of the calculation inside its only caller; the call is at `CALL_AT`. Unique on 2.6.2.0 to 2.7.2.0.
const CALL_AOB: &str = "E8 ?? ?? ?? ?? 48 8B C8 88 5C 24 28 48 8D 45 D8 8B D7 4C 8D 4D 38 48 89 44 24 20 4C 8D 45 48 E8 ?? ?? ?? ?? 8B 55 DC 32 C9";
const CALL_AT: usize = 31;
/// `SpecialEffect::owner`.
const OWNER: usize = 0x10;

static ACTIVE: AtomicBool = AtomicBool::new(false);
static ORIGINAL: AtomicUsize = AtomicUsize::new(0);
static TICKS: Ring<EffectTick, 64> = Ring::new();

/// What the calculation found for one character in one frame, when it was not zero.
#[derive(Clone, Copy)]
pub struct EffectTick {
    pub at: Instant,
    pub owner: usize,
    pub handle: Option<u64>,
    pub max: i32,
    /// The handle the loss is credited to, `-1` for none.
    pub causer: i64,
    /// The `spAttribute` of the effect: 20 poison, 21 scarlet rot, 22 blood loss, 23 frostbite; `-2` for
    /// the effects that have none (burns, the proc of death blight and madness).
    pub kind: i8,
    /// HP change of the frame, negative for a loss; the first is the one that matters.
    pub changes: [i32; 3],
}

pub fn set_active(active: bool) {
    ACTIVE.store(active, Ordering::Release);
}

type CalculationFn = unsafe extern "C" fn(usize, i32, *mut i64, *mut i8, *mut [i32; 3], u8);

/// Stands in for the call of the calculation: runs it, then records its result.
unsafe extern "C" fn on_calculation(
    container: usize,
    max: i32,
    causer: *mut i64,
    kind: *mut i8,
    changes: *mut [i32; 3],
    flag: u8,
) {
    // SAFETY: the original is stored before the redirect is installed.
    let original: CalculationFn = unsafe { std::mem::transmute(ORIGINAL.load(Ordering::Acquire)) };
    // SAFETY: the arguments are those of the game's call, forwarded as they came.
    unsafe { original(container, max, causer, kind, changes, flag) };
    if !ACTIVE.load(Ordering::Acquire) {
        return;
    }
    // SAFETY: the original has just written through these pointers, which the caller keeps alive.
    let (causer, kind, changes) = unsafe { (causer.read(), kind.read(), changes.read()) };
    if changes == [0; 3] {
        return;
    }
    let owner = memscan::read_safe::<usize>(container.wrapping_add(OWNER)).unwrap_or(0);
    TICKS.push(EffectTick {
        at: Instant::now(),
        owner,
        handle: memscan::read_safe::<u64>(owner.wrapping_add(8)),
        max,
        causer,
        kind,
        changes,
    });
}

/// Redirects the call of the calculation to the hook.
pub fn install() -> bool {
    let Some(hit) = memscan::find_pattern_in_module(CALL_AOB) else {
        logger::warn("Effect damage: the calculation was not found, capture disabled");
        return false;
    };
    // SAFETY: the pattern matched a call instruction at `CALL_AT`.
    let site = unsafe { hit.add(CALL_AT) };
    // SAFETY: the matched call has five readable bytes.
    let Some(original) = (unsafe { codepatch::rel32_target(site) }) else { return false };
    ORIGINAL.store(original as usize, Ordering::Release);
    let stub = codepatch::absolute_jmp(on_calculation as *const () as usize as u64);
    // SAFETY: the stub tail-jumps to the callback, which takes the same six arguments.
    if unsafe { codepatch::redirect_rel32(site, &stub) }.is_none() {
        logger::error("Effect damage: the call could not be redirected, capture disabled");
        return false;
    }
    true
}

/// What the hook collected, oldest first, and how many it had to drop.
pub fn drain() -> (Vec<EffectTick>, usize) {
    TICKS.drain()
}

pub fn log(ticks: &[EffectTick], player: usize) {
    for tick in ticks {
        logger::log(&format!(
            "HP effects: {} {:?} changes {:?} max {} credited to {:X} kind {}",
            if tick.owner == player { "player" } else { "target" },
            tick.handle,
            tick.changes,
            tick.max,
            tick.causer,
            tick.kind,
        ));
    }
}
