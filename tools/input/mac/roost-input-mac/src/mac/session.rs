//! What the desktop looks like to this process: its grants (read-only checks;
//! nothing here may ask for one, because the request APIs raise a dialog), the
//! console session, the frontmost app, who holds Secure Input, the input
//! source and the displays.

use super::ax::rect_json;
use super::cf::{self, Cf};
use super::ffi::*;
use crate::Outcome;
use serde_json::{json, Value};
use std::ffi::c_void;

pub struct Frontmost {
    pub pid: i32,
    pub bundle_id: Option<String>,
}

/// The frontmost app's pid, asked anew on every call. `NSWorkspace`'s
/// `frontmostApplication` is refreshed by run-loop notifications this process
/// never runs, so after its first read it goes stale (on the harness Mac it
/// kept naming an app that had quit), and the system-wide
/// `AXFocusedApplication` answers `kAXErrorCannotComplete` there. The Process
/// Manager's front process, deprecated but present, is live.
pub fn front_pid() -> Option<i32> {
    let mut psn = ProcessSerialNumber::default();
    let mut pid = 0;
    // SAFETY: both write only into the locals passed.
    unsafe { (GetFrontProcess(&mut psn) == 0 && GetProcessPID(&psn, &mut pid) == 0).then_some(pid) }
}

pub fn frontmost() -> Option<Frontmost> {
    let pid = front_pid()?;
    Some(Frontmost {
        pid,
        bundle_id: bundle_id(pid),
    })
}

/// `[NSRunningApplication runningApplicationWithProcessIdentifier:pid]
/// .bundleIdentifier`, through the runtime.
fn bundle_id(pid: i32) -> Option<String> {
    type SendPid = unsafe extern "C" fn(*const c_void, *const c_void, i32) -> *const c_void;
    type SendId = unsafe extern "C" fn(*const c_void, *const c_void) -> *const c_void;
    // SAFETY: objc_msgSend called through the exact prototype of each message;
    // a message to nil answers nil. Every object stays inside the autorelease
    // pool popped below.
    unsafe {
        let send_pid = std::mem::transmute::<unsafe extern "C" fn(), SendPid>(
            objc_msgSend as unsafe extern "C" fn(),
        );
        let send_id = std::mem::transmute::<unsafe extern "C" fn(), SendId>(
            objc_msgSend as unsafe extern "C" fn(),
        );
        let pool = objc_autoreleasePoolPush();
        let app = send_pid(
            objc_getClass(c"NSRunningApplication".as_ptr()),
            sel_registerName(c"runningApplicationWithProcessIdentifier:".as_ptr()),
            pid,
        );
        let bundle = cf::to_string(send_id(app, sel_registerName(c"bundleIdentifier".as_ptr())));
        objc_autoreleasePoolPop(pool);
        bundle
    }
}

pub fn input_source() -> Option<String> {
    // SAFETY: a Copy result adopted once; the property is borrowed from it.
    unsafe {
        let source = Cf::owned(TISCopyCurrentKeyboardInputSource())?;
        cf::to_string(TISGetInputSourceProperty(
            source.ptr(),
            kTISPropertyInputSourceID,
        ))
    }
}

/// `CGSessionCopyCurrentDictionary`: null for a process outside the GUI login
/// session (an ssh child), which is itself the answer "no console here".
fn session() -> Value {
    // SAFETY: a Copy result adopted once; values are borrowed from it.
    unsafe {
        let Some(dict) = Cf::owned(CGSessionCopyCurrentDictionary()) else {
            return json!({"available": false});
        };
        let get = |key: &str| cf::dict_get(dict.ptr(), key);
        json!({
            "available": true,
            "locked": cf::to_bool(get("CGSSessionScreenIsLocked")).unwrap_or(false),
            "on_console": cf::to_bool(get("kCGSSessionOnConsoleKey")),
            "login_done": cf::to_bool(get("kCGSessionLoginDoneKey")),
            "secure_input_pid": cf::to_i64(get("kCGSSessionSecureInputPID")),
        })
    }
}

/// This user's on-console entry of the registry's `IOConsoleUsers`, which is
/// what `ioreg` prints: the lock flag and `kCGSSessionSecureInputPID`.
fn console() -> Value {
    // SAFETY: registry calls with their results released; dictionary values
    // are borrowed from the retained array elements.
    unsafe {
        let root = IORegistryGetRootEntry(0);
        if root == 0 {
            return json!({"available": false});
        }
        let key = cf::string("IOConsoleUsers");
        let users = Cf::owned(IORegistryEntryCreateCFProperty(
            root,
            key.ptr(),
            std::ptr::null(),
            0,
        ));
        IOObjectRelease(root);
        let Some(users) = users else {
            return json!({"available": false});
        };
        let uid = i64::from(getuid());
        let entries = cf::array_items(users.ptr());
        let mine = entries.iter().find(|entry| {
            cf::is_dict(entry.ptr())
                && cf::to_i64(cf::dict_get(entry.ptr(), "kCGSSessionUserIDKey")) == Some(uid)
                && cf::to_bool(cf::dict_get(entry.ptr(), "kCGSSessionOnConsoleKey")) == Some(true)
        });
        match mine {
            None => json!({"available": true, "on_console": false}),
            Some(entry) => json!({
                "available": true,
                "on_console": true,
                "locked": cf::to_bool(cf::dict_get(entry.ptr(), "CGSSessionScreenIsLocked")).unwrap_or(false),
                "secure_input_pid": cf::to_i64(cf::dict_get(entry.ptr(), "kCGSSessionSecureInputPID")),
            }),
        }
    }
}

/// The console as a posting command must find it before every event: the
/// GUI session's own dictionary and the registry's console entry, read fresh.
/// A process outside the GUI session sees no dictionary, which reads as off
/// the console.
pub fn posting_session() -> crate::input::Session {
    let session = session();
    let console = console();
    let flag = |value: &Value, key: &str| value.get(key).and_then(Value::as_bool);
    crate::input::Session {
        locked: flag(&session, "locked").unwrap_or(false)
            || flag(&console, "locked").unwrap_or(false),
        on_console: flag(&session, "available") == Some(true)
            && flag(&session, "on_console") != Some(false)
            && flag(&console, "on_console") != Some(false),
        secure_input_pid: console
            .get("secure_input_pid")
            .and_then(Value::as_i64)
            .or_else(|| session.get("secure_input_pid").and_then(Value::as_i64)),
    }
}

/// The active displays' bounds, in the global top-left points AX frames
/// and CGEvent use, so a window's frame can be checked against them.
fn displays() -> Value {
    const MAX: usize = 16;
    let mut ids = [0u32; MAX];
    let mut count = 0u32;
    // SAFETY: writes at most MAX ids into `ids` and their number into `count`.
    if unsafe { CGGetActiveDisplayList(MAX as u32, ids.as_mut_ptr(), &mut count) } != 0 {
        return Value::Null;
    }
    // SAFETY: plain display queries.
    let main = unsafe { CGMainDisplayID() };
    ids[..(count as usize).min(MAX)]
        .iter()
        .map(|&id| {
            // SAFETY: a plain display query.
            let bounds = unsafe { CGDisplayBounds(id) };
            json!({"id": id, "main": id == main, "bounds": rect_json(bounds)})
        })
        .collect()
}

pub fn alive(pid: i32) -> bool {
    // SAFETY: signal 0 only checks for existence.
    unsafe { kill(pid, 0) == 0 || std::io::Error::last_os_error().raw_os_error() == Some(1) }
}

pub fn preflight(pid: Option<i32>) -> Outcome {
    // SAFETY: the read-only preflight checks.
    let capabilities = unsafe {
        json!({
            "accessibility": AXIsProcessTrusted() != 0,
            "post_event": CGPreflightPostEventAccess(),
            "listen_event": CGPreflightListenEventAccess(),
            "screen_capture": CGPreflightScreenCaptureAccess(),
        })
    };
    let session = session();
    let console = console();
    // `ioreg` names the frontmost app's pid while anyone holds Secure Input,
    // not the holder's, so this says "someone has it", not who.
    let secure_input_pid = console
        .get("secure_input_pid")
        .filter(|pid| !pid.is_null())
        .or_else(|| session.get("secure_input_pid"))
        .cloned()
        .unwrap_or(Value::Null);
    let front = frontmost();
    let mut report = json!({
        "self_pid": std::process::id(),
        "capabilities": capabilities,
        "session": session,
        "console": console,
        "frontmost": front.as_ref().map(|app| json!({"pid": app.pid, "bundle_id": app.bundle_id})),
        "secure_input_pid": secure_input_pid,
        "input_source": input_source(),
        "displays": displays(),
    });
    if let Some(pid) = pid {
        report["target"] = json!({
            "pid": pid,
            "alive": alive(pid),
            "frontmost": front.as_ref().is_some_and(|app| app.pid == pid),
        });
    }
    Ok(report)
}
