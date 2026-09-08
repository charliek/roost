//! Text extraction via `ghostty_terminal_selection_format_alloc`.
//!
//! The formatter is the only libghostty API that can read cells outside
//! the viewport, so it — not the render state — is what makes a
//! scrollback-spanning copy, and a scrollback-carrying dump, complete.
//!
//! # Why every function here formats inside one call
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
//! Every function here therefore pins, formats, and frees inside a
//! single synchronous call holding `&Terminal`. No `GridRef` escapes
//! them, which makes the hazardous interleaving unrepresentable instead
//! of merely documented. Selection endpoints live outside as
//! [`crate::TrackedRef`]s, which libghostty keeps current; snapshotting
//! them into raw pins happens *here*, immediately before the
//! `GhosttySelection` is built, and the pins die with the call.

use std::ptr;

use crate::sys;
use crate::{Error, GridRef, Point, Result, Terminal, TrackedRef};

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
/// `ghostty_terminal_selection_format_alloc` returns. Shared with
/// [`crate::vt_dump`], whose `ghostty_formatter_format_alloc` output is
/// allocated and freed the same way.
pub(crate) struct FormatterBuf {
    pub(crate) ptr: *mut u8,
    pub(crate) len: usize,
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
    format_selection(terminal, &start_ref, &end_ref, UNWRAP_SOFT_WRAPPED_LINES).map(Some)
}

/// The one `selection_format_alloc` call this module's readers share:
/// `start..=end` as plain text with trailing spaces removed, soft-wrapped
/// rows joined when `unwrap`.
///
/// Roost does want trailing spaces gone, but not libghostty's version of
/// it: its trim treats any cell whose base codepoint is a space as blank,
/// so a space carrying a combining mark loses the mark and comes back as
/// a bare space. `trim: false` plus [`trim_trailing_spaces`] is otherwise
/// equivalent — textless cells are dropped by libghostty either way.
/// `false` also happens to be bindgen's zeroed default; it is set
/// explicitly because libghostty's own default is `true`.
///
/// Both pins must be taken immediately before the call with nothing
/// touching the terminal in between, for the reason this module's header
/// gives.
fn format_selection(
    terminal: &Terminal,
    start: &GridRef,
    end: &GridRef,
    unwrap: bool,
) -> Result<String> {
    let selection = sys::GhosttySelection {
        size: std::mem::size_of::<sys::GhosttySelection>(),
        start: start.as_sys(),
        end: end.as_sys(),
        rectangle: false,
    };
    let options = sys::GhosttyTerminalSelectionFormatOptions {
        size: std::mem::size_of::<sys::GhosttyTerminalSelectionFormatOptions>(),
        emit: sys::GhosttyFormatterFormat_GHOSTTY_FORMATTER_FORMAT_PLAIN,
        unwrap,
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
        return Ok(String::new());
    }
    let buf = FormatterBuf {
        ptr: out_ptr,
        len: out_len,
    };

    // SAFETY: libghostty reports the buffer it just allocated; the slice
    // borrow ends before `buf` frees it.
    let bytes = unsafe { std::slice::from_raw_parts(buf.ptr, buf.len) };
    let text = std::str::from_utf8(bytes).map_err(|_| Error::InvalidUtf8)?;
    Ok(trim_trailing_spaces(text))
}

/// Rows of history sitting above the **current** viewport.
///
/// Deliberately `scrollbar().offset` rather than `total - len`: history
/// is anchored on what the viewport is showing, so a caller reading a
/// scrolled terminal gets the rows adjacent to its own first row instead
/// of rows it is already displaying.
pub fn scrollback_rows(terminal: &Terminal) -> Result<u32> {
    Ok(u32::try_from(terminal.scrollbar()?.offset).unwrap_or(u32::MAX))
}

/// The last `rows` lines of history above the current viewport, top to
/// bottom — the final entry is always the row immediately above the
/// viewport's first row.
///
/// Fewer rows than asked for only when history is shorter; the result is
/// exactly `min(rows, scrollback_rows)` entries, blank rows included as
/// empty strings.
pub fn scrollback_text(terminal: &Terminal, rows: u32) -> Result<Vec<String>> {
    let top = scrollback_rows(terminal)?;
    let count = rows.min(top);
    if count == 0 {
        return Ok(Vec::new());
    }
    let cols = terminal.cols().ok_or(Error::InvalidValue)?;

    // Every history row is `cols` wide, so a rejected endpoint is a bug
    // in this arithmetic, never a state history can be in — reporting it
    // as empty history would hide it.
    let (Some(start), Some(end)) = (
        terminal.grid_ref(Point::screen(0, top - count)),
        terminal.grid_ref(Point::screen(cols.saturating_sub(1), top - 1)),
    ) else {
        return Err(Error::InvalidValue);
    };

    // Not unwrapped, whatever the soft-wrap flags say: the caller indexes
    // history by row.
    let text = format_selection(terminal, &start, &end, false)?;

    // The formatter emits a row's newline only when a later non-blank
    // row follows, so blank rows at the end of the range come back
    // missing rather than empty. Pad them back: the count is part of the
    // contract, and a short vector would silently renumber history.
    let mut lines: Vec<String> = text.split('\n').map(str::to_owned).collect();
    while lines.len() < count as usize {
        lines.push(String::new());
    }
    Ok(lines)
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
