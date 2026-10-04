//! A listen-only event tap: what a keylogger would see. Secure Input blinds it
//! (§2.12), which is how the harness observes Secure Keyboard Entry from the
//! outside. It writes `<outdir>/ready` once it is live, appends each event to
//! `<outdir>/events.jsonl` as it arrives, and stops early when `<outdir>/stop`
//! appears.

use super::cf::Cf;
use super::ffi::*;
use crate::{write_atomically, Failure, Outcome};
use serde_json::{json, Value};
use std::ffi::c_void;
use std::io::Write;
use std::path::Path;
use std::sync::atomic::{AtomicPtr, AtomicU32, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

#[derive(Clone, Copy)]
struct TapEvent {
    kind: u32,
    keycode: i64,
    flags: u64,
    millis: u64,
}

impl TapEvent {
    fn to_json(self) -> Value {
        let kind = if self.kind == kCGEventKeyDown {
            "key_down".to_string()
        } else if self.kind == kCGEventKeyUp {
            "key_up".to_string()
        } else if self.kind == kCGEventFlagsChanged {
            "flags_changed".to_string()
        } else {
            format!("other:{}", self.kind)
        };
        json!({"type": kind, "keycode": self.keycode, "flags": self.flags, "t_ms": self.millis})
    }
}

static EVENTS: Mutex<Vec<TapEvent>> = Mutex::new(Vec::new());
static PORT: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());
static REENABLED: AtomicU32 = AtomicU32::new(0);
static STARTED: OnceLock<Instant> = OnceLock::new();

/// Slice of run-loop time between checks of the stop file and the clock.
const SLICE: f64 = 0.05;

extern "C" fn callback(
    _proxy: CGEventTapProxy,
    kind: u32,
    event: CGEventRef,
    _user: *mut c_void,
) -> CGEventRef {
    if kind == kCGEventTapDisabledByTimeout || kind == kCGEventTapDisabledByUserInput {
        let port = PORT.load(Ordering::SeqCst);
        if !port.is_null() {
            // SAFETY: `port` is the live tap; it is cleared before release.
            unsafe { CGEventTapEnable(port, true) };
        }
        REENABLED.fetch_add(1, Ordering::SeqCst);
        return event;
    }
    // SAFETY: `event` is the live event the tap is delivering.
    let (keycode, flags) = unsafe {
        (
            CGEventGetIntegerValueField(event, kCGKeyboardEventKeycode),
            CGEventGetFlags(event),
        )
    };
    let millis = STARTED
        .get()
        .map_or(0, |started| started.elapsed().as_millis() as u64);
    if let Ok(mut events) = EVENTS.lock() {
        events.push(TapEvent {
            kind,
            keycode,
            flags,
            millis,
        });
    }
    event
}

pub fn listen(duration: Duration, outdir: Option<&Path>) -> Outcome {
    // SAFETY: a read-only check.
    if !unsafe { CGPreflightListenEventAccess() } {
        return Err(Failure::Unavailable(
            "listening to events is not granted (Input Monitoring)".into(),
        ));
    }
    let started = *STARTED.get_or_init(Instant::now);
    let mask = (1u64 << kCGEventKeyDown) | (1u64 << kCGEventKeyUp) | (1u64 << kCGEventFlagsChanged);
    // SAFETY: the tap and its source are adopted once and outlive the loop;
    // PORT is cleared before they drop.
    let (port, source) = unsafe {
        let port = Cf::owned(CGEventTapCreate(
            kCGHIDEventTap,
            kCGHeadInsertEventTap,
            kCGEventTapOptionListenOnly,
            mask,
            callback,
            std::ptr::null_mut(),
        ))
        .ok_or_else(|| {
            Failure::Unavailable("CGEventTapCreate returned null (Input Monitoring?)".into())
        })?;
        let source = Cf::owned(CFMachPortCreateRunLoopSource(
            std::ptr::null(),
            port.ptr(),
            0,
        ))
        .ok_or_else(|| Failure::Failed("CFMachPortCreateRunLoopSource failed".into()))?;
        PORT.store(port.ptr().cast_mut(), Ordering::SeqCst);
        CFRunLoopAddSource(CFRunLoopGetCurrent(), source.ptr(), kCFRunLoopCommonModes);
        CGEventTapEnable(port.ptr(), true);
        (port, source)
    };

    let mut log = match outdir {
        Some(dir) => Some(
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(dir.join("events.jsonl"))
                .map_err(|error| Failure::Failed(format!("events.jsonl: {error}")))?,
        ),
        None => None,
    };
    if let Some(dir) = outdir {
        write_atomically(
            &dir.join("ready"),
            &format!("{}\n", json!({"pid": std::process::id()})),
        )
        .map_err(|error| Failure::Failed(format!("ready: {error}")))?;
    }

    let stop = outdir.map(|dir| dir.join("stop"));
    let end = Instant::now() + duration;
    let mut flushed = 0;
    let mut stopped_by = "seconds";
    loop {
        // SAFETY: runs this thread's run loop, which holds the tap source.
        unsafe { CFRunLoopRunInMode(kCFRunLoopDefaultMode, SLICE, 0) };
        flushed = flush(&mut log, flushed);
        if stop.as_deref().is_some_and(Path::exists) {
            stopped_by = "stop-file";
            break;
        }
        if Instant::now() >= end {
            break;
        }
    }
    flush(&mut log, flushed);

    // SAFETY: the tap is live until invalidated here; PORT is cleared first.
    let enabled_at_end = unsafe {
        let enabled = CGEventTapIsEnabled(port.ptr());
        PORT.store(std::ptr::null_mut(), Ordering::SeqCst);
        CGEventTapEnable(port.ptr(), false);
        CFMachPortInvalidate(port.ptr());
        enabled
    };
    drop(source);
    drop(port);

    let events: Vec<TapEvent> = EVENTS
        .lock()
        .map(|events| events.clone())
        .unwrap_or_default();
    Ok(json!({
        "listened_ms": started.elapsed().as_millis() as u64,
        "stopped_by": stopped_by,
        "enabled_at_end": enabled_at_end,
        "reenabled": REENABLED.load(Ordering::SeqCst),
        "key_downs": events
            .iter()
            .filter(|event| event.kind == kCGEventKeyDown)
            .map(|event| event.keycode)
            .collect::<Vec<_>>(),
        "events": events.iter().map(|event| event.to_json()).collect::<Vec<_>>(),
    }))
}

/// Append the events seen since `from` to the log; returns the new count.
fn flush(log: &mut Option<std::fs::File>, from: usize) -> usize {
    let Ok(events) = EVENTS.lock() else {
        return from;
    };
    if let Some(file) = log.as_mut() {
        for event in &events[from.min(events.len())..] {
            let _ = writeln!(file, "{}", event.to_json());
        }
        let _ = file.flush();
    }
    events.len()
}
