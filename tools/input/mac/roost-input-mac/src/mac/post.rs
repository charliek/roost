//! The macOS desktop under `input`: real events posted at the HID event tap,
//! and the live queries the per-event checks make.

use super::cf::Cf;
use super::ffi::*;
use super::{ax, session};
use crate::args::{Button, Point};
use crate::input::{button_name, point_json, Desktop, Event, Journal, Session, KEY_GAP, MOUSE_GAP};
use crate::keys::MODIFIERS;
use crate::Failure;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;

pub struct MacDesktop;

/// Where this invocation journals what it holds (`<outdir>/held.json`), set
/// once from `--outdir`; without one nothing is journaled.
pub static JOURNAL: OnceLock<PathBuf> = OnceLock::new();

/// A journal file: another helper's, for `release-held`.
pub struct FileJournal<'a>(pub &'a Path);

impl Journal for FileJournal<'_> {
    fn journal(&self, held: &Value) -> Result<(), Failure> {
        crate::write_atomically(self.0, &format!("{held}\n"))
            .map_err(|error| Failure::Failed(format!("{}: {error}", self.0.display())))
    }

    fn discard_journal(&self) -> Result<(), Failure> {
        match std::fs::remove_file(self.0) {
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
                Err(Failure::Failed(format!("{}: {error}", self.0.display())))
            }
            _ => Ok(()),
        }
    }
}

impl Journal for MacDesktop {
    fn journal(&self, held: &Value) -> Result<(), Failure> {
        JOURNAL
            .get()
            .map_or(Ok(()), |path| FileJournal(path).journal(held))
    }

    fn discard_journal(&self) -> Result<(), Failure> {
        JOURNAL
            .get()
            .map_or(Ok(()), |path| FileJournal(path).discard_journal())
    }
}

impl Desktop for MacDesktop {
    fn session(&self) -> Result<Session, Failure> {
        Ok(session::posting_session())
    }

    fn frontmost(&self) -> Result<Option<i32>, Failure> {
        Ok(session::front_pid())
    }

    fn foreign_key_windows(&self, pid: i32) -> Result<Vec<i32>, Failure> {
        ax::foreign_key_windows(pid)
    }

    fn click_target(&self, at: Point) -> Result<i32, Failure> {
        ax::click_target(at)
    }

    fn describe(&self, at: Point) -> Value {
        ax::describe_at(at)
    }

    fn post(&self, event: Event, flags: u64) -> Result<(), Failure> {
        let cg = match event {
            Event::Modifier { modifier, down } => keyboard(modifier.keycode, down, flags, true)?,
            Event::Key { code, down } => keyboard(code, down, flags, false)?,
            Event::Move(at) => mouse(kCGEventMouseMoved, at, kCGMouseButtonLeft, flags)?,
            Event::Button { button, down, at } => {
                let (cg, press, release, _) = cg_button(button);
                mouse(if down { press } else { release }, at, cg, flags)?
            }
            Event::Drag { button, at } => {
                let (cg, _, _, dragged) = cg_button(button);
                mouse(dragged, at, cg, flags)?
            }
        };
        // SAFETY: `cg` is a live CGEvent.
        unsafe { CGEventPost(kCGHIDEventTap, cg.ptr()) };
        Ok(())
    }

    fn pause(&self, gap: Duration) {
        std::thread::sleep(gap);
    }
}

fn keyboard(keycode: u16, down: bool, flags: u64, flags_changed: bool) -> Result<Cf, Failure> {
    // SAFETY: plain constructors; a null result is handled.
    unsafe {
        let source = Cf::owned(CGEventSourceCreate(kCGEventSourceStateHIDSystemState));
        let event = Cf::owned(CGEventCreateKeyboardEvent(
            source.as_ref().map_or(std::ptr::null(), Cf::ptr),
            keycode,
            down,
        ))
        .ok_or_else(|| Failure::Failed("CGEventCreateKeyboardEvent returned null".into()))?;
        if flags_changed {
            CGEventSetType(event.ptr(), kCGEventFlagsChanged);
        }
        CGEventSetFlags(event.ptr(), flags);
        Ok(event)
    }
}

fn mouse(event_type: u32, at: Point, button: u32, flags: u64) -> Result<Cf, Failure> {
    // SAFETY: plain constructors; a null result is handled.
    unsafe {
        let source = Cf::owned(CGEventSourceCreate(kCGEventSourceStateHIDSystemState));
        let event = Cf::owned(CGEventCreateMouseEvent(
            source.as_ref().map_or(std::ptr::null(), Cf::ptr),
            event_type,
            CGPoint { x: at.x, y: at.y },
            button,
        ))
        .ok_or_else(|| Failure::Failed("CGEventCreateMouseEvent returned null".into()))?;
        let press_or_release = [
            kCGEventLeftMouseDown,
            kCGEventLeftMouseUp,
            kCGEventRightMouseDown,
            kCGEventRightMouseUp,
        ];
        if press_or_release.contains(&event_type) {
            CGEventSetIntegerValueField(event.ptr(), kCGMouseEventClickState, 1);
        }
        CGEventSetFlags(event.ptr(), flags);
        Ok(event)
    }
}

fn cg_button(button: Button) -> (u32, u32, u32, u32) {
    match button {
        Button::Left => (
            kCGMouseButtonLeft,
            kCGEventLeftMouseDown,
            kCGEventLeftMouseUp,
            kCGEventLeftMouseDragged,
        ),
        Button::Right => (
            kCGMouseButtonRight,
            kCGEventRightMouseDown,
            kCGEventRightMouseUp,
            kCGEventRightMouseDragged,
        ),
    }
}

/// Whether the session or the HID system reports `keycode` down; a posted
/// press may be in either.
fn key_down(keycode: u16) -> bool {
    // SAFETY: plain state queries.
    unsafe {
        CGEventSourceKeyState(kCGEventSourceStateCombinedSessionState, keycode)
            || CGEventSourceKeyState(kCGEventSourceStateHIDSystemState, keycode)
    }
}

fn button_down(button: u32) -> bool {
    // SAFETY: plain state queries.
    unsafe {
        CGEventSourceButtonState(kCGEventSourceStateCombinedSessionState, button)
            || CGEventSourceButtonState(kCGEventSourceStateHIDSystemState, button)
    }
}

/// `key --release-all`, run by hand only: it cannot know who pressed what, so
/// every modifier gets a flags-cleared release (a no-op for one that is up),
/// and a button or an ordinary key gets its release when the system reports it
/// down — the person at the desk's included. Automated recovery releases a
/// helper's own journal instead (`release-held`).
pub fn release_all() -> Result<Value, Failure> {
    // SAFETY: a plain query; a null result is handled.
    let location = unsafe {
        Cf::owned(CGEventCreate(std::ptr::null()))
            .map(|event| CGEventGetLocation(event.ptr()))
            .unwrap_or_default()
    };
    let at = Point {
        x: location.x,
        y: location.y,
    };
    let desktop = MacDesktop;
    let mut buttons = Vec::new();
    for button in [Button::Left, Button::Right] {
        if button_down(cg_button(button).0) {
            let release = Event::Button {
                button,
                down: false,
                at,
            };
            desktop.post(release, 0)?;
            buttons.push(button_name(button));
            desktop.pause(MOUSE_GAP);
        }
    }
    let modifier_codes: Vec<u16> = MODIFIERS.iter().map(|modifier| modifier.keycode).collect();
    let mut keys = Vec::new();
    for code in (0..128).filter(|code| !modifier_codes.contains(code)) {
        if key_down(code) {
            desktop.post(Event::Key { code, down: false }, 0)?;
            keys.push(code);
        }
    }
    for modifier in MODIFIERS {
        desktop.post(
            Event::Modifier {
                modifier,
                down: false,
            },
            0,
        )?;
    }
    desktop.pause(KEY_GAP);
    Ok(json!({
        "buttons_released": buttons,
        "keys_released": keys,
        "modifiers_released": MODIFIERS.iter().map(|modifier| modifier.name).collect::<Vec<_>>(),
        "at": point_json(at),
    }))
}
