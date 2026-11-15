//! In-place code patches shared by the "Anywhere" features: find an AOB,
//! overwrite bytes, log the outcome.

use std::time::{Duration, Instant};

use shared::{codepatch, logger, memscan};

const SCAN_RETRY_INTERVAL: Duration = Duration::from_millis(500);
const SCAN_TIMEOUT: Duration = Duration::from_secs(60);

/// How long a patch to code that other mods look for by signature waits before it is applied. A
/// mod that scans the code for the original bytes (ErdTools does, 5 s after it loads) finds
/// nothing once they are changed, and fails; waiting lets it scan first.
const OTHER_MODS_SCAN_GRACE: Duration = Duration::from_secs(15);

/// Waits for `pattern` in the game module and overwrites `patched` at `offset` bytes past the
/// match. Another mod may have made the same patch first: the patched bytes then count as
/// applied. Returns whether the code is patched.
pub fn apply(feature: &str, name: &str, pattern: &str, offset: usize, patched: &[u8]) -> bool {
    let already_patched = patched_form(pattern, offset, patched);
    let deadline = Instant::now() + SCAN_TIMEOUT;
    loop {
        if let Some(addr) = memscan::find_pattern_in_module(pattern) {
            // SAFETY: the range is mapped code of the game image and the bytes replace whole instructions.
            if unsafe { codepatch::overwrite_bytes(addr.add(offset), patched) } {
                logger::log(&format!("{feature}: {name} patched at {addr:p}."));
                return true;
            }
            logger::error(&format!("{feature}: {name} patch failed."));
            return false;
        }
        if let Some(addr) = already_patched.as_deref().and_then(memscan::find_pattern_in_module) {
            logger::log(&format!(
                "{feature}: {name} already patched at {addr:p} (by another mod)."
            ));
            return true;
        }
        if Instant::now() >= deadline {
            logger::error(&format!("{feature}: {name} pattern not found."));
            return false;
        }
        std::thread::sleep(SCAN_RETRY_INTERVAL);
    }
}

/// [`apply`] after [`OTHER_MODS_SCAN_GRACE`], for a site that another mod's signature covers.
pub fn apply_after_other_mods(
    feature: &str,
    name: &str,
    pattern: &str,
    offset: usize,
    patched: &[u8],
) -> bool {
    std::thread::sleep(OTHER_MODS_SCAN_GRACE);
    apply(feature, name, pattern, offset, patched)
}

/// `pattern` with the bytes of `patched` put in at `offset`: what the scan would look for once
/// someone else has made the patch. `None` if the patch reaches past the pattern.
fn patched_form(pattern: &str, offset: usize, patched: &[u8]) -> Option<String> {
    let mut tokens: Vec<String> = pattern.split_whitespace().map(str::to_owned).collect();
    let end = offset.checked_add(patched.len())?;
    if end > tokens.len() {
        return None;
    }
    for (token, byte) in tokens[offset..end].iter_mut().zip(patched) {
        *token = format!("{byte:02X}");
    }
    Some(tokens.join(" "))
}

/// Parks the calling feature thread for good (the task it registered keeps
/// running on the game's own schedule).
pub fn park() -> ! {
    loop {
        std::thread::sleep(Duration::from_secs(60));
    }
}

#[cfg(test)]
mod tests {
    use super::patched_form;

    #[test]
    fn the_patched_bytes_replace_the_head_of_the_pattern() {
        assert_eq!(
            patched_form("E8 ?? ?? ?? ?? 84 C0", 0, &[0x48, 0x31, 0xC0, 0x90, 0x90]).as_deref(),
            Some("48 31 C0 90 90 84 C0")
        );
    }

    #[test]
    fn a_patch_in_the_middle_keeps_the_rest() {
        assert_eq!(patched_form("BA 1B 74 10 48", 2, &[0xEB]).as_deref(), Some("BA 1B EB 10 48"));
    }

    #[test]
    fn a_patch_past_the_end_has_no_patched_form() {
        assert_eq!(patched_form("74 2E", 1, &[0xEB, 0x2E]), None);
    }
}
