//! What the desktop looks like to this process: its grants (read-only checks;
//! nothing here may ask for one, because the request APIs raise a dialog), the
//! console session, the frontmost app, who holds Secure Input, the input
//! source and the displays.

use super::ax::rect_json;
use super::cf::{self, Cf};
use super::ffi::*;
use crate::Outcome;
use serde_json::{json, Value};
use std::collections::HashMap;
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

/// A display's top insets, in points: `safe_area_top` is the camera housing's
/// (`NSScreen.safeAreaInsets.top`, non-zero only on a notched display) and
/// `menu_bar_inset` what AppKit reserves for the menu bar
/// (`frame.maxY - visibleFrame.maxY`). A full-screen window on a notched
/// display sits below the menu bar, not the housing (plan 075 §D3).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct ScreenInsets {
    safe_area_top: f64,
    menu_bar_inset: f64,
    unavailable: Option<&'static str>,
}

fn display_entry(id: u32, bounds: CGRect, main: bool, insets: ScreenInsets) -> Value {
    let mut entry = json!({
        "id": id,
        "main": main,
        "bounds": rect_json(bounds),
        "safe_area_top": insets.safe_area_top,
        "menu_bar_inset": insets.menu_bar_inset,
    });
    if let Some(reason) = insets.unavailable {
        entry["insets"] = json!(reason);
    }
    entry
}

/// Whether this x86_64 process runs under Rosetta, where `NSRect` and
/// `NSEdgeInsets` come back through the `_stret` convention this helper does
/// not implement.
#[cfg(target_arch = "x86_64")]
fn translated() -> bool {
    let mut value = 0i32;
    let mut len = std::mem::size_of::<i32>();
    // SAFETY: writes at most `len` bytes into `value`.
    let rc = unsafe {
        sysctlbyname(
            c"sysctl.proc_translated".as_ptr(),
            (&mut value as *mut i32).cast(),
            &mut len,
            std::ptr::null(),
            0,
        )
    };
    rc == 0 && value == 1
}

#[cfg(target_arch = "x86_64")]
fn screen_insets() -> Result<HashMap<u32, ScreenInsets>, &'static str> {
    if translated() {
        Err("unavailable-translated")
    } else {
        Ok(HashMap::new())
    }
}

#[cfg(target_arch = "aarch64")]
#[repr(C)]
#[derive(Clone, Copy)]
struct NSEdgeInsets {
    top: f64,
    left: f64,
    bottom: f64,
    right: f64,
}

/// Every `NSScreen`'s insets by `CGDirectDisplayID`
/// (`deviceDescription["NSScreenNumber"]`).
#[cfg(target_arch = "aarch64")]
fn screen_insets() -> Result<HashMap<u32, ScreenInsets>, &'static str> {
    type Id = *const c_void;
    type Sel = *const c_void;
    type SendId = unsafe extern "C" fn(Id, Sel) -> Id;
    type SendCount = unsafe extern "C" fn(Id, Sel) -> usize;
    type SendIndex = unsafe extern "C" fn(Id, Sel, usize) -> Id;
    type SendObject = unsafe extern "C" fn(Id, Sel, Id) -> Id;
    type SendSel = unsafe extern "C" fn(Id, Sel, Sel) -> bool;
    type SendU32 = unsafe extern "C" fn(Id, Sel) -> u32;
    type SendInsets = unsafe extern "C" fn(Id, Sel) -> NSEdgeInsets;
    type SendRect = unsafe extern "C" fn(Id, Sel) -> CGRect;
    // SAFETY: objc_msgSend called through the exact prototype of each message:
    // on arm64 an NSRect and an NSEdgeInsets (four f64 each, homogeneous
    // floating-point aggregates) come back in d0-d3, so no `_stret` variant
    // applies. A message to nil answers zero. Every object is autoreleased and
    // stays inside the pool popped below; `safeAreaInsets` is only sent to a
    // screen that responds to it (macOS 12).
    unsafe {
        let msg = objc_msgSend as unsafe extern "C" fn();
        let send_id = std::mem::transmute_copy::<_, SendId>(&msg);
        let send_count = std::mem::transmute_copy::<_, SendCount>(&msg);
        let send_index = std::mem::transmute_copy::<_, SendIndex>(&msg);
        let send_object = std::mem::transmute_copy::<_, SendObject>(&msg);
        let send_sel = std::mem::transmute_copy::<_, SendSel>(&msg);
        let send_u32 = std::mem::transmute_copy::<_, SendU32>(&msg);
        let send_insets = std::mem::transmute_copy::<_, SendInsets>(&msg);
        let send_rect = std::mem::transmute_copy::<_, SendRect>(&msg);
        let sel = |name: &std::ffi::CStr| sel_registerName(name.as_ptr());

        let pool = objc_autoreleasePoolPush();
        let key = cf::string("NSScreenNumber");
        let screens = send_id(objc_getClass(c"NSScreen".as_ptr()), sel(c"screens"));
        let mut found = HashMap::new();
        for index in 0..send_count(screens, sel(c"count")) {
            let screen = send_index(screens, sel(c"objectAtIndex:"), index);
            let description = send_id(screen, sel(c"deviceDescription"));
            let number = send_object(description, sel(c"objectForKey:"), key.ptr());
            if number.is_null() {
                continue;
            }
            let id = send_u32(number, sel(c"unsignedIntValue"));
            let frame = send_rect(screen, sel(c"frame"));
            let visible = send_rect(screen, sel(c"visibleFrame"));
            let menu_bar_inset =
                (frame.origin.y + frame.size.height) - (visible.origin.y + visible.size.height);
            let insets = if send_sel(screen, sel(c"respondsToSelector:"), sel(c"safeAreaInsets")) {
                ScreenInsets {
                    safe_area_top: send_insets(screen, sel(c"safeAreaInsets")).top,
                    menu_bar_inset,
                    unavailable: None,
                }
            } else {
                ScreenInsets {
                    safe_area_top: 0.0,
                    menu_bar_inset,
                    unavailable: Some("unavailable"),
                }
            };
            found.insert(id, insets);
        }
        objc_autoreleasePoolPop(pool);
        Ok(found)
    }
}

/// The active displays' bounds, in the global top-left points AX frames
/// and CGEvent use, so a window's frame can be checked against them, with
/// each one's top insets.
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
    let screens = screen_insets();
    ids[..(count as usize).min(MAX)]
        .iter()
        .map(|&id| {
            // SAFETY: a plain display query.
            let bounds = unsafe { CGDisplayBounds(id) };
            let insets = match &screens {
                Ok(found) => found.get(&id).copied().unwrap_or_default(),
                Err(reason) => ScreenInsets {
                    unavailable: Some(reason),
                    ..ScreenInsets::default()
                },
            };
            display_entry(id, bounds, id == main, insets)
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

#[cfg(test)]
mod tests {
    use super::*;

    const BOUNDS: CGRect = CGRect {
        origin: CGPoint { x: 0.0, y: 0.0 },
        size: CGSize {
            width: 1470.0,
            height: 956.0,
        },
    };

    #[test]
    fn a_notched_display_reports_both_insets() {
        let entry = display_entry(
            1,
            BOUNDS,
            true,
            ScreenInsets {
                safe_area_top: 32.0,
                menu_bar_inset: 33.0,
                unavailable: None,
            },
        );
        assert_eq!(entry["safe_area_top"], 32.0);
        assert_eq!(entry["menu_bar_inset"], 33.0);
        assert_eq!(entry["bounds"]["height"], 956.0);
        assert_eq!(entry["main"], true);
        assert!(entry.get("insets").is_none());
    }

    #[test]
    fn a_display_nsscreen_does_not_list_gets_no_insets() {
        let found: HashMap<u32, ScreenInsets> = HashMap::new();
        let entry = display_entry(7, BOUNDS, false, found.get(&7).copied().unwrap_or_default());
        assert_eq!(entry["safe_area_top"], 0.0);
        assert_eq!(entry["menu_bar_inset"], 0.0);
        assert!(entry.get("insets").is_none());
    }

    #[test]
    fn an_unavailable_reading_is_named_not_reported_as_zero_alone() {
        let entry = display_entry(
            1,
            BOUNDS,
            true,
            ScreenInsets {
                unavailable: Some("unavailable-translated"),
                ..ScreenInsets::default()
            },
        );
        assert_eq!(entry["insets"], "unavailable-translated");
        assert_eq!(entry["safe_area_top"], 0.0);
    }
}
