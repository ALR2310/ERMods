//! Swaps the bytes of one Scaleform movie for an edited copy as the game opens
//! it, so a mod can change a menu movie without shipping the file. The game's
//! file opener is only ever reached through a function-pointer slot in the exe's
//! data, so the slot is repointed instead of hooking the function (which
//! ERNativeUI does): both can coexist. Several DLLs of ours can each edit a movie: they chain, each
//! detour calling what its slot held before. Design and checks: `shared/docs/gfx.md`.

use std::ffi::c_void;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use crate::{codepatch, conflict, logger, memscan};

/// The opener (`sub_140D6E970` on 2.7.2.0) from its 17th byte on: ERNativeUI
/// hooks the function's entry, and its jump overwrites up to the first 14
/// bytes, so a pattern that starts at the entry stops matching once it is
/// loaded. The body below is untouched. Unique on 2.6.2 - 2.7.2.
const OPENER_AOB: &str = "48 81 EC B0 00 00 00 48 C7 45 B7 FE FF FF FF 48 8B 05 ?? ?? ?? ?? 48 33 C4 48 89 45 1F 45 8B F9 45 8B F0      48 8B DA 48 8B F1 48 8B 0D ?? ?? ?? ?? 48 85 C9 75 0F E8";
/// Where the pattern sits from the function's first byte.
const OPENER_OFFSET: usize = 16;

/// `(opener, path, flags, mode)`, returns the opened file object.
type OpenFn = unsafe extern "system" fn(*mut c_void, *const u8, i32, i32) -> *mut c_void;

/// The in-memory file object the opener returns for a movie read from the
/// archives: `+0x08` refcount, `+0x18` bytes, `+0x20` length, `+0x24` cursor,
/// `+0x28` valid flag.
const FILE_REFCOUNT: usize = 0x08;
const FILE_BYTES: usize = 0x18;
const FILE_LENGTH: usize = 0x20;
const FILE_CURSOR: usize = 0x24;
const FILE_VALID: usize = 0x28;
const FILE_OBJECT_LEN: usize = 0x29;
/// A movie bigger than this is not touched.
const MAX_MOVIE_LEN: usize = 64 << 20;

type Edit = fn(&[u8]) -> Option<Vec<u8>>;

struct Target {
    file_name: &'static str,
    marker: &'static [u8],
    edit: Edit,
}

/// What the slots held before this DLL's detour: the opener, or the detour of another DLL of ours.
static ORIGINAL: AtomicUsize = AtomicUsize::new(0);
static TARGET: OnceLock<Target> = OnceLock::new();
/// The edited movie, built once and kept for the process lifetime (Scaleform
/// can hold the pointer as long as the movie lives). `None`: left unchanged.
static REPLACEMENT: OnceLock<Option<&'static [u8]>> = OnceLock::new();
static PUBLISH: Mutex<()> = Mutex::new(());

#[link(name = "kernel32")]
unsafe extern "system" {
    fn CreateMutexA(attrs: *const c_void, initial_owner: i32, name: *const u8) -> *mut c_void;
    fn WaitForSingleObject(handle: *mut c_void, milliseconds: u32) -> u32;
    fn ReleaseMutex(handle: *mut c_void) -> i32;
    fn GetCurrentProcessId() -> u32;
    fn SetEnvironmentVariableA(name: *const u8, value: *const u8) -> i32;
}

/// `<opener>:<slot>,<slot>...` in hex, set by the first DLL that scanned for the slots. Once a DLL has
/// repointed them they no longer hold the opener, so the DLLs after it cannot find them by value.
const SLOTS_VAR: &str = "ERMODS_GFX_OPENER_SLOTS";

/// Process-wide lock so two DLLs do not read and repoint the slots at the same time.
struct InstallLock(*mut c_void);

impl InstallLock {
    fn acquire() -> Self {
        // SAFETY: plain Win32 calls with a NUL-terminated name and the handle this struct owns.
        let name = format!("Local\\ERMods_GfxHook_{}\0", unsafe { GetCurrentProcessId() });
        // SAFETY: as above.
        let handle = unsafe { CreateMutexA(std::ptr::null(), 0, name.as_ptr()) };
        if !handle.is_null() {
            // SAFETY: as above.
            unsafe { WaitForSingleObject(handle, 10_000) };
        }
        Self(handle)
    }
}

impl Drop for InstallLock {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: the mutex this struct owns, acquired in `acquire`.
            unsafe { ReleaseMutex(self.0) };
        }
    }
}

/// The slots another DLL of ours published for this `opener`, empty when there are none.
fn published_slots(opener: usize) -> Vec<*mut usize> {
    let Ok(text) = std::env::var(SLOTS_VAR) else { return Vec::new() };
    let Some((owner, list)) = text.split_once(':') else { return Vec::new() };
    if usize::from_str_radix(owner, 16) != Ok(opener) {
        return Vec::new();
    }
    list.split(',')
        .filter_map(|slot| usize::from_str_radix(slot, 16).ok())
        .map(|slot| slot as *mut usize)
        .collect()
}

fn publish_slots(opener: usize, slots: &[*mut usize]) {
    let list: Vec<String> = slots.iter().map(|&slot| format!("{:X}", slot as usize)).collect();
    let value = format!("{opener:X}:{}\0", list.join(","));
    let name = format!("{SLOTS_VAR}\0");
    // SAFETY: both strings are NUL-terminated.
    unsafe { SetEnvironmentVariableA(name.as_ptr(), value.as_ptr()) };
}

/// Repoints the opener's slot so the movie whose path ends in `file_name`
/// (case-insensitive) is replaced by `edit(original bytes)` when it is `Some`.
/// A file that a mod loader serves under a path of its own (`\\me3...`) has lost
/// its name, so there the movie is the one whose bytes contain `marker`.
/// One movie per DLL. DLLs of ours that edit the same movie chain: each edits what the one before left.
/// `false` (nothing changed) if the opener or its slot isn't found, or this was called before.
pub fn install(file_name: &'static str, marker: &'static [u8], edit: Edit) -> bool {
    if TARGET.set(Target { file_name, marker, edit }).is_err() {
        return false;
    }
    let Some(anchor) = memscan::wait_for_pattern_in_module(
        OPENER_AOB,
        Duration::from_millis(500),
        Duration::from_secs(60),
    ) else {
        logger::error("Movie: file opener pattern not found");
        return false;
    };
    let opener = anchor.wrapping_sub(OPENER_OFFSET) as usize;
    let _lock = InstallLock::acquire();
    let mut slots = published_slots(opener);
    if slots.is_empty() {
        slots = memscan::find_data_pointers(opener);
        if slots.is_empty() {
            logger::error(
                "Movie: file opener slot not found (an older build of another mod may hold it)",
            );
            return false;
        }
        publish_slots(opener, &slots);
    }
    let Some(previous) = memscan::read_safe::<usize>(slots[0] as usize) else {
        logger::error("Movie: file opener slot unreadable");
        return false;
    };
    ORIGINAL.store(previous, Ordering::Release);
    let detour = open_detour as OpenFn as usize;
    let swapped = slots
        .iter()
        .filter(|&&slot| memscan::read_safe::<usize>(slot as usize) == Some(previous))
        // SAFETY: `slot` is an aligned pointer-sized cell of the exe image.
        .filter(|&&slot| unsafe {
            codepatch::overwrite_bytes(slot as *mut u8, &detour.to_le_bytes())
        })
        .count();
    let chained = if previous == opener { "" } else { ", after another hook" };
    logger::log(&format!("Movie: opener slot swapped ({swapped}/{}{chained})", slots.len()));
    swapped > 0
}

unsafe extern "system" fn open_detour(
    opener: *mut c_void,
    path: *const u8,
    flags: i32,
    mode: i32,
) -> *mut c_void {
    // SAFETY: `ORIGINAL` holds what the slots held before `install` swapped them, stored before any was.
    let original: OpenFn = unsafe { std::mem::transmute(ORIGINAL.load(Ordering::Acquire)) };
    // SAFETY: the arguments are the ones the game passed to this call.
    let file = unsafe { original(opener, path, flags, mode) };
    // A panic must not unwind into the game.
    // SAFETY: `file` and `path` are what the opener returned and was given.
    let _ = catch_unwind(AssertUnwindSafe(|| unsafe { replace_movie(file, path) }));
    file
}

/// Prefix of the paths me3 gives a file it serves from a mod folder.
const LOADER_PREFIX: &[u8] = b"\\\\me3";

/// Whether the NUL-terminated `path` starts with `prefix`.
unsafe fn path_starts_with(path: *const u8, prefix: &[u8]) -> bool {
    // SAFETY: the opener's paths are NUL-terminated, so a prefix that differs stops at the NUL.
    !path.is_null() && prefix.iter().enumerate().all(|(i, &b)| unsafe { *path.add(i) } == b)
}

/// Whether the NUL-terminated `path` ends with `name`, ignoring case.
unsafe fn path_ends_with(path: *const u8, name: &str) -> bool {
    if path.is_null() {
        return false;
    }
    let mut len = 0;
    // SAFETY: the game passes a NUL-terminated string; capped well past any real path.
    while len < 1024 && unsafe { *path.add(len) } != 0 {
        len += 1;
    }
    // SAFETY: the loop above found `len` readable bytes before the terminator.
    let path = unsafe { std::slice::from_raw_parts(path, len) };
    path.len() >= name.len()
        && path[path.len() - name.len()..].eq_ignore_ascii_case(name.as_bytes())
}

unsafe fn replace_movie(file: *mut c_void, path: *const u8) {
    let Some(target) = TARGET.get() else { return };
    // SAFETY: `path` is the NUL-terminated string the game passed to the opener.
    // SAFETY: same string as above.
    let by_name = unsafe { path_ends_with(path, target.file_name) };
    // SAFETY: same string as above.
    if file.is_null() || !(by_name || unsafe { path_starts_with(path, LOADER_PREFIX) }) {
        return;
    }
    let base = file as usize;
    if conflict::readable_len(base) < FILE_OBJECT_LEN {
        logger::warn(&format!("Movie: {} opened, file object unreadable", target.file_name));
        return;
    }
    let field = |offset: usize| (base + offset) as *mut u8;
    // SAFETY: the object is readable for `FILE_OBJECT_LEN` bytes (checked above).
    let (refcount, bytes, length, cursor, valid) = unsafe {
        (
            (field(FILE_REFCOUNT) as *const u32).read_unaligned(),
            (field(FILE_BYTES) as *const usize).read_unaligned(),
            (field(FILE_LENGTH) as *const i32).read_unaligned(),
            (field(FILE_CURSOR) as *const i32).read_unaligned(),
            field(FILE_VALID).read(),
        )
    };
    let length = length.max(0) as usize;
    if refcount == 0
        || cursor != 0
        || valid != 1
        || !(12..=MAX_MOVIE_LEN).contains(&length)
        || conflict::readable_len(bytes) < length
    {
        logger::warn(&format!(
            "Movie: {} left unchanged, unexpected file object",
            target.file_name
        ));
        return;
    }
    if !by_name {
        // SAFETY: `bytes..bytes + length` is readable, checked above.
        let source = unsafe { std::slice::from_raw_parts(bytes as *const u8, length) };
        if !source.windows(target.marker.len()).any(|w| w == target.marker) {
            return;
        }
    }
    let _guard = PUBLISH.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let replacement = REPLACEMENT.get_or_init(|| {
        // SAFETY: `bytes..bytes + length` is readable, checked above.
        let source = unsafe { std::slice::from_raw_parts(bytes as *const u8, length) };
        let Some(edited) = (target.edit)(source) else {
            logger::log(&format!("Movie: {} needs no edit", target.file_name));
            return None;
        };
        logger::log(&format!(
            "Movie: {} patched ({length} -> {} bytes)",
            target.file_name,
            edited.len()
        ));
        Some(&*Box::leak(edited.into_boxed_slice()))
    });
    let Some(replacement) = replacement else { return };
    if replacement.len() > i32::MAX as usize {
        return;
    }
    // SAFETY: same object, fields checked above; the replacement is never freed.
    unsafe {
        (field(FILE_BYTES) as *mut usize).write_unaligned(replacement.as_ptr() as usize);
        (field(FILE_LENGTH) as *mut i32).write_unaligned(replacement.len() as i32);
    }
}
