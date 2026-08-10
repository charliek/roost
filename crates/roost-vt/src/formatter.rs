//! Selection text extraction via `ghostty_formatter_*`.
//!
//! The formatter is the only libghostty API that can read cells outside
//! the viewport, so it — not the render state — is what makes a
//! scrollback-spanning copy complete.
//!
//! # Why this module exposes exactly one function
//!
//! `GhosttyGridRef` is an unvalidated pin into the terminal's page list,
//! and libghostty resolves a selection's endpoints with an unchecked
//! `pointFromPin(...).?` (`Selection.order`). The archive is built
//! `-Doptimize=ReleaseFast`, where that null unwrap is undefined
//! behavior rather than a panic. Any mutating terminal call — `vt_write`,
//! `resize`, `reset`, an alt-screen switch — landing between the pin and
//! the format is enough to trigger it.
//!
//! [`selection_text`] therefore pins, formats, and frees inside a single
//! synchronous call holding `&Terminal`. No `GridRef` and no formatter
//! handle escapes it, which makes the hazardous interleaving
//! unrepresentable instead of merely documented.

use std::ptr;

use crate::sys;
use crate::{Error, Point, Result, Terminal};

/// RAII owner of a `GhosttyFormatter` handle. Private so a formatter can
/// never be stored across a terminal mutation (see the module docs).
struct Formatter(sys::GhosttyFormatter);

impl Drop for Formatter {
    fn drop(&mut self) {
        // SAFETY: handle came from a successful
        // `ghostty_formatter_terminal_new` and is freed exactly once.
        unsafe { sys::ghostty_formatter_free(self.0) };
    }
}

/// RAII owner of the buffer `ghostty_formatter_format_alloc` returns.
struct FormatterBuf {
    ptr: *mut u8,
    len: usize,
}

impl Drop for FormatterBuf {
    fn drop(&mut self) {
        // SAFETY: allocated by `format_alloc` with the default allocator,
        // so it is freed with the same (null) allocator and its exact len.
        unsafe { sys::ghostty_free(ptr::null(), self.ptr, self.len) };
    }
}

/// Format the inclusive cell range `start..=end` of the active screen as
/// plain text.
///
/// Both endpoints are inclusive — pass the raw anchor/cursor cells, not a
/// half-open range. Drag order does not matter: libghostty normalizes
/// reversed endpoints itself via `Selection.order`.
///
/// Returns `None` when an endpoint no longer names a cell on the active
/// screen — an alt-screen switch, or a row evicted from scrollback. That
/// is an empty selection, not a failure.
pub(crate) fn selection_text(
    terminal: &Terminal,
    start: Point,
    end: Point,
) -> Result<Option<String>> {
    let (Some(start_ref), Some(end_ref)) = (terminal.grid_ref(start), terminal.grid_ref(end))
    else {
        return Ok(None);
    };

    let selection = sys::GhosttySelection {
        size: std::mem::size_of::<sys::GhosttySelection>(),
        start: start_ref.as_sys(),
        end: end_ref.as_sys(),
        rectangle: false,
    };
    let options = sys::GhosttyFormatterTerminalOptions {
        size: std::mem::size_of::<sys::GhosttyFormatterTerminalOptions>(),
        emit: sys::GhosttyFormatterFormat_GHOSTTY_FORMATTER_FORMAT_PLAIN,
        unwrap: false,
        // Roost does want trailing spaces gone, but not libghostty's
        // version of it: its trim treats any cell whose base codepoint is
        // a space as blank, so a space carrying a combining mark loses the
        // mark and comes back as a bare space. Trailing spaces are removed
        // in `trim_trailing_spaces` instead, which is otherwise
        // equivalent — textless cells are dropped by libghostty either
        // way. `false` also happens to be bindgen's zeroed default;
        // it is set explicitly because libghostty's own default is `true`.
        trim: false,
        // Ignored for PLAIN, but every `size` still has to be right.
        extra: sys::GhosttyFormatterTerminalExtra {
            size: std::mem::size_of::<sys::GhosttyFormatterTerminalExtra>(),
            screen: sys::GhosttyFormatterScreenExtra {
                size: std::mem::size_of::<sys::GhosttyFormatterScreenExtra>(),
                ..Default::default()
            },
            ..Default::default()
        },
        selection: &selection,
    };

    let mut handle: sys::GhosttyFormatter = ptr::null_mut();
    // SAFETY: default allocator, an out-pointer we own, a live terminal
    // handle, and an options struct that outlives the call (libghostty
    // copies the selection into the formatter).
    let rc = unsafe {
        sys::ghostty_formatter_terminal_new(ptr::null(), &mut handle, terminal.handle(), options)
    };
    Error::from_result(rc)?;
    if handle.is_null() {
        return Err(Error::NullHandle);
    }
    let formatter = Formatter(handle);

    let mut out_ptr: *mut u8 = ptr::null_mut();
    let mut out_len: usize = 0;
    // SAFETY: formatter handle is live; both out params are stack locals.
    let rc = unsafe {
        sys::ghostty_formatter_format_alloc(formatter.0, ptr::null(), &mut out_ptr, &mut out_len)
    };
    Error::from_result(rc)?;
    if out_ptr.is_null() {
        return Ok(Some(String::new()));
    }
    let buf = FormatterBuf {
        ptr: out_ptr,
        len: out_len,
    };

    // SAFETY: libghostty reports the buffer it just allocated; the slice
    // borrow ends before `buf` frees it.
    let bytes = unsafe { std::slice::from_raw_parts(buf.ptr, buf.len) };
    std::str::from_utf8(bytes)
        .map(|text| Some(trim_trailing_spaces(text)))
        .map_err(|_| Error::InvalidUtf8)
}

/// Drop trailing `0x20` from every line. Only spaces — every other
/// whitespace codepoint is content a terminal cell holds deliberately.
fn trim_trailing_spaces(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for (i, line) in text.split('\n').enumerate() {
        if i > 0 {
            out.push('\n');
        }
        out.push_str(line.trim_end_matches(' '));
    }
    out
}
