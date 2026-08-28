//! Selection text extraction via `ghostty_terminal_selection_format_alloc`.
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
//! behavior rather than a panic. Upstream codifies this as an unchecked
//! precondition rather than validating it, so any mutating terminal call
//! — `vt_write`, `resize`, `reset`, an alt-screen switch — landing
//! between the pin and the format is enough to trigger it.
//!
//! [`selection_text`] therefore pins, formats, and frees inside a single
//! synchronous call holding `&Terminal`. No `GridRef` escapes it, which
//! makes the hazardous interleaving unrepresentable instead of merely
//! documented. Selection endpoints live outside as [`crate::TrackedRef`]s,
//! which libghostty keeps current; snapshotting them into raw pins
//! happens *here*, immediately before the `GhosttySelection` is built,
//! and the pins die with the call.

use std::ptr;

use crate::sys;
use crate::{Error, Result, Terminal, TrackedRef};

/// Join soft-wrapped rows into one line when copying.
///
/// Plan 024 D4.4. This is a **deliberate, visible behavior change**: a
/// line the terminal wrapped across several rows copies as one long
/// line, the way Ghostty and every other modern terminal copy it,
/// instead of as one line per screen row. Flip it to `false` to restore
/// per-row copying.
///
/// Both copy paths honor this constant — libghostty's formatter here,
/// and the render-state walk in `selection.rs` that handles a selection
/// entirely inside the viewport — so the two agree whichever way it is
/// set, and a copy never depends on scroll position. Its Swift twin is
/// `SelectionFormatter.unwrapSoftWrappedLines`; the two must match or
/// the Mac and Linux UIs copy differently.
pub const UNWRAP_SOFT_WRAPPED_LINES: bool = true;

/// RAII owner of the buffer
/// `ghostty_terminal_selection_format_alloc` returns.
struct FormatterBuf {
    ptr: *mut u8,
    len: usize,
}

impl Drop for FormatterBuf {
    fn drop(&mut self) {
        // SAFETY: allocated by `selection_format_alloc` with the default
        // allocator, so it is freed with the same (null) allocator and
        // its exact len, as that call's contract requires.
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
/// Returns `None` when an endpoint no longer names a cell — a row
/// evicted from scrollback, or a terminal reset. That is an empty
/// selection, not a failure.
///
/// Both endpoints must belong to the terminal's currently active screen;
/// the caller gates on that and [`TrackedRef::snapshot`] debug-asserts
/// it, because libghostty's formatter treats it as a precondition.
pub(crate) fn selection_text(
    terminal: &Terminal,
    start: &TrackedRef,
    end: &TrackedRef,
) -> Result<Option<String>> {
    // Snapshot here, not in the caller: the pins are only valid until
    // the terminal's next update, and nothing between this line and the
    // one `selection_format_alloc` call touches the terminal.
    let (Some(start_ref), Some(end_ref)) = (start.snapshot(terminal)?, end.snapshot(terminal)?)
    else {
        return Ok(None);
    };

    let selection = sys::GhosttySelection {
        size: std::mem::size_of::<sys::GhosttySelection>(),
        start: start_ref.as_sys(),
        end: end_ref.as_sys(),
        rectangle: false,
    };
    let options = sys::GhosttyTerminalSelectionFormatOptions {
        size: std::mem::size_of::<sys::GhosttyTerminalSelectionFormatOptions>(),
        emit: sys::GhosttyFormatterFormat_GHOSTTY_FORMATTER_FORMAT_PLAIN,
        unwrap: UNWRAP_SOFT_WRAPPED_LINES,
        // Roost does want trailing spaces gone, but not libghostty's
        // version of it: its trim treats any cell whose base codepoint is
        // a space as blank, so a space carrying a combining mark loses the
        // mark and comes back as a bare space. Trailing spaces are removed
        // in `trim_trailing_spaces` instead, which is otherwise
        // equivalent — textless cells are dropped by libghostty either
        // way. `false` also happens to be bindgen's zeroed default;
        // it is set explicitly because libghostty's own default is `true`.
        trim: false,
        // Non-null, so the terminal's own active selection is not
        // consulted and `GHOSTTY_NO_VALUE` cannot come back for a missing
        // one.
        selection: &selection,
    };

    let mut out_ptr: *mut u8 = ptr::null_mut();
    let mut out_len: usize = 0;
    // SAFETY: a live terminal handle, the default allocator (null — the
    // same one `FormatterBuf` frees with), an options struct whose
    // `selection` pointer outlives the call, and two stack out-params.
    let rc = unsafe {
        sys::ghostty_terminal_selection_format_alloc(
            terminal.handle(),
            ptr::null(),
            options,
            &mut out_ptr,
            &mut out_len,
        )
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
///
/// Shared with the viewport walk so both paths trim identically. With
/// [`UNWRAP_SOFT_WRAPPED_LINES`] on, "line" means the joined logical
/// line: a wrapped row's trailing spaces sit mid-line and survive,
/// which is what keeps the rejoin from eating characters.
pub(crate) fn trim_trailing_spaces(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for (i, line) in text.split('\n').enumerate() {
        if i > 0 {
            out.push('\n');
        }
        out.push_str(line.trim_end_matches(' '));
    }
    out
}
