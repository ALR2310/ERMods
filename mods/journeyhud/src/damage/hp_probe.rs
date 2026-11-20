//! `Logging.DamageProbe` only: every HP value the game sets on a character, and the calls on the stack
//! that led to it. The game sets HP in one function (`sub_1404374E0` on 2.7.2.0): hits reach it through
//! `sub_140436B70`, the effects through `sub_140436AD0`, so the callers tell the sources apart. The hook
//! copies into a bounded buffer; the frame task logs it.

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use shared::{codepatch, logger, memscan};

use super::ring::Ring;

/// The start of the function that sets HP; unique on 2.6.2.0 to 2.7.2.0.
const SET_HP_AOB: &str = "48 89 5C 24 18 48 89 6C 24 20 89 54 24 10 56 57 41 56 48 83 EC 30 8B A9 38 01 00 00 48 8D B9 3C 01 00 00 45 33 F6 0F 29 74 24 20 44 89 74 24 50 0F 28 F3 41 0F B6 F0 48 8B D9 85 D2 79 07 48 8D 44 24 50 EB 0B 3B 17 48 8D 44 24 58 48 0F 4F C7 8B 00 89 81 38 01 00 00 E8 ?? ?? ?? ?? 84 C0 0F 85 ?? ?? ?? ??";
/// The first instruction, `mov [rsp+18h], rbx`, which the stub replays.
const REPLAYED: usize = 5;
const HP: usize = 0x138;
const MAX_HP: usize = 0x13C;
const CALLERS: usize = 6;
const SCAN_BYTES: usize = 0x400;

#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetModuleHandleA(name: *const u8) -> *mut c_void;
}

static ACTIVE: AtomicBool = AtomicBool::new(false);
static BASE: AtomicUsize = AtomicUsize::new(0);
static SIZE: AtomicUsize = AtomicUsize::new(0);
static WRITES: Ring<HpWrite, 256> = Ring::new();

#[derive(Clone, Copy)]
pub struct HpWrite {
    owner: usize,
    handle: Option<u64>,
    old: Option<i32>,
    asked: i32,
    max: Option<i32>,
    callers: [u32; CALLERS],
}

pub fn set_active(active: bool) {
    ACTIVE.store(active, Ordering::Release);
}

/// The return addresses on the stack from `sp` up, as RVAs: the first slot is the caller, the others
/// are values that look like one (a call just before them), so the list is a hint, not an unwind.
fn callers(sp: usize) -> [u32; CALLERS] {
    let (base, size) = (BASE.load(Ordering::Relaxed), SIZE.load(Ordering::Relaxed));
    let in_image = |value: usize| value > base && value < base + size;
    let after_call = |value: usize| {
        memscan::read_safe::<[u8; 6]>(value - 6)
            .is_some_and(|b| b[1] == 0xE8 || b[0] == 0xFF || b[3] == 0xFF || b[4] == 0xFF)
    };
    let mut found = [0; CALLERS];
    let mut count = 0;
    for offset in (0..SCAN_BYTES).step_by(8) {
        let Some(value) = memscan::read_safe::<usize>(sp.wrapping_add(offset)) else { break };
        if in_image(value) && (offset == 0 || after_call(value)) {
            found[count] = (value - base) as u32;
            count += 1;
            if count == CALLERS {
                break;
            }
        }
    }
    found
}

/// The HP the game stores for `asked`: not below 0 and not above the maximum.
fn clamped(asked: i32, max: i32) -> i32 {
    if asked < 0 { 0 } else { asked.min(max) }
}

/// Entered from the stub with the arguments of the HP function (the character's data module and the
/// HP asked for) and the stack pointer it was entered with.
unsafe extern "C" fn on_hp_write(this: usize, asked: i32, sp: usize) {
    if !ACTIVE.load(Ordering::Acquire) {
        return;
    }
    let old = memscan::read_safe::<i32>(this.wrapping_add(HP));
    let max = memscan::read_safe::<i32>(this.wrapping_add(MAX_HP));
    // The game sets HP again every frame with the value it already has.
    if old.zip(max).is_some_and(|(old, max)| old == clamped(asked, max)) {
        return;
    }
    let owner = memscan::read_safe::<usize>(this.wrapping_add(8)).unwrap_or(0);
    WRITES.push(HpWrite {
        owner,
        handle: memscan::read_safe::<u64>(owner.wrapping_add(8)),
        old,
        asked,
        max,
        callers: callers(sp),
    });
}

/// Hooks the HP function: the stub replays its first instruction, saves the argument registers and calls
/// `on_hp_write(rcx, edx, entry rsp)`; the jump back to the rest of the function is appended.
pub fn install() -> bool {
    let Some(target) = memscan::find_pattern_in_module(SET_HP_AOB) else {
        logger::warn("HP probe: the HP function was not found, not logging HP writes");
        return false;
    };
    // SAFETY: a null name asks for the main module, whose headers stay mapped.
    let base = unsafe { GetModuleHandleA(std::ptr::null()) } as usize;
    let nt = memscan::read_safe::<u32>(base + 0x3C).map_or(0, |offset| base + offset as usize);
    let Some(size) = memscan::read_safe::<u32>(nt + 0x50).filter(|_| nt != 0) else { return false };
    BASE.store(base, Ordering::Relaxed);
    SIZE.store(size as usize, Ordering::Relaxed);

    let mut stub = vec![
        0x48, 0x89, 0x5C, 0x24, 0x18, // mov [rsp+18h], rbx (replayed)
        0x51, // push rcx
        0x52, // push rdx
        0x41, 0x50, // push r8
        0x41, 0x51, // push r9
        0x48, 0x83, 0xEC,
        0x48, // sub rsp, 48h (shadow space, a float slot, 16-byte alignment)
        0xF3, 0x0F, 0x11, 0x5C, 0x24, 0x20, // movss [rsp+20h], xmm3 (the float argument)
        0x4C, 0x8D, 0x84, 0x24, 0x68, 0x00, 0x00, 0x00, // lea r8, [rsp+68h] (rsp at entry)
        0x48, 0xB8, // mov rax, imm64
    ];
    stub.extend((on_hp_write as *const () as usize as u64).to_le_bytes());
    stub.extend([
        0xFF, 0xD0, // call rax
        0xF3, 0x0F, 0x10, 0x5C, 0x24, 0x20, // movss xmm3, [rsp+20h]
        0x48, 0x83, 0xC4, 0x48, // add rsp, 48h
        0x41, 0x59, // pop r9
        0x41, 0x58, // pop r8
        0x5A, // pop rdx
        0x59, // pop rcx
    ]);
    // SAFETY: the pattern matched the function start; its first instruction is the five replayed bytes.
    if unsafe { codepatch::install_jmp_hook(target, REPLAYED, &stub) }.is_none() {
        logger::error("HP probe: the hook could not be installed");
        return false;
    }
    true
}

/// What the hook collected, oldest first.
pub fn drain() -> Vec<HpWrite> {
    let (writes, dropped) = WRITES.drain();
    if dropped != 0 {
        logger::log(&format!("HP write: {dropped} not logged, the buffer was full"));
    }
    writes
}

pub fn log(writes: &[HpWrite], player: usize) {
    for write in writes {
        let (Some(old), Some(max)) = (write.old, write.max) else { continue };
        let new = clamped(write.asked, max);
        logger::log(&format!(
            "HP write: {} {:?} {old}->{new} ({:+}) asked {} max {max} callers {:X?}",
            if write.owner == player { "player" } else { "target" },
            write.handle,
            new - old,
            write.asked,
            write.callers,
        ));
    }
}
