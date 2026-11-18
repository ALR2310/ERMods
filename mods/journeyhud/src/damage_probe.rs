//! Shared hit capture for floating damage and the optional Logging.DamageProbe.
//! The hook copies into a reusable bounded buffer; the frame task logs it.

use std::cell::UnsafeCell;
use std::mem::MaybeUninit;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use eldenring::cs::{ChrCtrl, ChrIns, FieldInsHandle, WorldChrMan};
use fromsoftware_shared::FromStatic;

use shared::{codepatch, logger, memscan};

use crate::settings;

// The call into AutoRegen's observed function, without patching its prologue.
const CALL_AOB: &str = "4D 8B C6 49 8B D4 44 88 6C 24 20 48 8B CD E8 ?? ?? ?? ?? 44 0F B6 A4 24 A8 00 00 00 4D 8B C6 45 0F B6 CC";
const CALL_AT: usize = 14;
const LIMIT: usize = 512;
const FOLLOW_FRAMES: u32 = 120;
const FOLLOW_TIME: Duration = Duration::from_secs(2);

type HitFn = unsafe extern "C" fn(usize, usize, usize, u64, u8);

static ORIGINAL: AtomicUsize = AtomicUsize::new(0);
static CAPTURING: AtomicBool = AtomicBool::new(false);
static PLAYER: AtomicUsize = AtomicUsize::new(0);
static NEXT: AtomicUsize = AtomicUsize::new(0);
static DROPPED: AtomicUsize = AtomicUsize::new(0);
static SLOTS: [Slot; LIMIT] = [const { Slot::new() }; LIMIT];

#[derive(Clone, Copy)]
pub(super) struct State {
    pub hp: Option<i32>,
    pub resist: Option<[i32; 7]>,
}

#[derive(Clone, Copy)]
pub(super) struct Event {
    pub at: Instant,
    pub position: Option<[f32; 3]>,
    pub sequence: usize,
    attacker: Option<u64>,
    pub target: Option<u64>,
    pub target_address: Option<usize>,
    pub own: bool,
    incoming: bool,
    pub damage: Option<i32>,
    pub components: Option<[f32; 5]>,
    /// Fire, magic, lightning and holy after the reduction, truncated.
    pub elements: Option<[i32; 4]>,
    pub hit_kind: Option<u8>,
    /// The first 0x280 bytes of the hit record, copied only for the damage probe.
    raw: Option<[u32; 160]>,
    pub before: State,
    pub after: State,
    flag: u8,
    arg4: u64,
}

#[cfg(test)]
impl Event {
    pub fn test_hit() -> Self {
        Self {
            at: Instant::now(),
            position: Some([0.0, 0.0, 10.0]),
            sequence: 0,
            attacker: Some(1),
            target: Some(2),
            target_address: None,
            own: true,
            incoming: false,
            damage: Some(574),
            components: Some([151.0, 0.0, 0.0, 0.0, 155.0]),
            elements: None,
            hit_kind: Some(2),
            raw: None,
            before: State { hp: Some(219), resist: None },
            after: State { hp: Some(0), resist: None },
            flag: 0,
            arg4: 0,
        }
    }
}

struct Slot {
    state: AtomicU8,
    event: UnsafeCell<MaybeUninit<Event>>,
}

// SAFETY: state 0 is free, 1 exclusively owns the payload, and 2 publishes it.
// Producers and the consumer acquire ownership with CAS; release publishes or
// frees a slot only after the payload access finishes.
unsafe impl Sync for Slot {}

impl Slot {
    const fn new() -> Self {
        Self { state: AtomicU8::new(0), event: UnsafeCell::new(MaybeUninit::uninit()) }
    }
}

/// Where the character stands: the translation row of its physics matrix.
pub(super) fn position(chr: usize) -> Option<[f32; 3]> {
    let ctrl =
        memscan::read_safe::<usize>(chr.wrapping_add(std::mem::offset_of!(ChrIns, chr_ctrl)))
            .filter(|&p| p >= 0x10000)?;
    memscan::read_safe::<[f32; 3]>(
        ctrl.wrapping_add(std::mem::offset_of!(ChrCtrl, physics_model_matrix) + 48),
    )
    .filter(|point| point.iter().all(|n| n.is_finite()))
}

pub(super) fn state(chr: usize) -> State {
    let modules = memscan::read_safe::<usize>(chr.wrapping_add(0x190)).filter(|&p| p >= 0x10000);
    let data = modules.and_then(memscan::read_safe::<usize>).filter(|&p| p >= 0x10000);
    let resist = modules
        .and_then(|p| memscan::read_safe::<usize>(p.wrapping_add(0x20)))
        .filter(|&p| p >= 0x10000);
    State {
        hp: data.and_then(|p| memscan::read_safe(p.wrapping_add(0x138))),
        resist: resist.and_then(|p| memscan::read_safe(p.wrapping_add(0x10))),
    }
}

unsafe extern "C" fn observe(ctx: usize, attacker: usize, info: usize, arg4: u64, flag: u8) {
    // SAFETY: the original call target is stored before the redirect is installed.
    let original: HitFn = unsafe { std::mem::transmute(ORIGINAL.load(Ordering::Acquire)) };
    if !CAPTURING.load(Ordering::Acquire) {
        // SAFETY: all five original arguments, including the stack argument, are forwarded.
        unsafe { original(ctx, attacker, info, arg4, flag) };
        return;
    }
    let sequence = NEXT.fetch_add(1, Ordering::Relaxed);
    let slot = &SLOTS[sequence % LIMIT];
    if slot.state.compare_exchange(0, 1, Ordering::Acquire, Ordering::Relaxed).is_err() {
        DROPPED.fetch_add(1, Ordering::Relaxed);
        // SAFETY: a full buffer skips capture but still forwards every original argument.
        unsafe { original(ctx, attacker, info, arg4, flag) };
        return;
    }

    let target = memscan::read_safe::<usize>(ctx.wrapping_add(8)).filter(|&p| p >= 0x10000);
    let player = PLAYER.load(Ordering::Relaxed);
    let at = Instant::now();
    let position = target.and_then(position);
    let before = target.map(state).unwrap_or(State { hp: None, resist: None });
    let damage = memscan::read_safe::<i32>(info.wrapping_add(0x228));
    let components = memscan::read_safe::<[f32; 5]>(info);
    let elements = memscan::read_safe::<[i32; 4]>(info.wrapping_add(0x234));
    let hit_kind = memscan::read_safe::<u8>(info.wrapping_add(0xD9));
    let raw = settings::damage_probe().then(|| memscan::read_safe::<[u32; 160]>(info)).flatten();
    let attacker_handle = memscan::read_safe::<u64>(attacker.wrapping_add(8));
    let target_handle = target.and_then(|p| memscan::read_safe::<u64>(p.wrapping_add(8)));
    // SAFETY: the game provided these arguments at the original call site.
    unsafe { original(ctx, attacker, info, arg4, flag) };
    let event = Event {
        at,
        position,
        sequence,
        attacker: attacker_handle,
        target: target_handle,
        target_address: target,
        own: player != 0 && attacker == player,
        incoming: player != 0 && target == Some(player),
        damage,
        components,
        elements,
        hit_kind,
        raw,
        before,
        after: target.map(state).unwrap_or(State { hp: None, resist: None }),
        flag,
        arg4,
    };
    // SAFETY: CAS reserved this slot exclusively until publication.
    unsafe { (*slot.event.get()).write(event) };
    slot.state.store(2, Ordering::Release);
}

#[derive(Default)]
pub struct Probe {
    attempted: bool,
    installed: bool,
    watches: Vec<Watch>,
    numbers: crate::damage_numbers::Tracker,
}

struct Watch {
    address: usize,
    handle: u64,
    hit: usize,
    previous: State,
    after_hit_hp: Option<i32>,
    frames: u32,
    started: Instant,
    effects: Vec<i32>,
}

impl Probe {
    pub fn tick(&mut self) {
        let diagnostic = settings::damage_probe();
        let enabled = diagnostic || settings::damage_numbers();
        if !enabled && !self.installed {
            self.attempted = false;
            return;
        }
        PLAYER.store(
            shared::player::main_player_chr_ins_ptr().map_or(0, |p| p as usize),
            Ordering::Relaxed,
        );
        if enabled && !self.installed && !self.attempted && PLAYER.load(Ordering::Relaxed) != 0 {
            self.attempted = true;
            self.installed = install();
        }
        let capturing = enabled && self.installed && PLAYER.load(Ordering::Relaxed) != 0;
        if CAPTURING.swap(capturing, Ordering::AcqRel) != capturing {
            logger::log(if capturing {
                "Damage capture: started"
            } else {
                "Damage capture: stopped"
            });
        }
        let mut events = Vec::new();
        for slot in &SLOTS {
            if slot.state.compare_exchange(2, 1, Ordering::Acquire, Ordering::Relaxed).is_err() {
                continue;
            }
            // SAFETY: acquire observes the published write and CAS excludes other access.
            let e = unsafe { (*slot.event.get()).assume_init_read() };
            slot.state.store(0, Ordering::Release);
            if !enabled {
                continue;
            }
            events.push(e);
        }
        events.sort_unstable_by_key(|event| event.sequence);
        let dropped = DROPPED.swap(0, Ordering::Relaxed);
        self.numbers.update(&events, dropped != 0);
        for e in events {
            if !diagnostic {
                continue;
            }
            let loss = e
                .before
                .hp
                .zip(e.after.hp)
                .map(|(before, after)| i64::from(before) - i64::from(after));
            logger::log(&format!(
                "Damage probe: hit {} attacker {:?} target {:?} own {} incoming {} damage {:?} components {:?} elements {:?} hp {:?}->{:?} loss {:?} resist {:?}->{:?} hit_kind {:?} critical_candidate {:?} arg4 {} flag {}",
                e.sequence.wrapping_add(1),
                e.attacker,
                e.target,
                e.own,
                e.incoming,
                e.damage,
                e.components,
                e.elements,
                e.before.hp,
                e.after.hp,
                loss,
                e.before.resist,
                e.after.resist,
                e.hit_kind,
                e.hit_kind.map(|kind| kind == 2),
                e.arg4,
                e.flag
            ));
            if let Some(raw) = e.raw.filter(|_| e.own && e.damage.is_some_and(|n| n > 0)) {
                let words: Vec<String> = raw
                    .iter()
                    .enumerate()
                    .filter(|(_, word)| **word != 0)
                    .map(|(i, word)| format!("+{:#X}={word:#X}", i * 4))
                    .collect();
                logger::log(&format!(
                    "Damage probe: record hit {} {}",
                    e.sequence.wrapping_add(1),
                    words.join(" ")
                ));
            }
            self.watch(e.sequence.wrapping_add(1), e);
        }
        if dropped != 0 {
            logger::warn(&format!("Damage probe: buffer full, skipped {dropped} hits"));
        }
        if diagnostic {
            self.follow();
        } else {
            self.watches.clear();
        }
    }

    fn watch(&mut self, hit: usize, event: Event) {
        let (Some(address), Some(handle)) = (event.target_address, event.target) else { return };
        if let Some(index) = self.watches.iter().position(|watch| watch.handle == handle) {
            let previous = self.watches.remove(index);
            if let Some(loss) = previous
                .previous
                .hp
                .zip(event.before.hp)
                .map(|(before, after)| i64::from(before) - i64::from(after))
                && loss != 0
            {
                logger::log(&format!(
                    "Damage probe: gap hit {}->{} target {} hp {:?}->{:?} loss {} (before next intercepted hit)",
                    previous.hit, hit, handle, previous.previous.hp, event.before.hp, loss
                ));
            }
        }
        // A short bounded list avoids following an entire crowd during the probe.
        if self.watches.len() == 8 {
            let removed = self.watches.remove(0);
            logger::log(&format!("Damage probe: follow hit {} evicted", removed.hit));
        }
        self.watches.push(Watch {
            address,
            handle,
            hit,
            previous: event.after,
            after_hit_hp: event.after.hp,
            frames: 0,
            started: Instant::now(),
            effects: Vec::new(),
        });
    }

    fn follow(&mut self) {
        self.watches.retain_mut(|watch| {
            if memscan::read_safe::<u64>(watch.address + 8) != Some(watch.handle) {
                logger::log(&format!("Damage probe: follow hit {} target unavailable", watch.hit));
                return false;
            }
            let effects = effect_ids(watch.handle);
            if effects != watch.effects {
                logger::log(&format!(
                    "Damage probe: follow hit {} frame {} effects of the target {effects:?}",
                    watch.hit,
                    watch.frames + 1
                ));
                watch.effects = effects;
            }
            let current = state(watch.address);
            watch.frames += 1;
            let elapsed = watch.started.elapsed();
            let finished = watch.frames >= FOLLOW_FRAMES || elapsed >= FOLLOW_TIME;
            let loss = watch.previous.hp.zip(current.hp)
                .map(|(before, after)| i64::from(before) - i64::from(after));
            if watch.frames == 1 || current.hp != watch.previous.hp || finished {
                let delayed = watch.after_hit_hp.zip(current.hp)
                    .map(|(before, after)| i64::from(before) - i64::from(after));
                logger::log(&format!(
                    "Damage probe: follow hit {} target {} frame {} elapsed_ms {} hp {:?}->{:?} loss {:?} since_return {:?} resist {:?}->{:?} end {}",
                    watch.hit, watch.handle, watch.frames, elapsed.as_millis(),
                    watch.previous.hp, current.hp, loss, delayed,
                    watch.previous.resist, current.resist, finished
                ));
            }
            watch.previous = current;
            !finished && current.hp.is_some()
        });
    }
}

/// The param ids of the effects on the character of `handle`, sorted.
fn effect_ids(handle: u64) -> Vec<i32> {
    let raw = handle.to_ne_bytes();
    let Some(handle) = memscan::read_safe::<FieldInsHandle>(raw.as_ptr() as usize) else {
        return Vec::new();
    };
    // SAFETY: the accessor returns Err while the manager is unavailable.
    let Some(world) = (unsafe { WorldChrMan::instance() }).ok() else { return Vec::new() };
    let Some(chr) = crate::target::chr_by_handle(world, handle) else { return Vec::new() };
    let mut ids: Vec<i32> = chr.special_effect.entries().map(|entry| entry.param_id).collect();
    ids.sort_unstable();
    ids
}

fn install() -> bool {
    let Some(hit) = memscan::find_pattern_in_module(CALL_AOB) else {
        logger::error("Damage probe: call pattern missing, capture disabled");
        return false;
    };
    // SAFETY: CALL_AOB matches a mapped call instruction at CALL_AT.
    let site = unsafe { hit.add(CALL_AT) };
    // SAFETY: the matched call has five readable bytes.
    let Some(original) = (unsafe { codepatch::rel32_target(site) }) else { return false };
    // A prologue hook (such as AutoRegen) can chain naturally: this patch redirects
    // the caller and still invokes its original destination with all five arguments.
    ORIGINAL.store(original as usize, Ordering::Release);
    let stub = codepatch::absolute_jmp(observe as *const () as usize as u64);
    // SAFETY: the stub tail-jumps to the callback with the same five-argument ABI.
    if unsafe { codepatch::redirect_rel32(site, &stub) }.is_none() {
        logger::error("Damage probe: call redirect failed, capture disabled");
        return false;
    }
    true
}
