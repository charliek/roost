//! Safe wrapper around `ghostty_key_encoder_*` + `ghostty_key_event_*`.
//!
//! The encoder turns a (key, mods, utf8) triple into the VT bytes that
//! libghostty wants to see on stdin. The Mac UI's
//! `mac/Sources/Roost/KeyEncoder.swift` is the closest reference; this
//! wrapper exposes the same surface in Rust idioms (RAII allocations,
//! borrow-checked event builder, `Vec<u8>` return).

use std::ptr;

use crate::sys;
use crate::{Error, Result, Terminal};

/// Re-export of the bindgen-generated key enum. Roughly 177 variants
/// covering letters, digits, function keys, navigation, numpad,
/// punctuation, and special keys. Constants are accessible as
/// `Key::GHOSTTY_KEY_*` through the bindgen-generated `pub const`s.
pub type Key = sys::GhosttyKey;

/// Modifier bitmask. Combine with `|` from the `GHOSTTY_MODS_*` consts
/// re-exported below.
pub type Mods = sys::GhosttyMods;

/// Key action: press, repeat, release. The encoder cares about all
/// three (Kitty keyboard protocol emits release events on press-up).
pub type KeyAction = sys::GhosttyKeyAction;

/// Bit constants for [`Mods`].
#[allow(dead_code)]
pub mod mods {
    use super::sys;
    pub const SHIFT: u16 = sys::GHOSTTY_MODS_SHIFT as u16;
    pub const CTRL: u16 = sys::GHOSTTY_MODS_CTRL as u16;
    pub const ALT: u16 = sys::GHOSTTY_MODS_ALT as u16;
    pub const SUPER: u16 = sys::GHOSTTY_MODS_SUPER as u16;
    pub const CAPS_LOCK: u16 = sys::GHOSTTY_MODS_CAPS_LOCK as u16;
    pub const NUM_LOCK: u16 = sys::GHOSTTY_MODS_NUM_LOCK as u16;
}

/// Key-action constants for [`KeyAction`].
#[allow(dead_code)]
pub mod key_action {
    use super::sys;
    pub const RELEASE: u32 = sys::GhosttyKeyAction_GHOSTTY_KEY_ACTION_RELEASE;
    pub const PRESS: u32 = sys::GhosttyKeyAction_GHOSTTY_KEY_ACTION_PRESS;
    pub const REPEAT: u32 = sys::GhosttyKeyAction_GHOSTTY_KEY_ACTION_REPEAT;
}

pub struct KeyEvent {
    handle: sys::GhosttyKeyEvent,
}

unsafe impl Send for KeyEvent {}

impl KeyEvent {
    pub fn new() -> Result<Self> {
        let mut handle: sys::GhosttyKeyEvent = ptr::null_mut();
        // SAFETY: null allocator + out-pointer we own.
        let rc = unsafe { sys::ghostty_key_event_new(ptr::null_mut(), &mut handle) };
        Error::from_result(rc)?;
        if handle.is_null() {
            return Err(Error::NullHandle);
        }
        Ok(Self { handle })
    }

    pub fn set_action(&mut self, action: KeyAction) -> &mut Self {
        // SAFETY: handle non-null per constructor.
        unsafe { sys::ghostty_key_event_set_action(self.handle, action) };
        self
    }

    pub fn set_key(&mut self, key: Key) -> &mut Self {
        // SAFETY: handle non-null.
        unsafe { sys::ghostty_key_event_set_key(self.handle, key) };
        self
    }

    pub fn set_mods(&mut self, mods: Mods) -> &mut Self {
        // SAFETY: handle non-null.
        unsafe { sys::ghostty_key_event_set_mods(self.handle, mods) };
        self
    }

    pub fn set_consumed_mods(&mut self, mods: Mods) -> &mut Self {
        // SAFETY: handle non-null.
        unsafe { sys::ghostty_key_event_set_consumed_mods(self.handle, mods) };
        self
    }

    pub fn set_composing(&mut self, composing: bool) -> &mut Self {
        // SAFETY: handle non-null.
        unsafe { sys::ghostty_key_event_set_composing(self.handle, composing) };
        self
    }

    pub fn set_unshifted_codepoint(&mut self, codepoint: u32) -> &mut Self {
        // SAFETY: handle non-null.
        unsafe { sys::ghostty_key_event_set_unshifted_codepoint(self.handle, codepoint) };
        self
    }

    /// Set the typed UTF-8 string (printable characters only — the
    /// caller is responsible for filtering C0/DEL/private-use). An
    /// empty slice clears the field.
    pub fn set_utf8(&mut self, utf8: &[u8]) -> &mut Self {
        // SAFETY: handle non-null; libghostty copies the bytes
        // internally so the slice's lifetime ending after this is fine.
        unsafe {
            sys::ghostty_key_event_set_utf8(self.handle, utf8.as_ptr() as *const _, utf8.len());
        }
        self
    }

    pub(crate) fn handle(&self) -> sys::GhosttyKeyEvent {
        self.handle
    }
}

impl Drop for KeyEvent {
    fn drop(&mut self) {
        // SAFETY: handle non-null per constructor.
        unsafe { sys::ghostty_key_event_free(self.handle) };
    }
}

pub struct KeyEncoder {
    handle: sys::GhosttyKeyEncoder,
    /// Scratch buffer to avoid reallocating on every keystroke.
    scratch: Vec<u8>,
}

unsafe impl Send for KeyEncoder {}

impl KeyEncoder {
    pub fn new() -> Result<Self> {
        let mut handle: sys::GhosttyKeyEncoder = ptr::null_mut();
        // SAFETY: null allocator + out-pointer we own.
        let rc = unsafe { sys::ghostty_key_encoder_new(ptr::null_mut(), &mut handle) };
        Error::from_result(rc)?;
        if handle.is_null() {
            return Err(Error::NullHandle);
        }
        // 128 bytes is enough for any single-keystroke encoding the Mac
        // UI has seen in practice (including Kitty CSI-u + multi-byte
        // UTF-8 + bracketed mode); grow on OUT_OF_SPACE if needed.
        Ok(Self {
            handle,
            scratch: Vec::with_capacity(128),
        })
    }

    /// Re-sync the encoder's options from the terminal's current
    /// modes. Call before each `encode` if the terminal modes might
    /// have changed (DECCKM, Kitty flags, modifyOtherKeys) — the Mac
    /// UI does this unconditionally.
    pub fn sync_from_terminal(&mut self, terminal: &Terminal) {
        // SAFETY: both handles non-null.
        unsafe { sys::ghostty_key_encoder_setopt_from_terminal(self.handle, terminal.handle()) };
    }

    /// Encode a key event to VT bytes. Returns an empty slice for
    /// no-output events (modifier-only press, IME dead key). The
    /// returned `Vec<u8>` is a fresh allocation; if you need to avoid
    /// the per-keystroke alloc, [`Self::encode_into`] writes into a
    /// caller-provided buffer.
    pub fn encode(&mut self, event: &KeyEvent) -> Result<Vec<u8>> {
        let len = self.encode_into_scratch(event)?;
        Ok(self.scratch[..len].to_vec())
    }

    /// Encode into a caller-provided buffer. Grows the buffer if
    /// libghostty signals `OUT_OF_SPACE`. Returns the number of bytes
    /// written.
    pub fn encode_into(&mut self, event: &KeyEvent, out: &mut Vec<u8>) -> Result<usize> {
        let len = self.encode_into_scratch(event)?;
        out.clear();
        out.extend_from_slice(&self.scratch[..len]);
        Ok(len)
    }

    fn encode_into_scratch(&mut self, event: &KeyEvent) -> Result<usize> {
        // Start with whatever scratch we already have; grow on demand.
        if self.scratch.capacity() < 16 {
            self.scratch.reserve(16);
        }
        // Allow buffer to be addressed as len = capacity.
        loop {
            let cap = self.scratch.capacity();
            let mut written: usize = 0;
            // SAFETY: handle non-null; buf has `cap` writable bytes
            // (Vec's capacity is its backing alloc). out_len is local.
            let rc = unsafe {
                sys::ghostty_key_encoder_encode(
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
                    return Ok(written);
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

impl Drop for KeyEncoder {
    fn drop(&mut self) {
        // SAFETY: handle non-null per constructor.
        unsafe { sys::ghostty_key_encoder_free(self.handle) };
    }
}
