//! Safe wrapper around `ghostty_mouse_encoder_*` + `ghostty_mouse_event_*`.
//!
//! The encoder turns a normalized mouse event (action, button, mods,
//! surface-space position) into the VT bytes a mouse-tracking app
//! expects on stdin (X10 / SGR / URXVT / pixel formats — whichever the
//! terminal negotiated). Roost uses it to forward the scroll wheel as
//! button-4/5 reports when an app (opencode, htop, …) enables mouse
//! tracking; without it the wheel was silently dropped.
//!
//! Shape mirrors [`crate::key_encoder`]: RAII handles, a borrow-checked
//! event builder, a reusable scratch buffer, and a `Vec<u8>` return.

use std::ptr;

use crate::sys;
use crate::{Error, Result, Terminal};

/// Mouse action: press, release, motion.
pub type MouseAction = sys::GhosttyMouseAction;

/// Mouse button identity (left/right/middle and the extended 4..11
/// range — wheel up/down map to buttons 4/5).
pub type MouseButton = sys::GhosttyMouseButton;

/// Action constants for [`MouseAction`].
#[allow(dead_code)]
pub mod mouse_action {
    use super::sys;
    pub const PRESS: u32 = sys::GhosttyMouseAction_GHOSTTY_MOUSE_ACTION_PRESS;
    pub const RELEASE: u32 = sys::GhosttyMouseAction_GHOSTTY_MOUSE_ACTION_RELEASE;
    pub const MOTION: u32 = sys::GhosttyMouseAction_GHOSTTY_MOUSE_ACTION_MOTION;
}

/// Button constants for [`MouseButton`]. Wheel up = button 4, wheel
/// down = button 5 — the X11 convention every mouse-tracking app reads.
#[allow(dead_code)]
pub mod mouse_button {
    use super::sys;
    pub const LEFT: u32 = sys::GhosttyMouseButton_GHOSTTY_MOUSE_BUTTON_LEFT;
    pub const MIDDLE: u32 = sys::GhosttyMouseButton_GHOSTTY_MOUSE_BUTTON_MIDDLE;
    pub const RIGHT: u32 = sys::GhosttyMouseButton_GHOSTTY_MOUSE_BUTTON_RIGHT;
    /// Wheel up.
    pub const FOUR: u32 = sys::GhosttyMouseButton_GHOSTTY_MOUSE_BUTTON_FOUR;
    /// Wheel down.
    pub const FIVE: u32 = sys::GhosttyMouseButton_GHOSTTY_MOUSE_BUTTON_FIVE;
}

pub struct MouseEvent {
    handle: sys::GhosttyMouseEvent,
}

unsafe impl Send for MouseEvent {}

impl MouseEvent {
    pub fn new() -> Result<Self> {
        let mut handle: sys::GhosttyMouseEvent = ptr::null_mut();
        // SAFETY: null allocator + out-pointer we own.
        let rc = unsafe { sys::ghostty_mouse_event_new(ptr::null_mut(), &mut handle) };
        Error::from_result(rc)?;
        if handle.is_null() {
            return Err(Error::NullHandle);
        }
        Ok(Self { handle })
    }

    pub fn set_action(&mut self, action: MouseAction) -> &mut Self {
        // SAFETY: handle non-null per constructor.
        unsafe { sys::ghostty_mouse_event_set_action(self.handle, action) };
        self
    }

    pub fn set_button(&mut self, button: MouseButton) -> &mut Self {
        // SAFETY: handle non-null.
        unsafe { sys::ghostty_mouse_event_set_button(self.handle, button) };
        self
    }

    /// Set the event to "no button" (motion events).
    pub fn clear_button(&mut self) -> &mut Self {
        // SAFETY: handle non-null.
        unsafe { sys::ghostty_mouse_event_clear_button(self.handle) };
        self
    }

    pub fn set_mods(&mut self, mods: crate::Mods) -> &mut Self {
        // SAFETY: handle non-null.
        unsafe { sys::ghostty_mouse_event_set_mods(self.handle, mods) };
        self
    }

    /// Position in **surface-space pixels** (not cells). The encoder
    /// divides by the cell size from [`MouseEncoder::set_size`] to
    /// derive the reported cell; getting the units wrong yields
    /// off-by-one button reports.
    pub fn set_position(&mut self, x: f32, y: f32) -> &mut Self {
        let pos = sys::GhosttyMousePosition { x, y };
        // SAFETY: handle non-null; pos is stack-owned and copied.
        unsafe { sys::ghostty_mouse_event_set_position(self.handle, pos) };
        self
    }

    pub(crate) fn handle(&self) -> sys::GhosttyMouseEvent {
        self.handle
    }
}

impl Drop for MouseEvent {
    fn drop(&mut self) {
        // SAFETY: handle non-null per constructor.
        unsafe { sys::ghostty_mouse_event_free(self.handle) };
    }
}

pub struct MouseEncoder {
    handle: sys::GhosttyMouseEncoder,
    scratch: Vec<u8>,
}

unsafe impl Send for MouseEncoder {}

impl MouseEncoder {
    pub fn new() -> Result<Self> {
        let mut handle: sys::GhosttyMouseEncoder = ptr::null_mut();
        // SAFETY: null allocator + out-pointer we own.
        let rc = unsafe { sys::ghostty_mouse_encoder_new(ptr::null_mut(), &mut handle) };
        Error::from_result(rc)?;
        if handle.is_null() {
            return Err(Error::NullHandle);
        }
        // 64 bytes covers any single SGR/URXVT mouse report; grow on
        // OUT_OF_SPACE if a format ever exceeds it.
        Ok(Self {
            handle,
            scratch: Vec::with_capacity(64),
        })
    }

    /// Pull tracking mode + output format from the terminal's current
    /// modes (DECSET 1000/1002/1003 + SGR 1006, etc.). Does NOT touch
    /// the size context — call [`Self::set_size`] separately. Mirrors
    /// the key encoder's `sync_from_terminal`.
    pub fn sync_from_terminal(&mut self, terminal: &Terminal) {
        // SAFETY: both handles non-null.
        unsafe { sys::ghostty_mouse_encoder_setopt_from_terminal(self.handle, terminal.handle()) };
    }

    /// Set the rendered geometry the encoder uses to map a surface-space
    /// pixel position to a cell. `cell_width`/`cell_height` must be
    /// non-zero (the C API rejects a zero cell size). Padding is assumed
    /// zero — Roost's renderer draws cells flush to the surface origin.
    pub fn set_size(
        &mut self,
        screen_width: u32,
        screen_height: u32,
        cell_width: u32,
        cell_height: u32,
    ) {
        let size = sys::GhosttyMouseEncoderSize {
            size: std::mem::size_of::<sys::GhosttyMouseEncoderSize>(),
            screen_width,
            screen_height,
            cell_width,
            cell_height,
            padding_top: 0,
            padding_bottom: 0,
            padding_right: 0,
            padding_left: 0,
        };
        // SAFETY: handle non-null; size struct is stack-owned and the
        // encoder copies it. The option tag matches the value type.
        unsafe {
            sys::ghostty_mouse_encoder_setopt(
                self.handle,
                sys::GhosttyMouseEncoderOption_GHOSTTY_MOUSE_ENCODER_OPT_SIZE,
                (&size) as *const _ as *const _,
            );
        }
    }

    /// Encode a mouse event to VT bytes. Returns an empty `Vec` for
    /// events the encoder chooses not to report (e.g. deduplicated
    /// motion, or a wheel event when tracking is off).
    pub fn encode(&mut self, event: &MouseEvent) -> Result<Vec<u8>> {
        loop {
            let cap = self.scratch.capacity();
            let mut written: usize = 0;
            // SAFETY: handle non-null; scratch has `cap` writable bytes
            // (Vec capacity is its backing alloc). out_len is local.
            let rc = unsafe {
                sys::ghostty_mouse_encoder_encode(
                    self.handle,
                    event.handle(),
                    self.scratch.as_mut_ptr() as *mut _,
                    cap,
                    &mut written,
                )
            };
            match Error::from_result(rc) {
                Ok(()) => {
                    // SAFETY: libghostty wrote `written` bytes into the
                    // backing alloc; setting len is sound.
                    unsafe { self.scratch.set_len(written) };
                    return Ok(self.scratch[..written].to_vec());
                }
                Err(Error::OutOfSpace) => {
                    self.scratch.reserve(cap.saturating_mul(2).max(64));
                    continue;
                }
                Err(e) => return Err(e),
            }
        }
    }
}

impl Drop for MouseEncoder {
    fn drop(&mut self) {
        // SAFETY: handle non-null per constructor.
        unsafe { sys::ghostty_mouse_encoder_free(self.handle) };
    }
}
