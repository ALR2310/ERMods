//! "A new version is out" notice. One small HTTPS request reads `version.json` from the project's
//! public repository when the mod starts, in the background. The menu host takes title-screen text
//! (the only place the player sees the notice) while the game starts, so [`notice`] waits briefly
//! for the answer before the menu is built. Without the host the result is only logged.

use std::ffi::c_void;
use std::sync::{Condvar, Mutex};
use std::time::Duration;

use crate::logger;

const HOST: &str = "raw.githubusercontent.com";
const PATH: &str = "/ALR2310/ERMods/main/version.json";
/// Per step of the request; [`notice`] waits much less than that.
const TIMEOUT_MS: i32 = 5000;
/// `version.json` lists a few mods; anything larger is not ours.
const MAX_BODY: usize = 16 * 1024;
const MAX_VERSION_LEN: usize = 20;

#[link(name = "winhttp")]
unsafe extern "system" {
    fn WinHttpOpen(
        agent: *const u16,
        access_type: u32,
        proxy: *const u16,
        bypass: *const u16,
        flags: u32,
    ) -> *mut c_void;
    fn WinHttpSetTimeouts(
        session: *mut c_void,
        resolve: i32,
        connect: i32,
        send: i32,
        receive: i32,
    ) -> i32;
    fn WinHttpConnect(
        session: *mut c_void,
        server: *const u16,
        port: u16,
        reserved: u32,
    ) -> *mut c_void;
    fn WinHttpOpenRequest(
        connection: *mut c_void,
        verb: *const u16,
        object: *const u16,
        version: *const u16,
        referrer: *const u16,
        accept_types: *const *const u16,
        flags: u32,
    ) -> *mut c_void;
    fn WinHttpSendRequest(
        request: *mut c_void,
        headers: *const u16,
        headers_len: u32,
        optional: *const c_void,
        optional_len: u32,
        total_len: u32,
        context: usize,
    ) -> i32;
    fn WinHttpReceiveResponse(request: *mut c_void, reserved: *mut c_void) -> i32;
    fn WinHttpQueryHeaders(
        request: *mut c_void,
        info_level: u32,
        name: *const u16,
        buffer: *mut c_void,
        buffer_len: *mut u32,
        index: *mut u32,
    ) -> i32;
    fn WinHttpReadData(
        request: *mut c_void,
        buffer: *mut c_void,
        to_read: u32,
        read: *mut u32,
    ) -> i32;
    fn WinHttpCloseHandle(handle: *mut c_void) -> i32;
}

const ACCESS_DEFAULT_PROXY: u32 = 0;
const HTTPS_PORT: u16 = 443;
const FLAG_SECURE: u32 = 0x0080_0000;
const QUERY_STATUS_CODE_NUMBER: u32 = 19 | 0x2000_0000;

/// Closes a WinHTTP handle when dropped.
struct Handle(*mut c_void);

impl Handle {
    fn new(raw: *mut c_void, what: &str) -> Result<Handle, String> {
        if raw.is_null() { Err(format!("{what} failed")) } else { Ok(Handle(raw)) }
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        // SAFETY: the handle came from a WinHTTP open call and is closed once.
        unsafe { WinHttpCloseHandle(self.0) };
    }
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

/// The body of `https://{HOST}{PATH}`, or why it could not be read.
fn download() -> Result<Vec<u8>, String> {
    let agent = wide("ERMods update check");
    let host = wide(HOST);
    let verb = wide("GET");
    let path = wide(PATH);
    // SAFETY: every string is NUL-terminated and outlives the calls; handles are closed by `Handle`.
    unsafe {
        let session = Handle::new(
            WinHttpOpen(
                agent.as_ptr(),
                ACCESS_DEFAULT_PROXY,
                std::ptr::null(),
                std::ptr::null(),
                0,
            ),
            "WinHttpOpen",
        )?;
        WinHttpSetTimeouts(session.0, TIMEOUT_MS, TIMEOUT_MS, TIMEOUT_MS, TIMEOUT_MS);
        let connection =
            Handle::new(WinHttpConnect(session.0, host.as_ptr(), HTTPS_PORT, 0), "WinHttpConnect")?;
        let request = Handle::new(
            WinHttpOpenRequest(
                connection.0,
                verb.as_ptr(),
                path.as_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                std::ptr::null(),
                FLAG_SECURE,
            ),
            "WinHttpOpenRequest",
        )?;
        if WinHttpSendRequest(request.0, std::ptr::null(), 0, std::ptr::null(), 0, 0, 0) == 0
            || WinHttpReceiveResponse(request.0, std::ptr::null_mut()) == 0
        {
            return Err("request failed".into());
        }
        let mut status: u32 = 0;
        let mut len = std::mem::size_of::<u32>() as u32;
        if WinHttpQueryHeaders(
            request.0,
            QUERY_STATUS_CODE_NUMBER,
            std::ptr::null(),
            &mut status as *mut u32 as *mut c_void,
            &mut len,
            std::ptr::null_mut(),
        ) == 0
            || status != 200
        {
            return Err(format!("HTTP status {status}"));
        }
        let mut body = Vec::new();
        let mut chunk = [0u8; 2048];
        loop {
            let mut read = 0u32;
            if WinHttpReadData(
                request.0,
                chunk.as_mut_ptr() as *mut c_void,
                chunk.len() as u32,
                &mut read,
            ) == 0
            {
                return Err("read failed".into());
            }
            if read == 0 {
                return Ok(body);
            }
            body.extend_from_slice(&chunk[..read as usize]);
            if body.len() > MAX_BODY {
                return Err("response too large".into());
            }
        }
    }
}

/// `1.2.3`-style versions only: the text is shown on screen and kept in a file.
fn valid_version(text: &str) -> bool {
    !text.is_empty()
        && text.len() <= MAX_VERSION_LEN
        && text.split('.').all(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()))
}

fn parts(version: &str) -> Vec<u64> {
    version.split('.').map(|part| part.parse().unwrap_or(0)).collect()
}

/// Whether `latest` is a higher version than `current`.
pub fn is_newer(latest: &str, current: &str) -> bool {
    let (mut a, mut b) = (parts(latest), parts(current));
    let len = a.len().max(b.len());
    a.resize(len, 0);
    b.resize(len, 0);
    a > b
}

/// The version of `key` in the body of `version.json`.
fn latest_in(body: &[u8], key: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_slice(body).ok()?;
    let version = value.get(key)?.as_str()?;
    valid_version(version).then(|| version.to_string())
}

/// `None` until the request has finished, then the line for the title screen (or `None` again
/// inside when the mod is up to date or the request failed).
static RESULT: (Mutex<Option<Option<String>>>, Condvar) = (Mutex::new(None), Condvar::new());

/// Starts the check in the background; call once, as early as possible.
pub fn spawn(key: &'static str, display_name: &'static str, current: &'static str) {
    std::thread::spawn(move || {
        let latest = download()
            .and_then(|body| {
                latest_in(&body, key).ok_or_else(|| format!("no version for {key} in version.json"))
            })
            .inspect_err(|reason| logger::log(&format!("Update check failed: {reason}.")))
            .ok();
        let notice = latest.and_then(|latest| {
            if is_newer(&latest, current) {
                logger::log(&format!(
                    "Update check: version {latest} is available (you have {current})."
                ));
                Some(format!("{display_name} {latest} is available (you have {current})"))
            } else {
                logger::log(&format!("Update check: up to date ({current})."));
                None
            }
        });
        *RESULT.0.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(notice);
        RESULT.1.notify_all();
    });
}

/// The line for the title screen when a newer version exists, waiting at most `wait` for the check
/// started by [`spawn`].
pub fn notice(wait: Duration) -> Option<String> {
    let guard = RESULT.0.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let (guard, _) = RESULT
        .1
        .wait_timeout_while(guard, wait, |result| result.is_none())
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    guard.clone().flatten()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_compare_by_number() {
        assert!(is_newer("1.10.0", "1.9.9"));
        assert!(is_newer("2.0", "1.9.9"));
        assert!(!is_newer("1.6.0", "1.6.0"));
        assert!(!is_newer("1.5.9", "1.6.0"));
        assert!(!is_newer("1.6", "1.6.0"));
    }

    #[test]
    fn only_plain_versions_are_accepted() {
        assert!(valid_version("1.6.0"));
        assert!(!valid_version(""));
        assert!(!valid_version("1..0"));
        assert!(!valid_version("1.6.0-beta"));
        assert!(!valid_version("1.6.0 <script>"));
        assert!(!valid_version("1.2.3.4.5.6.7.8.9.10.11"));
    }

    #[test]
    fn the_version_of_a_mod_is_read_from_the_json() {
        let body = br#"{ "spiritmultiplier": "1.7.0", "autoregen": "2.6.2" }"#;
        assert_eq!(latest_in(body, "autoregen").as_deref(), Some("2.6.2"));
        assert_eq!(latest_in(body, "missing"), None);
        assert_eq!(latest_in(br#"{ "x": "evil text" }"#, "x"), None);
        assert_eq!(latest_in(b"not json", "x"), None);
    }
}
