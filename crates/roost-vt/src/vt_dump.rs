//! The `vt` attach payload: bytes any VT parser replays into a fresh
//! terminal of the attach geometry to reproduce this terminal's active
//! screen — scrollback, viewport, cursor, pen, modes, tabstops,
//! scrolling region, kitty-keyboard flags, charsets, and the parser's
//! unfinished input.
//!
//! # Why the composition is what it is
//!
//! libghostty's formatter emits its state extras *after* the content in
//! the same call (`TerminalFormatter.format`), and it trims trailing
//! blank rows. Both facts force the shape here:
//!
//! * A screen whose bottom `k` rows are blank replays `k` rows short, so
//!   the client's viewport would show `k` rows of history at its top and
//!   every subsequent absolute cursor address would land on the wrong
//!   content. The fix is a pad of `\r\n` between the content and the
//!   state — which is why there are two formatter passes and not one.
//!   (Prepending does not work: it shifts the content down by `k`.)
//! * Nothing in the C API formats state without content, so the second
//!   pass selects the cursor's own cell: re-printing that glyph where the
//!   cursor already sits is idempotent, and the formatter's cursor extra
//!   then restores the position and its pending-wrap.
//!
//! Building the extras by hand instead would re-implement libghostty's
//! state emission and drift from it.
//!
//! # What it deliberately does not carry
//!
//! The inactive screen; soft-wrap flags; per-cell hyperlinks (VT content
//! emits `OSC 8` only for HTML); text-free background-only rows *in
//! history* (there is no cursor addressing into history — viewport ones
//! are refilled); the saved cursor and the kitty-keyboard stack; Kitty
//! images; modes outside [`CARRIED_MODES`]; a program colour override
//! that happens to equal the server's own default; and, under origin
//! mode, a pending wrap — step 8's `ESC[?6h` homes the cursor and its
//! re-position clears the flag, whatever the scrolling region is.
//!
//! One ordering hazard, too: `?2027` (grapheme clustering) rides with
//! the other modes, ahead of the content whose parsing it changes, so a
//! ZWJ sequence the server printed with clustering off and enabled
//! afterwards replays as a single cluster.

use std::mem::size_of;
use std::ptr;

use crate::formatter::FormatterBuf;
use crate::sys;
use crate::{
    ColorRgb, CursorVisualStyle, Error, GridRef, Point, RenderState, RenderedRow, Result, Terminal,
    TerminalColor,
};

/// Modes carried into the payload, in emission order, paired with
/// whether the number is an ANSI mode rather than a DEC-private one.
///
/// An allowlist rather than the formatter's `modes` extra, which replays
/// *every* non-default mode including four that must never be replayed
/// (verified in `stream_terminal.zig`'s `setMode`): `?3` DECCOLM resizes
/// the client away from the attach geometry, `?1048` saves/restores the
/// cursor, `?2048` and `?2033` **write a reply to the PTY on enable**
/// which would travel back through the client to the server's child, and
/// `?2026` latches synchronized output mid-frame and can freeze the
/// client's rendering.
///
/// `?6` (DECOM) and ANSI `4` (IRM) are deliberately absent: leaving both
/// at the client's default (off) is what makes the absolute cursor
/// addresses below land correctly and the cursor-cell print replace
/// rather than insert. They are restored last, in
/// [`write_deferred_modes`].
const CARRIED_MODES: &[(u16, bool)] = &[
    (1, false),
    (5, false),
    (7, false),
    (9, false),
    (25, false),
    (45, false),
    (66, false),
    (69, false),
    (1000, false),
    (1002, false),
    (1003, false),
    (1004, false),
    (1005, false),
    (1006, false),
    (1007, false),
    (1015, false),
    (1016, false),
    (1036, false),
    (1039, false),
    (2004, false),
    (2027, false),
    (12, true),
    (20, true),
];

/// Alternate-screen modes, emitted first and only when set: the content
/// that follows belongs to the active screen, and a fresh client is
/// already on the primary.
const ALT_SCREEN_MODES: &[u16] = &[1049, 47, 1047];

/// Compose the `vt` payload for `terminal`.
///
/// `Ok(None)` is a state, not a failure: the VT parser is mid-sequence
/// and libghostty no longer retains the bytes that would finish it (the
/// unfinished input outran `continuation_max_bytes`), so nothing
/// encodable exists yet and the caller must retry after the next input
/// chunk. `Ok(Some(bytes))` is the payload.
///
/// Refreshes `render` itself — the composition reads the cursor and the
/// viewport's resolved cells, and a stale snapshot would place the
/// cursor on the wrong cell.
pub fn vt_snapshot(terminal: &Terminal, render: &mut RenderState) -> Result<Option<Vec<u8>>> {
    render.update(terminal)?;

    let mut out: Vec<u8> = Vec::new();

    write_color_overrides(terminal, &mut out)?;
    write_modes(terminal, &mut out);

    let content = format_terminal(terminal, CONTENT_EXTRA, None)?;
    let rows_written = count_crlf(&content);
    out.extend_from_slice(&content);

    // The client's viewport only lines up if the replay ends on the
    // active screen's last row: `total_rows` rows exist, the content
    // stopped after `rows_written` newlines, and every row between is a
    // trailing blank the formatter dropped.
    let pad = terminal
        .total_rows()?
        .saturating_sub(1)
        .saturating_sub(rows_written as u64);
    for _ in 0..pad {
        out.extend_from_slice(b"\r\n");
    }

    write_background_fills(terminal, render, &mut out)?;

    let cursor = render.cursor();
    let cursor_col = cursor.map_or(0, |cursor| cursor.col);
    // The formatter widens a spacer-tail start to the whole wide char,
    // so the pre-position has to name the same column the cursor cell's
    // re-print will start at. Only these two want the widened column:
    // pass B's own cursor extra then restores the true one.
    let col = match cursor {
        Some(cursor) if cursor.wide_tail => cursor_col.saturating_sub(1),
        _ => cursor_col,
    };
    let row = cursor.map_or(0, |cursor| cursor.row);
    push(&mut out, &format!("\x1b[{};{}H", row + 1, col + 1));

    out.extend_from_slice(&format_cursor_cell(terminal, row, col)?);

    let (style, blinking) = cursor.map_or((CursorVisualStyle::Block, false), |cursor| {
        (cursor.visual_style, cursor.blinking)
    });
    push(
        &mut out,
        &format!("\x1b[{} q", decscusr_code(style, blinking)),
    );

    write_deferred_modes(terminal, row, cursor_col, &mut out)?;

    // Last, so the client's first live PTY frame completes the cut
    // sequence exactly as it does on the server.
    let Some(continuation) = terminal.continuation()? else {
        return Ok(None);
    };
    out.extend_from_slice(&continuation);

    Ok(Some(out))
}

/// Step 0 — the program's colour *overrides*, never the server's theme.
///
/// The client owns its palette, and `swap_terminal` re-applies nothing,
/// so only what the program itself changed may ride the payload. That
/// rules out the formatter's palette extra, which emits all 256 slots
/// unconditionally and would replace the client's theme wholesale.
fn write_color_overrides(terminal: &Terminal, out: &mut Vec<u8>) -> Result<()> {
    let live = terminal.live_palette()?;
    let default = terminal.default_palette()?;
    for (slot, (live, default)) in live.iter().zip(default.iter()).enumerate() {
        if live != default {
            push(out, &osc_color(&format!("4;{slot}"), *live));
        }
    }

    for which in [
        TerminalColor::Foreground,
        TerminalColor::Background,
        TerminalColor::Cursor,
    ] {
        let Some(live) = terminal.color(which)? else {
            continue;
        };
        if terminal.default_color(which)? != Some(live) {
            push(out, &osc_color(&which.osc_code().to_string(), live));
        }
    }
    Ok(())
}

/// Step 1 — [`CARRIED_MODES`] in both directions, so the client matches
/// the server whatever either side's defaults are.
///
/// Cleared first and set second, in two sweeps over the same list:
/// several allowlisted modes share one piece of terminal state (the
/// mouse-event trio collapses into one tracking mode, the four mouse
/// formats into one), where a later `l` in the same family would undo an
/// earlier `h`.
fn write_modes(terminal: &Terminal, out: &mut Vec<u8>) {
    for &mode in ALT_SCREEN_MODES {
        if terminal.mode_get(mode) {
            push(out, &format!("\x1b[?{mode}h"));
        }
    }
    for setting in [false, true] {
        for &(mode, ansi) in CARRIED_MODES {
            let enabled = if ansi {
                terminal.ansi_mode_get(mode)
            } else {
                terminal.mode_get(mode)
            };
            if enabled != setting {
                continue;
            }
            let prefix = if ansi { "" } else { "?" };
            let suffix = if enabled { 'h' } else { 'l' };
            push(out, &format!("\x1b[{prefix}{mode}{suffix}"));
        }
    }
}

/// Step 4 — refill the viewport rows the formatter drops.
///
/// Its row-blank predicate is `Cell.hasTextAny`, so a row erased with a
/// background colour and no text — a TUI status bar, `EL`/`ED` under SGR
/// 4x — replays as a bare `\r\n` and loses its fill. A row of *spaces*
/// under a background is not that row: a space is text, so the formatter
/// emits those cells itself, and the ECH below would overwrite them with
/// the fill colour alone. Hence the predicate is "every cell is
/// genuinely textless", not "the row's text trimmed to nothing".
///
/// Consumes the caller's dirty set (`mark_full` + `walk_dirty`): only
/// the server encodes a payload, and it never renders from the same
/// [`RenderState`].
fn write_background_fills(
    terminal: &Terminal,
    render: &mut RenderState,
    out: &mut Vec<u8>,
) -> Result<()> {
    let colors = render.colors()?;
    let defaults = (colors.foreground, colors.background);
    let cols = terminal.cols().ok_or(Error::InvalidValue)?;
    let palette = terminal.live_palette()?;

    render.mark_full()?;
    let mut rows: Vec<(u32, RenderedRow)> = Vec::new();
    render.walk_dirty(terminal, |row, cells| {
        if !cells.iter().all(|cell| cell.text.is_empty()) {
            return;
        }
        let built = RenderedRow::build(cells, defaults, cols);
        if !built.cells.is_empty() {
            rows.push((row, built));
        }
    })?;

    for (row, built) in rows {
        let mut run: Option<(u16, u16, ColorRgb)> = None;
        for cell in &built.cells {
            match run {
                Some((start, len, background))
                    if background == cell.background && start + len == cell.col =>
                {
                    run = Some((start, len + 1, background));
                }
                Some((start, len, background)) => {
                    push(out, &erase_run(row, start, len, background, &palette));
                    run = Some((cell.col, 1, cell.background));
                }
                None => run = Some((cell.col, 1, cell.background)),
            }
        }
        if let Some((start, len, background)) = run {
            push(out, &erase_run(row, start, len, background, &palette));
        }
        push(out, "\x1b[0m");
    }
    Ok(())
}

/// One background run: address it, set the background, erase it.
fn erase_run(
    row: u32,
    col: u16,
    len: u16,
    background: ColorRgb,
    palette: &[ColorRgb; 256],
) -> String {
    format!(
        "\x1b[{};{}H{}\x1b[{}X",
        row + 1,
        col + 1,
        background_sgr(background, palette),
        len,
    )
}

/// The fill colour as SGR, indexed where it can be.
///
/// `RenderedRow` carries backgrounds already resolved through the
/// *server's* palette, and emitting those as truecolour would leave the
/// refilled rows wearing the server's theme while the text rows around
/// them — which pass A emits as `CSI 4x m` — wear the client's. Step 0
/// exists for exactly that, so the resolved colour is looked back up.
/// The ambiguity it buys: a program that set a truecolour background
/// numerically equal to a palette slot replays as that slot. Indexed
/// backgrounds are the common case, and the ones a client theme is meant
/// to re-map.
fn background_sgr(background: ColorRgb, palette: &[ColorRgb; 256]) -> String {
    match palette.iter().position(|slot| *slot == background) {
        Some(index) => format!("\x1b[48;5;{index}m"),
        None => format!(
            "\x1b[48;2;{};{};{}m",
            background.r, background.g, background.b
        ),
    }
}

/// Step 8 — IRM, then origin mode.
///
/// IRM first: it has no cursor side effect and everything that had to
/// print has printed. Origin mode last and with its own re-position,
/// because setting *or* clearing it homes the cursor in libghostty
/// (`stream_terminal.zig`'s `.origin => setCursorPos(1, 1)`), destroying
/// the position pass B just restored — and under origin mode a CUP is
/// region-relative, hence the offsets from the region pass B established.
fn write_deferred_modes(
    terminal: &Terminal,
    row: u32,
    cursor_col: u32,
    out: &mut Vec<u8>,
) -> Result<()> {
    if terminal.ansi_mode_get(4) {
        push(out, "\x1b[4h");
    }
    if terminal.mode_get(6) {
        let (top, left) = scrolling_region_origin(terminal)?;
        push(out, "\x1b[?6h");
        push(
            out,
            &format!(
                "\x1b[{};{}H",
                row.saturating_sub(top) + 1,
                cursor_col.saturating_sub(left) + 1
            ),
        );
    }
    Ok(())
}

/// Top row and left column of the scrolling region, 0-based.
///
/// libghostty exposes the region through exactly one API — the
/// formatter's `scrolling_region` extra — and through no
/// `GHOSTTY_TERMINAL_DATA_*` id, so it is read back differentially: the
/// same one-cell selection is formatted twice, once without the extra
/// and once with it, and the suffix the extra added is the DECSTBM /
/// DECSLRM pair. Differential rather than a scan of the whole output so
/// nothing has to assume what the content half may contain.
///
/// Only the origin-mode branch needs it, so the two extra format calls
/// are paid only when DECOM is set.
fn scrolling_region_origin(terminal: &Terminal) -> Result<(u32, u32)> {
    let anchor = terminal
        .grid_ref(Point::active(0, 0))
        .ok_or(Error::InvalidValue)?;
    let selection = cell_selection(&anchor);
    let bare = format_terminal(terminal, BARE_EXTRA, Some(&selection))?;
    let with_region = format_terminal(terminal, REGION_EXTRA, Some(&selection))?;
    let emitted = with_region.get(bare.len()..).ok_or(Error::InvalidValue)?;

    let (mut top, mut left) = (0, 0);
    for chunk in emitted.split(|byte| *byte == 0x1b) {
        let Some(body) = chunk.strip_prefix(b"[") else {
            continue;
        };
        let Some((final_byte, params)) = body.split_last() else {
            continue;
        };
        let first = params
            .split(|byte| *byte == b';')
            .next()
            .unwrap_or_default();
        let Ok(value) = std::str::from_utf8(first)
            .unwrap_or_default()
            .parse::<u32>()
        else {
            continue;
        };
        match final_byte {
            b'r' => top = value.saturating_sub(1),
            b's' => left = value.saturating_sub(1),
            _ => {}
        }
    }
    Ok((top, left))
}

/// Step 6 — the cursor's own cell as pass B's content.
fn format_cursor_cell(terminal: &Terminal, row: u32, col: u32) -> Result<Vec<u8>> {
    let cell = terminal
        .grid_ref(Point::active(col as u16, row))
        .ok_or(Error::InvalidValue)?;
    let selection = cell_selection(&cell);
    format_terminal(terminal, STATE_EXTRA, Some(&selection))
}

/// DECSCUSR code for a render-state cursor. `BlockHollow` maps to the
/// block codes: DECSCUSR cannot express hollow, which is a
/// focus-dependent render style rather than terminal state.
fn decscusr_code(style: CursorVisualStyle, blinking: bool) -> u8 {
    match (style, blinking) {
        (CursorVisualStyle::Block | CursorVisualStyle::BlockHollow, true) => 1,
        (CursorVisualStyle::Block | CursorVisualStyle::BlockHollow, false) => 2,
        (CursorVisualStyle::Underline, true) => 3,
        (CursorVisualStyle::Underline, false) => 4,
        (CursorVisualStyle::Bar, true) => 5,
        (CursorVisualStyle::Bar, false) => 6,
    }
}

// ============================================================================
// libghostty formatter plumbing
// ============================================================================

/// Pass A: content plus tabstops. The palette and the modes are step 0's
/// and step 1's; the region, keyboard, pwd and screen extras all belong
/// after the pad.
const CONTENT_EXTRA: Extras = Extras {
    scrolling_region: false,
    tabstops: true,
    pwd: false,
    keyboard: false,
    screen: false,
};

/// Pass B: everything that has to land after the content.
const STATE_EXTRA: Extras = Extras {
    scrolling_region: true,
    tabstops: false,
    pwd: true,
    keyboard: true,
    screen: true,
};

const BARE_EXTRA: Extras = Extras {
    scrolling_region: false,
    tabstops: false,
    pwd: false,
    keyboard: false,
    screen: false,
};

const REGION_EXTRA: Extras = Extras {
    scrolling_region: true,
    tabstops: false,
    pwd: false,
    keyboard: false,
    screen: false,
};

/// The formatter extras this module uses, minus the two it never sets:
/// `palette` and `modes` are steps 0 and 1.
#[derive(Clone, Copy)]
struct Extras {
    scrolling_region: bool,
    tabstops: bool,
    pwd: bool,
    keyboard: bool,
    /// All six screen extras together — the payload never wants a subset.
    screen: bool,
}

impl Extras {
    fn to_sys(self) -> sys::GhosttyFormatterTerminalExtra {
        sys::GhosttyFormatterTerminalExtra {
            size: size_of::<sys::GhosttyFormatterTerminalExtra>(),
            palette: false,
            modes: false,
            scrolling_region: self.scrolling_region,
            tabstops: self.tabstops,
            pwd: self.pwd,
            keyboard: self.keyboard,
            screen: sys::GhosttyFormatterScreenExtra {
                size: size_of::<sys::GhosttyFormatterScreenExtra>(),
                cursor: self.screen,
                style: self.screen,
                hyperlink: self.screen,
                protection: self.screen,
                kitty_keyboard: self.screen,
                charsets: self.screen,
            },
        }
    }
}

/// RAII owner of a `ghostty_formatter_terminal_new` handle.
struct Formatter(sys::GhosttyFormatter);

impl Drop for Formatter {
    fn drop(&mut self) {
        // SAFETY: created by `ghostty_formatter_terminal_new` with the
        // default allocator and never freed twice.
        unsafe { sys::ghostty_formatter_free(self.0) };
    }
}

/// Format the terminal once, VT emit, and return the bytes.
///
/// `selection` is `None` for the whole active screen including its
/// scrollback. A `Some` selection's pin is created, used and dropped
/// inside this one synchronous call, for the reason
/// [`crate::formatter`]'s module header gives.
fn format_terminal(
    terminal: &Terminal,
    extras: Extras,
    selection: Option<&sys::GhosttySelection>,
) -> Result<Vec<u8>> {
    let options = sys::GhosttyFormatterTerminalOptions {
        size: size_of::<sys::GhosttyFormatterTerminalOptions>(),
        emit: sys::GhosttyFormatterFormat_GHOSTTY_FORMATTER_FORMAT_VT,
        // One entry per screen row whatever the soft-wrap flags say, and
        // no trailing-space trim: the payload has to reproduce cells, not
        // read nicely.
        unwrap: false,
        trim: false,
        extra: extras.to_sys(),
        selection: selection.map_or(ptr::null(), |selection| selection as *const _),
    };

    let mut handle: sys::GhosttyFormatter = ptr::null_mut();
    // SAFETY: a live terminal handle, the default allocator (null), an
    // options struct whose `selection` pointer outlives the call, and an
    // out-param we own.
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
    // SAFETY: a live formatter handle, the default allocator (the same
    // one `FormatterBuf` frees with), and two stack out-params.
    let rc = unsafe {
        sys::ghostty_formatter_format_alloc(formatter.0, ptr::null(), &mut out_ptr, &mut out_len)
    };
    Error::from_result(rc)?;
    if out_ptr.is_null() {
        return Ok(Vec::new());
    }
    let buf = FormatterBuf {
        ptr: out_ptr,
        len: out_len,
    };
    // SAFETY: libghostty reports the buffer it just allocated; the slice
    // borrow ends before `buf` frees it.
    let bytes = unsafe { std::slice::from_raw_parts(buf.ptr, buf.len) };
    Ok(bytes.to_vec())
}

fn cell_selection(cell: &GridRef) -> sys::GhosttySelection {
    sys::GhosttySelection {
        size: size_of::<sys::GhosttySelection>(),
        start: cell.as_sys(),
        end: cell.as_sys(),
        rectangle: false,
    }
}

/// Rows the content pass emitted. Cells never hold C0 bytes and VT
/// content emits no `OSC 8`, so every `\r\n` in it is a row break.
fn count_crlf(content: &[u8]) -> usize {
    content.windows(2).filter(|pair| *pair == b"\r\n").count()
}

fn osc_color(target: &str, rgb: ColorRgb) -> String {
    format!(
        "\x1b]{};rgb:{:02x}/{:02x}/{:02x}\x1b\\",
        target, rgb.r, rgb.g, rgb.b
    )
}

fn push(out: &mut Vec<u8>, text: &str) {
    out.extend_from_slice(text.as_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decscusr_covers_every_shape_in_both_blink_states() {
        let codes: Vec<u8> = [
            CursorVisualStyle::Block,
            CursorVisualStyle::BlockHollow,
            CursorVisualStyle::Underline,
            CursorVisualStyle::Bar,
        ]
        .into_iter()
        .flat_map(|style| [true, false].map(|blink| decscusr_code(style, blink)))
        .collect();
        assert_eq!(codes, vec![1, 2, 1, 2, 3, 4, 5, 6]);
    }

    #[test]
    fn crlf_pairs_are_counted_but_lone_carriage_returns_are_not() {
        assert_eq!(count_crlf(b"a\r\nb\r\n"), 2);
        assert_eq!(count_crlf(b"a\rb\nc"), 0);
        assert_eq!(count_crlf(b""), 0);
    }
}
