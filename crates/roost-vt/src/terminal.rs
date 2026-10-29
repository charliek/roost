//! Safe wrapper around `ghostty_terminal_*`.
//!
//! `Terminal` owns a `*mut GhosttyTerminalImpl` handle. Construction
//! allocates via `ghostty_terminal_new`; `Drop` releases via
//! `ghostty_terminal_free`. The handle is `Send` (the FFI is thread-safe
//! at the "owned by one thread at a time" level) but explicitly `!Sync`
//! — libghostty-vt must not be touched from more than one thread
//! concurrently, enforcing the main-thread invariant (`CLAUDE.md`
//! "Threading") that the Mac UI's `@MainActor` discipline also keeps.

use std::marker::PhantomData;
use std::os::raw::c_void;
use std::panic::AssertUnwindSafe;
use std::ptr::{self, NonNull};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use crate::program_status::{
    desktop_notification_trampoline, program_status_trampoline, semantic_prompt_trampoline,
};
use crate::sys;
use crate::{Error, Result, TerminalEvent, TrackedRef};

// Compile-time guards on the `ColorRgb` <-> `GhosttyColorRgb` cast in
// `set_color_palette`. If a bindgen regen ever changes the size or
// alignment of `GhosttyColorRgb`, the build breaks here instead of
// silently corrupting palette data at runtime. CodeRabbit flagged
// the cast on PR #50; the assertions are the requested guard.
const _: () =
    assert!(std::mem::size_of::<crate::ColorRgb>() == std::mem::size_of::<sys::GhosttyColorRgb>(),);
const _: () = assert!(
    std::mem::align_of::<crate::ColorRgb>() == std::mem::align_of::<sys::GhosttyColorRgb>(),
);

/// Construction parameters for a new terminal. `cols`/`rows` go to
/// `ghostty_terminal_new`; `max_scrollback` and `continuation_max_bytes`
/// are separate `ghostty_terminal_set` calls the constructor applies
/// right after (upstream retired the options struct that used to carry
/// the sizing and scrollback fields together).
#[derive(Debug, Clone, Copy)]
pub struct TerminalOptions {
    pub cols: u16,
    pub rows: u16,
    /// Number of rows of off-screen scrollback to retain. Both UIs
    /// use 2000.
    pub max_scrollback: usize,
    /// Byte cap on the replay-safe VT continuation libghostty retains
    /// for an escape sequence or UTF-8 codepoint left unfinished by the
    /// most recent write. `0` disables tracking (the libghostty
    /// default). Only a terminal that had tracking enabled *before* the
    /// input that left it unfinished can be snapshotted from a
    /// non-ground state, so this is a construction-time knob; see
    /// [`Terminal::set_continuation_max_bytes`] for the post-hoc setter
    /// and its caveats.
    pub continuation_max_bytes: usize,
}

impl Default for TerminalOptions {
    fn default() -> Self {
        Self {
            cols: 80,
            rows: 24,
            max_scrollback: 0,
            continuation_max_bytes: 0,
        }
    }
}

/// Tag for `Terminal::scroll_viewport`. Mirrors the C-side
/// `GhosttyTerminalScrollViewport` tagged union but hides the `union`
/// layout from Rust callers.
#[derive(Debug, Clone, Copy)]
pub enum ScrollViewport {
    /// Scroll to the very top of the scrollback buffer.
    Top,
    /// Scroll to the bottom (active region). Used by the Mac/Linux UIs
    /// on keystroke to "snap-to-bottom" before encoding the key.
    Bottom,
    /// Scroll by a signed row delta. Negative = up (older history),
    /// positive = down (toward bottom).
    Delta(isize),
}

/// Authoritative scrollable-area state reported by libghostty-vt.
///
/// `offset + len >= total` means the viewport is at the live bottom.  Keep
/// this wrapper layout-independent so callers never depend on bindgen's C
/// struct representation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Scrollbar {
    pub total: u64,
    pub offset: u64,
    pub len: u64,
}

impl Scrollbar {
    pub fn is_at_bottom(self) -> bool {
        self.offset.saturating_add(self.len) >= self.total
    }
}

/// Result of `Terminal::active_screen()`. The Mac UI's scroll handler
/// uses this to decide between local scrollback and alt-screen arrow
/// translation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActiveScreen {
    Primary,
    Alternate,
}

#[cfg(feature = "ffi")]
impl ActiveScreen {
    /// Map a `GhosttyTerminalScreen`. Anything other than the alternate
    /// screen — including a variant a future Ghostty adds — collapses to
    /// primary, which is the safer fallback for every caller.
    pub(crate) fn from_sys(raw: sys::GhosttyTerminalScreen) -> Self {
        if raw == sys::GhosttyTerminalScreen_GHOSTTY_TERMINAL_SCREEN_ALTERNATE {
            Self::Alternate
        } else {
            Self::Primary
        }
    }
}

/// One of libghostty's three dynamic colors — the slots `OSC 10`, `OSC
/// 11` and `OSC 12` address, each of which has both an effective and a
/// default value ([`Terminal::color`] / [`Terminal::default_color`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalColor {
    Foreground,
    Background,
    Cursor,
}

#[cfg(feature = "ffi")]
impl TerminalColor {
    /// The OSC number that carries this color, so a caller emitting an
    /// override does not re-derive the 10/11/12 mapping.
    pub fn osc_code(self) -> u16 {
        match self {
            Self::Foreground => 10,
            Self::Background => 11,
            Self::Cursor => 12,
        }
    }

    fn effective_data_id(self) -> sys::GhosttyTerminalData {
        match self {
            Self::Foreground => sys::GhosttyTerminalData_GHOSTTY_TERMINAL_DATA_COLOR_FOREGROUND,
            Self::Background => sys::GhosttyTerminalData_GHOSTTY_TERMINAL_DATA_COLOR_BACKGROUND,
            Self::Cursor => sys::GhosttyTerminalData_GHOSTTY_TERMINAL_DATA_COLOR_CURSOR,
        }
    }

    fn default_data_id(self) -> sys::GhosttyTerminalData {
        match self {
            Self::Foreground => {
                sys::GhosttyTerminalData_GHOSTTY_TERMINAL_DATA_COLOR_FOREGROUND_DEFAULT
            }
            Self::Background => {
                sys::GhosttyTerminalData_GHOSTTY_TERMINAL_DATA_COLOR_BACKGROUND_DEFAULT
            }
            Self::Cursor => sys::GhosttyTerminalData_GHOSTTY_TERMINAL_DATA_COLOR_CURSOR_DEFAULT,
        }
    }
}

/// Which coordinate space a [`Point`] is interpreted in. Mirrors
/// `GhosttyPointTag` 1:1.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PointTag {
    /// Active region — where the cursor can move. 0 = top of the
    /// active region; scrollback rows are not addressable.
    Active,
    /// Visible viewport. 0 = top of what's currently on screen;
    /// changes as the user scrolls.
    Viewport,
    /// Full screen including scrollback. 0 = top of scrollback, so a
    /// screen coordinate names a *position*, not a piece of content:
    /// once `max_scrollback` saturates, every evicted row shifts all
    /// stored screen coordinates down by one relative to the content
    /// they named. Fine for a coordinate translated and used inside one
    /// synchronous call; not a place to store a long-lived endpoint.
    /// For that use [`Terminal::track`], whose refs libghostty rewrites
    /// so they keep naming the same content on both axes.
    Screen,
    /// Scrollback history only — the area above the active region.
    /// 0 = top of scrollback.
    History,
}

#[cfg(feature = "ffi")]
impl PointTag {
    pub(crate) fn to_sys(self) -> sys::GhosttyPointTag {
        match self {
            PointTag::Active => sys::GhosttyPointTag_GHOSTTY_POINT_TAG_ACTIVE,
            PointTag::Viewport => sys::GhosttyPointTag_GHOSTTY_POINT_TAG_VIEWPORT,
            PointTag::Screen => sys::GhosttyPointTag_GHOSTTY_POINT_TAG_SCREEN,
            PointTag::History => sys::GhosttyPointTag_GHOSTTY_POINT_TAG_HISTORY,
        }
    }
}

/// A grid coordinate interpreted under a specific [`PointTag`].
/// `y` is `u32` because `PointTag::Screen` indices grow with scrollback
/// and can exceed `u16` for long-running sessions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Point {
    pub tag: PointTag,
    pub x: u16,
    pub y: u32,
}

impl Point {
    pub fn active(x: u16, y: u32) -> Self {
        Self {
            tag: PointTag::Active,
            x,
            y,
        }
    }
    pub fn viewport(x: u16, y: u32) -> Self {
        Self {
            tag: PointTag::Viewport,
            x,
            y,
        }
    }
    pub fn screen(x: u16, y: u32) -> Self {
        Self {
            tag: PointTag::Screen,
            x,
            y,
        }
    }
    pub fn history(x: u16, y: u32) -> Self {
        Self {
            tag: PointTag::History,
            x,
            y,
        }
    }
}

#[cfg(feature = "ffi")]
impl Point {
    pub(crate) fn to_sys(self) -> sys::GhosttyPoint {
        sys::GhosttyPoint {
            tag: self.tag.to_sys(),
            value: sys::GhosttyPointValue {
                coordinate: sys::GhosttyPointCoordinate {
                    x: self.x,
                    y: self.y,
                },
            },
        }
    }
}

/// Opaque reference to a position in the terminal's internal page
/// structure, obtained via [`Terminal::grid_ref`].
///
/// # Transience
///
/// **A `GridRef` is only valid until the next update to the terminal
/// it was taken from.** Any `vt_write`, `resize`, `reset`, or other
/// mutating call may invalidate it. Per libghostty's C documentation,
/// "there is no guarantee that a grid reference will remain valid
/// after ANY operation, even if a seemingly unrelated part of the grid
/// is changed."
///
/// For long-lived position tracking (e.g. selection state), do not
/// store `GridRef` directly — use [`Terminal::track`], whose
/// [`crate::TrackedRef`] libghostty rewrites as the page list scrolls,
/// prunes, and reflows.
#[cfg(feature = "ffi")]
#[derive(Debug, Clone, Copy)]
pub struct GridRef(sys::GhosttyGridRef);

#[cfg(feature = "ffi")]
impl GridRef {
    pub(crate) fn from_sys(raw: sys::GhosttyGridRef) -> Self {
        Self(raw)
    }

    /// Raw pin, for in-crate wrappers that hand it straight back to
    /// libghostty (see `formatter::selection_text`). Same transience
    /// contract as the type itself.
    pub(crate) fn as_sys(&self) -> sys::GhosttyGridRef {
        self.0
    }
}

pub struct Terminal {
    handle: sys::GhosttyTerminal,
    /// Leaked from a `Box` at construction and reclaimed in `Drop` after
    /// the handle is freed. Boxed so the pointer libghostty holds as
    /// `OPT_USERDATA` survives moves of the `Terminal`.
    ctx: NonNull<CallbackCtx>,
    /// `!Sync` marker — libghostty-vt is single-threaded. Using
    /// `*const ()` makes the type `Send + !Sync`, which matches the
    /// Mac UI's `@MainActor`-only contract.
    _not_sync: PhantomData<std::cell::Cell<()>>,
}

/// The one `GHOSTTY_TERMINAL_OPT_USERDATA` value every callback receives.
/// libghostty has a single userdata slot shared by all callbacks
/// (`terminal.h`, "You cannot specify different userdata for different
/// callbacks"), so each callback's sink is a field here, and the slot
/// points at this context from construction until `Drop` — clearing one
/// callback never touches it. A field that is `None` while its callback
/// is still set is harmless: the trampoline does nothing.
///
/// # Drain contract (both sinks)
///
/// libghostty runs the callbacks synchronously inside the `Terminal`
/// call that triggered them, and the trampolines lock the caller's sink.
/// So a caller MUST NOT hold a sink's lock across [`Terminal::vt_write`],
/// [`Terminal::resize`], or a theme push: a held guard self-deadlocks.
/// Lock only to take the contents.
///
/// * The `write_pty` buffer fills from `vt_write` **and** `resize`
///   (mode 2048 in-band size reports fire inside
///   `ghostty_terminal_resize`), so drain it after every call of either.
/// * The status sink fills only from `vt_write` (OSC 7501, OSC 133, OSC
///   9 / 777, and RIS's `clear`); drain it after every `vt_write`, before acting on
///   anything else that write produced, to keep reports and marks in
///   byte order.
#[derive(Default)]
pub(crate) struct CallbackCtx {
    pub(crate) write_pty: Mutex<Option<Arc<Mutex<Vec<u8>>>>>,
    pub(crate) status: Mutex<Option<Arc<Mutex<Vec<TerminalEvent>>>>>,
}

const WRITE_PTY: sys::GhosttyTerminalOption =
    sys::GhosttyTerminalOption_GHOSTTY_TERMINAL_OPT_WRITE_PTY;
const PROGRAM_STATUS: sys::GhosttyTerminalOption =
    sys::GhosttyTerminalOption_GHOSTTY_TERMINAL_OPT_PROGRAM_STATUS;
const SEMANTIC_PROMPT: sys::GhosttyTerminalOption =
    sys::GhosttyTerminalOption_GHOSTTY_TERMINAL_OPT_SEMANTIC_PROMPT;
const DESKTOP_NOTIFICATION: sys::GhosttyTerminalOption =
    sys::GhosttyTerminalOption_GHOSTTY_TERMINAL_OPT_DESKTOP_NOTIFICATION;
const STATUS_CALLBACKS: [sys::GhosttyTerminalOption; 3] =
    [PROGRAM_STATUS, SEMANTIC_PROMPT, DESKTOP_NOTIFICATION];

// The trampolines are set as `*const c_void`; these pin each one to the
// callback type bindgen generated for its slot.
const _: sys::GhosttyTerminalWritePtyFn = Some(write_pty_trampoline);
const _: sys::GhosttyTerminalProgramStatusFn = Some(program_status_trampoline);
const _: sys::GhosttyTerminalSemanticPromptFn = Some(semantic_prompt_trampoline);
const _: sys::GhosttyTerminalDesktopNotificationFn = Some(desktop_notification_trampoline);

/// Lock poison-tolerantly: a panic in another holder must never surface
/// as an unwind across an `extern "C"` frame.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Append `items` to the sink in `slot`, if one is set.
fn append<T>(slot: &Mutex<Option<Arc<Mutex<Vec<T>>>>>, items: impl IntoIterator<Item = T>) {
    let slot = lock(slot);
    let Some(sink) = slot.as_ref() else {
        return;
    };
    lock(sink).extend(items);
}

/// Run a trampoline's body, dropping its event if anything in it panics:
/// a panic reaching an `extern "C"` frame aborts the process, and code a
/// trampoline can't vouch for runs inside it (a `tracing` subscriber).
pub(crate) fn contain(body: impl FnOnce()) {
    let _ = std::panic::catch_unwind(AssertUnwindSafe(body));
}

impl CallbackCtx {
    pub(crate) fn push_status(&self, event: TerminalEvent) {
        append(&self.status, std::iter::once(event));
    }
}

/// Trampoline installed as `GHOSTTY_TERMINAL_OPT_WRITE_PTY`. libghostty
/// invokes this synchronously from inside `ghostty_terminal_vt_write`
/// **and** `ghostty_terminal_resize` (mode 2048 in-band size reports).
///
/// Hygiene contract (all load-bearing for FFI soundness), shared by every
/// trampoline:
/// * Guards null pointers and `len == 0` — early return.
/// * Poison-tolerant locks, no `unwrap`/`expect`/indexing, and the body
///   runs under [`contain`], so a panic drops the event instead of
///   unwinding into this `extern "C"` frame. Out of memory aborts, as it
///   does everywhere else in Rust.
/// * Append-only. It must **never** call back into libghostty
///   (`terminal.h`'s no-reentrancy contract).
unsafe extern "C" fn write_pty_trampoline(
    _terminal: sys::GhosttyTerminal,
    userdata: *mut c_void,
    data: *const u8,
    len: usize,
) {
    contain(|| {
        if userdata.is_null() || data.is_null() || len == 0 {
            return;
        }
        // SAFETY: `userdata` is the `Terminal`'s `CallbackCtx`, set from
        // construction and freed only after the terminal handle is.
        let ctx = unsafe { &*userdata.cast::<CallbackCtx>() };
        // SAFETY: libghostty guarantees `data`/`len` describe a valid,
        // initialized byte range for the duration of this call.
        let bytes = unsafe { std::slice::from_raw_parts(data, len) };
        append(&ctx.write_pty, bytes.iter().copied());
    });
}

// SAFETY: the underlying libghostty-vt handle can move between threads
// as long as only one thread touches it at a time, and `ctx` is an owned
// allocation of `Send + Sync` fields that moves with it. Sync is
// intentionally not implemented (enforced via PhantomData above).
unsafe impl Send for Terminal {}

impl Terminal {
    /// Allocate a fresh terminal. Mirrors `ghostty_terminal_new(NULL,
    /// &out, cols, rows)` and panics-free: any non-success result is
    /// returned as an `Error`.
    pub fn new(options: TerminalOptions) -> Result<Self> {
        let mut handle: sys::GhosttyTerminal = ptr::null_mut();
        // SAFETY: passing a null allocator (libghostty's default) and an
        // out-pointer we own.
        let rc = unsafe {
            sys::ghostty_terminal_new(ptr::null_mut(), &mut handle, options.cols, options.rows)
        };
        Error::from_result(rc)?;
        if handle.is_null() {
            return Err(Error::NullHandle);
        }
        // Construct the RAII owner *before* configuring scrollback so a
        // failed `set` frees the terminal instead of leaking it.
        // SAFETY: non-null, and freshly allocated, so uniquely owned.
        let mut terminal = unsafe { Self::adopt(handle) };
        // Always applied, including `0` (= scrollback disabled):
        // otherwise the terminal keeps libghostty's own default limit,
        // not the one the caller asked for.
        // SAFETY: handle non-null; `options.max_scrollback` is a live
        // local of the `size_t` type this option documents.
        let rc = unsafe {
            sys::ghostty_terminal_set(
                terminal.handle,
                sys::GhosttyTerminalOption_GHOSTTY_TERMINAL_OPT_SCROLLBACK_MAX_LINES,
                (&raw const options.max_scrollback).cast(),
            )
        };
        Error::from_result(rc)?;
        // `ghostty_terminal_new` also installs a default *byte* limit
        // (10 KB at this pin) that wins over the line limit whenever it
        // is reached first — which is always, at these magnitudes. Clear
        // it (NULL removes the byte limit) so `max_scrollback` lines is
        // the one limit in force.
        // SAFETY: handle non-null; NULL is the documented "remove" value.
        let rc = unsafe {
            sys::ghostty_terminal_set(
                terminal.handle,
                sys::GhosttyTerminalOption_GHOSTTY_TERMINAL_OPT_SCROLLBACK_MAX_BYTES,
                ptr::null(),
            )
        };
        Error::from_result(rc)?;
        // Always applied, including `0`, for the same reason as the
        // scrollback limit above: leaving the `set` out would take
        // libghostty's own default rather than the caller's choice.
        terminal.set_continuation_max_bytes(options.continuation_max_bytes)?;
        Ok(terminal)
    }

    /// Adopt a terminal handle produced by
    /// `ghostty_snapshot_decoder_ready` (`snapshot.h:423-460`), which
    /// hands back a **caller-owned** terminal that the decoder then
    /// borrows while it restores history.
    ///
    /// # Safety
    ///
    /// * `handle` must be non-null and must come from a `GHOSTTY_SUCCESS`
    ///   return of `ghostty_snapshot_decoder_ready`.
    /// * Ownership transfers here and must be unique: nothing else may
    ///   free the handle, and no second `Terminal` may wrap it.
    /// * The decoder that produced the handle borrows it until FINISH or
    ///   until it is freed, so that decoder must be freed **before** this
    ///   `Terminal` is dropped. [`crate::SnapshotDecoder`]'s `Drop` and
    ///   its `finish`/`abandon` paths are what guarantee that ordering.
    pub(crate) unsafe fn from_decoded(handle: sys::GhosttyTerminal) -> Self {
        // SAFETY: the caller's contract is `adopt`'s.
        unsafe { Self::adopt(handle) }
    }

    /// Wrap a handle and point its `OPT_USERDATA` at a fresh
    /// [`CallbackCtx`]. Every construction path goes through here.
    ///
    /// # Safety
    ///
    /// `handle` must be non-null and owned by nothing else.
    unsafe fn adopt(handle: sys::GhosttyTerminal) -> Self {
        let ctx = NonNull::from(Box::leak(Box::new(CallbackCtx::default())));
        let mut terminal = Self {
            handle,
            ctx,
            _not_sync: PhantomData,
        };
        // libghostty rejects a `set` only for a null terminal, which this
        // function's contract rules out.
        let _ = terminal.set_opt(
            sys::GhosttyTerminalOption_GHOSTTY_TERMINAL_OPT_USERDATA,
            ctx.as_ptr().cast_const().cast(),
        );
        terminal
    }

    fn ctx(&self) -> &CallbackCtx {
        // SAFETY: `ctx` is live from `adopt` until `Drop`, and only ever
        // shared — no `&mut` to it exists before `Drop` reclaims it.
        unsafe { self.ctx.as_ref() }
    }

    /// Set the byte cap on retained VT continuation bytes; `0` disables
    /// tracking. HS-1's decode path needs this to zero tracking after a
    /// continuation export (`snapshot.h`'s cleanup rule for
    /// `RETAIN_CONTINUATION`).
    ///
    /// Raising tracking while the parser is already unfinished — or
    /// lowering the cap below an already-retained continuation — makes
    /// the *current* continuation unavailable, because the earlier bytes
    /// were never recorded. Tracking recovers on the next write that
    /// reaches ground. Prefer [`TerminalOptions::continuation_max_bytes`]
    /// when the terminal is yours to construct.
    pub fn set_continuation_max_bytes(&mut self, max_bytes: usize) -> Result<()> {
        // SAFETY: handle non-null; `max_bytes` is a live local of the
        // `size_t` type this option documents.
        let rc = unsafe {
            sys::ghostty_terminal_set(
                self.handle,
                sys::GhosttyTerminalOption_GHOSTTY_TERMINAL_OPT_CONTINUATION_MAX_BYTES,
                (&raw const max_bytes).cast(),
            )
        };
        Error::from_result(rc)
    }

    /// Read back the configured continuation byte cap. `0` means
    /// tracking is disabled.
    pub fn continuation_max_bytes(&self) -> Result<usize> {
        let mut out: usize = 0;
        // SAFETY: handle non-null; `out` is a real local of the `size_t`
        // type this data key documents.
        let rc = unsafe {
            sys::ghostty_terminal_get(
                self.handle,
                sys::GhosttyTerminalData_GHOSTTY_TERMINAL_DATA_CONTINUATION_MAX_BYTES,
                (&mut out) as *mut usize as *mut _,
            )
        };
        Error::from_result(rc)?;
        Ok(out)
    }

    /// The replay-safe VT continuation: the exact byte suffix that
    /// reconstructs whatever escape sequence or UTF-8 codepoint the last
    /// write left unfinished, so a terminal fed those bytes lands in the
    /// same parser state.
    ///
    /// `Ok(None)` and `Ok(Some(vec![]))` are different answers and
    /// callers depend on the difference. `None` is *unavailable* —
    /// tracking is off, or the unfinished input outran the retained cap
    /// ([`Self::set_continuation_max_bytes`]) — and nothing can be said
    /// about the parser. An empty vector is the parser at ground, with
    /// nothing to carry.
    pub fn continuation(&self) -> Result<Option<Vec<u8>>> {
        let mut needed: usize = 0;
        // SAFETY: handle non-null; a null buffer with zero capacity is
        // the size query this call documents, and `needed` is a real
        // local.
        let rc = unsafe {
            sys::ghostty_terminal_continuation_buf(self.handle, ptr::null_mut(), 0, &mut needed)
        };
        match Error::from_result(rc) {
            // A size query answers OUT_OF_SPACE even at ground, where
            // the size it reports is zero.
            Ok(()) | Err(Error::OutOfSpace) => {}
            Err(Error::InvalidValue) => return Ok(None),
            Err(err) => return Err(err),
        }
        if needed == 0 {
            return Ok(Some(Vec::new()));
        }

        let mut buf = vec![0_u8; needed];
        let mut written: usize = 0;
        // SAFETY: handle non-null; `buf` is a live allocation of exactly
        // the length passed, and `written` is a real local.
        let rc = unsafe {
            sys::ghostty_terminal_continuation_buf(
                self.handle,
                buf.as_mut_ptr(),
                buf.len(),
                &mut written,
            )
        };
        Error::from_result(rc)?;
        buf.truncate(written);
        Ok(Some(buf))
    }

    /// Raw FFI handle. Pass-through for crates that need to call a
    /// not-yet-wrapped symbol (e.g. `crates/roost-iced/`'s key encoder
    /// sync). Stays `pub(crate)` deliberately — internal modules use
    /// this; external code goes through [`Self::as_ffi`].
    pub(crate) fn handle(&self) -> sys::GhosttyTerminal {
        self.handle
    }

    /// Public escape-hatch accessor. Use only when no safe wrapper
    /// covers your call yet; prefer adding one over reaching for this.
    pub fn as_ffi(&self) -> sys::GhosttyTerminal {
        self.handle
    }

    /// Reset the terminal to its initial state. Used after a clear /
    /// shell restart so attrs and modes go back to defaults.
    pub fn reset(&mut self) {
        // SAFETY: handle is non-null (constructor enforces) and reset
        // is documented as never failing.
        unsafe { sys::ghostty_terminal_reset(self.handle) };
    }

    /// Feed VT bytes into the parser. Idempotent across split chunks —
    /// the parser holds its own state.
    pub fn vt_write(&mut self, data: &[u8]) {
        if data.is_empty() {
            return;
        }
        // SAFETY: libghostty-vt copies the bytes internally; the slice
        // lifetime ending after this call is fine.
        unsafe { sys::ghostty_terminal_vt_write(self.handle, data.as_ptr(), data.len()) };
    }

    /// Resize the grid + report cell pixel metrics. Cell pixels go to
    /// libghostty so its OSC 14 / size-report responses are accurate.
    pub fn resize(
        &mut self,
        cols: u16,
        rows: u16,
        cell_width_px: u32,
        cell_height_px: u32,
    ) -> Result<()> {
        // SAFETY: handle non-null per constructor.
        let rc = unsafe {
            sys::ghostty_terminal_resize(self.handle, cols, rows, cell_width_px, cell_height_px)
        };
        Error::from_result(rc)
    }

    /// Install `buf` as the sink for engine-emitted device-query
    /// replies. libghostty's `write_pty` effects callback fires from
    /// inside `vt_write` / `resize` and the [`write_pty_trampoline`]
    /// appends every emitted byte to `buf`; the caller drains it per
    /// [`CallbackCtx`]'s drain contract. This is what makes DA1/DA2, DSR,
    /// DECRQM, XTVERSION, the Kitty keyboard query, the OSC 7501 probe,
    /// and mode-2048 in-band size reports actually reach the PTY.
    ///
    /// Calling again replaces the buffer; the old `Arc` is dropped. If
    /// the callback can't be installed, nothing is left installed.
    pub fn set_write_pty_buffer(&mut self, buf: Arc<Mutex<Vec<u8>>>) -> Result<()> {
        *lock(&self.ctx().write_pty) = Some(buf);
        self.set_callbacks(&[(WRITE_PTY, write_pty_trampoline as *const c_void)])
            .inspect_err(|_| *lock(&self.ctx().write_pty) = None)
    }

    /// Uninstall the `write_pty` callback, then drop the reply buffer.
    /// The status sink, and `OPT_USERDATA`, are untouched.
    pub fn clear_write_pty(&mut self) -> Result<()> {
        self.clear_callbacks([WRITE_PTY])?;
        *lock(&self.ctx().write_pty) = None;
        Ok(())
    }

    /// Install `sink` as the receiver of OSC 7501 program-status reports,
    /// OSC 133 prompt marks **and** the places of OSC 9 / 777 desktop
    /// notifications. All three callbacks push into the one sink, so it
    /// holds them in the order libghostty parsed them; drain it per
    /// [`CallbackCtx`]'s drain contract.
    ///
    /// Setting the program-status callback is also what makes libghostty
    /// answer the `OSC 7501 ; ?` support probe (through `write_pty`).
    ///
    /// Calling again replaces the sink; the old `Arc` is dropped. If any
    /// callback can't be installed, none is left installed.
    pub fn set_status_sink(&mut self, sink: Arc<Mutex<Vec<TerminalEvent>>>) -> Result<()> {
        *lock(&self.ctx().status) = Some(sink);
        self.set_callbacks(&[
            (PROGRAM_STATUS, program_status_trampoline as *const c_void),
            (SEMANTIC_PROMPT, semantic_prompt_trampoline as *const c_void),
            (
                DESKTOP_NOTIFICATION,
                desktop_notification_trampoline as *const c_void,
            ),
        ])
        .inspect_err(|_| *lock(&self.ctx().status) = None)
    }

    /// Uninstall the status callbacks, then drop the sink. The
    /// `write_pty` buffer, and `OPT_USERDATA`, are untouched.
    pub fn clear_status_sink(&mut self) -> Result<()> {
        self.clear_callbacks(STATUS_CALLBACKS)?;
        *lock(&self.ctx().status) = None;
        Ok(())
    }

    /// Scroll the viewport per the given behavior. Returning `()` is
    /// intentional — the C call has no return code.
    pub fn scroll_viewport(&mut self, behavior: ScrollViewport) {
        let viewport = match behavior {
            ScrollViewport::Top => sys::GhosttyTerminalScrollViewport {
                tag: sys::GhosttyTerminalScrollViewportTag_GHOSTTY_SCROLL_VIEWPORT_TOP,
                value: sys::GhosttyTerminalScrollViewportValue { delta: 0 },
            },
            ScrollViewport::Bottom => sys::GhosttyTerminalScrollViewport {
                tag: sys::GhosttyTerminalScrollViewportTag_GHOSTTY_SCROLL_VIEWPORT_BOTTOM,
                value: sys::GhosttyTerminalScrollViewportValue { delta: 0 },
            },
            ScrollViewport::Delta(d) => sys::GhosttyTerminalScrollViewport {
                tag: sys::GhosttyTerminalScrollViewportTag_GHOSTTY_SCROLL_VIEWPORT_DELTA,
                value: sys::GhosttyTerminalScrollViewportValue { delta: d },
            },
        };
        // SAFETY: handle non-null; viewport struct is stack-owned.
        unsafe { sys::ghostty_terminal_scroll_viewport(self.handle, viewport) };
    }

    /// Return the terminal's current viewport position within its scrollable
    /// rows. This is the authoritative way to distinguish a partial scroll
    /// toward the bottom from actually reaching the live region.
    pub fn scrollbar(&self) -> Result<Scrollbar> {
        let mut out = sys::GhosttyTerminalScrollbar {
            total: 0,
            offset: 0,
            len: 0,
        };
        // SAFETY: handle is non-null and `out` is a correctly typed local for
        // GHOSTTY_TERMINAL_DATA_SCROLLBAR.
        let rc = unsafe {
            sys::ghostty_terminal_get(
                self.handle,
                sys::GhosttyTerminalData_GHOSTTY_TERMINAL_DATA_SCROLLBAR,
                (&mut out) as *mut sys::GhosttyTerminalScrollbar as *mut _,
            )
        };
        Error::from_result(rc)?;
        Ok(Scrollbar {
            total: out.total,
            offset: out.offset,
            len: out.len,
        })
    }

    /// Total rows of the active screen — its scrollback plus its active
    /// area.
    pub fn total_rows(&self) -> Result<u64> {
        let mut out: usize = 0;
        // SAFETY: handle is non-null and `out` is a real local of the
        // `size_t` type GHOSTTY_TERMINAL_DATA_TOTAL_ROWS documents.
        let rc = unsafe {
            sys::ghostty_terminal_get(
                self.handle,
                sys::GhosttyTerminalData_GHOSTTY_TERMINAL_DATA_TOTAL_ROWS,
                (&mut out) as *mut usize as *mut _,
            )
        };
        Error::from_result(rc)?;
        Ok(out as u64)
    }

    /// Read a DEC-private mode bit (e.g. mode 2004 for bracketed paste).
    /// Returns `false` if the mode is not currently set or if the mode
    /// number is unknown to libghostty. [`Self::ansi_mode_get`] is the
    /// ANSI half.
    pub fn mode_get(&self, mode: u16) -> bool {
        // `mode` is packed DEC-private (ANSI bit clear) exactly as the
        // removed `ghostty_terminal_mode_get` took it; the caller-facing
        // `u16` mode numbering is unchanged.
        let mut cfg = sys::GhosttyTerminalModeConfig {
            mode: mode as _,
            value: false,
        };
        // SAFETY: handle non-null; cfg is a real local of the type
        // GHOSTTY_TERMINAL_DATA_MODE documents as its in/out parameter.
        let rc = unsafe {
            sys::ghostty_terminal_get(
                self.handle,
                sys::GhosttyTerminalData_GHOSTTY_TERMINAL_DATA_MODE,
                (&mut cfg) as *mut sys::GhosttyTerminalModeConfig as *mut _,
            )
        };
        // Treat any non-success as "false" — the Mac UI does the same.
        Error::from_result(rc).ok();
        cfg.value
    }

    /// Read an ANSI (non-DEC-private) mode bit — IRM is ANSI mode 4,
    /// where DECOM is DEC-private mode 6.
    ///
    /// libghostty distinguishes the two families by packing the ANSI
    /// flag into bit 15 of `GhosttyMode` (`ghostty_mode_new(value,
    /// ansi)`, `modes.h`), so an ANSI read is [`Self::mode_get`] with
    /// that bit set. Same `false` fallback for an unknown mode.
    pub fn ansi_mode_get(&self, mode: u16) -> bool {
        self.mode_get(mode | 0x8000)
    }

    /// Encode the canonical xterm focus report when DEC mode 1004 is active.
    /// Returns no bytes when the mode is disabled or libghostty rejects the
    /// request, allowing UI adapters to forward the result directly.
    pub fn encode_focus(&self, focused: bool) -> Vec<u8> {
        if !self.mode_get(1004) {
            return Vec::new();
        }
        let event = if focused {
            sys::GhosttyFocusEvent_GHOSTTY_FOCUS_GAINED
        } else {
            sys::GhosttyFocusEvent_GHOSTTY_FOCUS_LOST
        };
        let mut buffer = [0_u8; 8];
        let mut written = 0_usize;
        // SAFETY: both output pointers refer to live stack values and the
        // supplied capacity is exactly the backing buffer's size.
        let rc = unsafe {
            sys::ghostty_focus_encode(
                event,
                buffer.as_mut_ptr().cast(),
                buffer.len(),
                &mut written,
            )
        };
        if Error::from_result(rc).is_err() || written == 0 || written > buffer.len() {
            return Vec::new();
        }
        buffer[..written].to_vec()
    }

    /// True if the active screen is the alternate buffer (vim, less,
    /// htop, etc.). The Linux/Mac UIs use this to decide between local
    /// scrollback and arrow-key translation for the wheel.
    pub fn active_screen(&self) -> ActiveScreen {
        let mut out: sys::GhosttyTerminalScreen =
            sys::GhosttyTerminalScreen_GHOSTTY_TERMINAL_SCREEN_PRIMARY;
        // SAFETY: handle non-null; out is a real local.
        let rc = unsafe {
            sys::ghostty_terminal_get(
                self.handle,
                sys::GhosttyTerminalData_GHOSTTY_TERMINAL_DATA_ACTIVE_SCREEN,
                (&mut out) as *mut sys::GhosttyTerminalScreen as *mut _,
            )
        };
        if Error::from_result(rc).is_err() {
            return ActiveScreen::Primary;
        }
        ActiveScreen::from_sys(out)
    }

    /// True if the app has enabled any mouse-tracking mode (X10 /
    /// normal / button / any-event via DECSET 1000/1002/1003). The
    /// Linux/Mac UIs use this to decide whether the scroll wheel should
    /// be encoded as a button-4/5 report instead of scrolling the local
    /// viewport. Mirrors the Mac `isMouseTrackingActive`.
    pub fn mouse_tracking(&self) -> bool {
        let mut active: bool = false;
        // SAFETY: handle non-null; out is a real local.
        let rc = unsafe {
            sys::ghostty_terminal_get(
                self.handle,
                sys::GhosttyTerminalData_GHOSTTY_TERMINAL_DATA_MOUSE_TRACKING,
                (&mut active) as *mut bool as *mut _,
            )
        };
        // Any non-success collapses to "not tracking" — the scroll
        // handler then falls back to local scrollback / alt-screen.
        if Error::from_result(rc).is_err() {
            return false;
        }
        active
    }

    /// The terminal's working directory as last reported by an escape
    /// sequence — OSC 7 (`file://` URI), OSC 9 (ConEmu `CurrentDir`), or
    /// OSC 1337 `CurrentDir` (iTerm2). `Ok(None)` is "unset" (including
    /// an explicit clear via an empty OSC 7), not a failure — libghostty
    /// documents GHOSTTY_TERMINAL_DATA_PWD's zero-length string as
    /// exactly that, mirroring [`Self::continuation`]'s `None`/`Some`
    /// split.
    ///
    /// libghostty stores whatever bytes the shell emitted, without
    /// parsing, and hands back a pointer valid only until the next
    /// mutating terminal call — copied into an owned `String` before
    /// returning. Malformed UTF-8 is replaced via lossy conversion
    /// rather than surfaced as an error: this value is informational
    /// (mirrored into the vt-dump payload / UI title), not something
    /// downstream code parses.
    pub fn pwd(&self) -> Result<Option<String>> {
        let mut out = sys::GhosttyString {
            ptr: ptr::null(),
            len: 0,
        };
        // SAFETY: handle is non-null and `out` is a real local of the
        // `GhosttyString` type GHOSTTY_TERMINAL_DATA_PWD documents as
        // its out parameter.
        let rc = unsafe {
            sys::ghostty_terminal_get(
                self.handle,
                sys::GhosttyTerminalData_GHOSTTY_TERMINAL_DATA_PWD,
                (&mut out) as *mut sys::GhosttyString as *mut _,
            )
        };
        Error::from_result(rc)?;
        if out.len == 0 {
            return Ok(None);
        }
        // SAFETY: libghostty guarantees `ptr` is valid for `len` bytes
        // until the next mutating call, and we make none before the
        // copy below.
        let bytes = unsafe { std::slice::from_raw_parts(out.ptr, out.len) };
        Ok(Some(String::from_utf8_lossy(bytes).into_owned()))
    }

    /// Read libghostty's currently-effective default colors — i.e. the
    /// values an OSC `]10;?` / `]11;?` / `]12;?` query should answer.
    /// Returns the post-OSC-override view: if the app has set the bg
    /// via `OSC 11;rgb:…`, the new value wins; otherwise the theme's
    /// `set_color_background` push is what we return.
    ///
    /// Cursor is `Option<ColorRgb>` because libghostty can leave it
    /// unset (then the renderer falls back to the theme's cursor
    /// color); fg/bg are required and surface `Error::NoValue` if the
    /// theme push hasn't happened — caller chooses how to fall back.
    pub fn live_colors(&self) -> Result<crate::Colors> {
        use crate::ColorRgb;
        let mut fg = sys::GhosttyColorRgb { r: 0, g: 0, b: 0 };
        // SAFETY: handle non-null; fg is a real local.
        let rc = unsafe {
            sys::ghostty_terminal_get(
                self.handle,
                sys::GhosttyTerminalData_GHOSTTY_TERMINAL_DATA_COLOR_FOREGROUND,
                (&mut fg) as *mut sys::GhosttyColorRgb as *mut _,
            )
        };
        Error::from_result(rc)?;
        let mut bg = sys::GhosttyColorRgb { r: 0, g: 0, b: 0 };
        // SAFETY: handle non-null; bg is a real local.
        let rc = unsafe {
            sys::ghostty_terminal_get(
                self.handle,
                sys::GhosttyTerminalData_GHOSTTY_TERMINAL_DATA_COLOR_BACKGROUND,
                (&mut bg) as *mut sys::GhosttyColorRgb as *mut _,
            )
        };
        Error::from_result(rc)?;
        // Cursor may be unset — collapse `NoValue` to `None` so the
        // caller can fall back to the theme without an error path.
        let mut cur = sys::GhosttyColorRgb { r: 0, g: 0, b: 0 };
        // SAFETY: handle non-null; cur is a real local.
        let rc = unsafe {
            sys::ghostty_terminal_get(
                self.handle,
                sys::GhosttyTerminalData_GHOSTTY_TERMINAL_DATA_COLOR_CURSOR,
                (&mut cur) as *mut sys::GhosttyColorRgb as *mut _,
            )
        };
        let cursor = match Error::from_result(rc) {
            Ok(()) => Some(ColorRgb::new(cur.r, cur.g, cur.b)),
            Err(Error::NoValue) => None,
            Err(err) => return Err(err),
        };
        Ok(crate::Colors {
            foreground: ColorRgb::new(fg.r, fg.g, fg.b),
            background: ColorRgb::new(bg.r, bg.g, bg.b),
            cursor,
        })
    }

    /// Read libghostty's live 256-entry palette — the post-OSC-override
    /// view an `OSC 4;Ps;?` query should answer. If the app changed a
    /// slot via `OSC 4;Ps;rgb:…`, the new value wins; otherwise each
    /// entry is the theme's `set_color_palette` push.
    ///
    /// Mirrors [`Self::live_colors`] (which answers the OSC 10/11/12
    /// special-color queries); the index-into-256 form here answers OSC
    /// 4. The caller falls back to the static theme palette on error.
    pub fn live_palette(&self) -> Result<[crate::ColorRgb; 256]> {
        self.read_palette(sys::GhosttyTerminalData_GHOSTTY_TERMINAL_DATA_COLOR_PALETTE)
    }

    /// libghostty's *default* 256-entry palette — what the palette would
    /// answer with every program `OSC 4` override removed.
    ///
    /// Paired with [`Self::live_palette`], this is what separates "the
    /// program recolored slot N" from "the embedder's theme set slot N":
    /// only the first belongs in a payload replayed into someone else's
    /// terminal.
    pub fn default_palette(&self) -> Result<[crate::ColorRgb; 256]> {
        self.read_palette(sys::GhosttyTerminalData_GHOSTTY_TERMINAL_DATA_COLOR_PALETTE_DEFAULT)
    }

    fn read_palette(&self, data: sys::GhosttyTerminalData) -> Result<[crate::ColorRgb; 256]> {
        use crate::ColorRgb;
        // libghostty's PaletteC is `[256]RGB.C`, layout-compatible with
        // `[GhosttyColorRgb; 256]` (the top-of-file size/align guards
        // pin `ColorRgb` <-> `GhosttyColorRgb`). `get` copies into this
        // caller-owned buffer.
        let mut raw = [sys::GhosttyColorRgb { r: 0, g: 0, b: 0 }; 256];
        // SAFETY: handle non-null; `raw` is a 256-entry buffer matching
        // the PaletteC layout libghostty writes.
        let rc =
            unsafe { sys::ghostty_terminal_get(self.handle, data, raw.as_mut_ptr() as *mut _) };
        Error::from_result(rc)?;
        Ok(raw.map(|c| ColorRgb::new(c.r, c.g, c.b)))
    }

    /// The effective value of one dynamic color — the program's `OSC
    /// 10`/`11`/`12` override if it set one, else the embedder's default.
    ///
    /// `None` means neither exists. [`Self::live_colors`] reads the same
    /// three at once for the renderer; this per-slot form exists so a
    /// caller can compare against [`Self::default_color`] slot by slot
    /// without an error path for the unset case.
    pub fn color(&self, which: TerminalColor) -> Result<Option<crate::ColorRgb>> {
        self.read_color(which.effective_data_id())
    }

    /// The *default* value of one dynamic color — what it would be with
    /// the program's `OSC 10`/`11`/`12` override removed. See
    /// [`Self::default_palette`] for why the distinction matters.
    pub fn default_color(&self, which: TerminalColor) -> Result<Option<crate::ColorRgb>> {
        self.read_color(which.default_data_id())
    }

    fn read_color(&self, data: sys::GhosttyTerminalData) -> Result<Option<crate::ColorRgb>> {
        let mut raw = sys::GhosttyColorRgb { r: 0, g: 0, b: 0 };
        // SAFETY: handle non-null; `raw` is a real local of the type every
        // `COLOR_*` data id documents as its out parameter.
        let rc = unsafe {
            sys::ghostty_terminal_get(
                self.handle,
                data,
                (&mut raw) as *mut sys::GhosttyColorRgb as *mut _,
            )
        };
        match Error::from_result(rc) {
            Ok(()) => Ok(Some(crate::ColorRgb::new(raw.r, raw.g, raw.b))),
            Err(Error::NoValue) => Ok(None),
            Err(err) => Err(err),
        }
    }

    /// True when the cursor sits at the right edge with its wrap
    /// deferred: the next printable byte wraps to the following row
    /// instead of overwriting the last column.
    ///
    /// Not part of the render state's cursor — the two UIs never need it
    /// — but it is terminal state a replayed payload has to reproduce, so
    /// the `vt` fidelity corpus asserts on it.
    pub fn cursor_pending_wrap(&self) -> bool {
        let mut pending: bool = false;
        // SAFETY: handle non-null; out is a real local.
        let rc = unsafe {
            sys::ghostty_terminal_get(
                self.handle,
                sys::GhosttyTerminalData_GHOSTTY_TERMINAL_DATA_CURSOR_PENDING_WRAP,
                (&mut pending) as *mut bool as *mut _,
            )
        };
        Error::from_result(rc).is_ok() && pending
    }

    /// Push the default foreground color into libghostty so SGR cells
    /// that inherit the default flip to the theme. Wraps
    /// `ghostty_terminal_set(OPT_COLOR_FOREGROUND, &rgb)`.
    pub fn set_color_foreground(&mut self, rgb: crate::ColorRgb) -> Result<()> {
        self.set_color_opt(
            sys::GhosttyTerminalOption_GHOSTTY_TERMINAL_OPT_COLOR_FOREGROUND,
            rgb,
        )
    }

    pub fn set_color_background(&mut self, rgb: crate::ColorRgb) -> Result<()> {
        self.set_color_opt(
            sys::GhosttyTerminalOption_GHOSTTY_TERMINAL_OPT_COLOR_BACKGROUND,
            rgb,
        )
    }

    pub fn set_color_cursor(&mut self, rgb: crate::ColorRgb) -> Result<()> {
        self.set_color_opt(
            sys::GhosttyTerminalOption_GHOSTTY_TERMINAL_OPT_COLOR_CURSOR,
            rgb,
        )
    }

    /// Set the full 256-entry palette in one FFI call. Mirrors the
    /// Mac UI's M6 P3 path.
    pub fn set_color_palette(&mut self, palette: &[crate::ColorRgb; 256]) -> Result<()> {
        // libghostty expects a contiguous array of `GhosttyColorRgb`;
        // our `ColorRgb` is layout-compatible so a transmute slice is
        // safe to pass.
        let ptr = palette.as_ptr() as *const sys::GhosttyColorRgb as *const _;
        // SAFETY: pointer is to a stack-owned array of 256 entries
        // matching the layout libghostty expects.
        let rc = unsafe {
            sys::ghostty_terminal_set(
                self.handle,
                sys::GhosttyTerminalOption_GHOSTTY_TERMINAL_OPT_COLOR_PALETTE,
                ptr,
            )
        };
        Error::from_result(rc)
    }

    /// Capture a transient [`GridRef`] for the position described by
    /// `point`. Returns `None` if libghostty rejects the point
    /// (out-of-range coordinates, no such row in the requested
    /// coordinate space).
    ///
    /// The returned `GridRef` is only valid until the next mutating
    /// terminal call. Prefer [`Self::convert_point`] for selection
    /// logic that needs a stable handle.
    pub fn grid_ref(&self, point: Point) -> Option<GridRef> {
        let c_point = point.to_sys();
        let mut out = sys::GhosttyGridRef {
            size: std::mem::size_of::<sys::GhosttyGridRef>(),
            node: std::ptr::null_mut(),
            x: 0,
            y: 0,
        };
        // SAFETY: handle non-null (constructor enforces); `c_point` and
        // `out` are stack-owned for the call.
        let rc = unsafe { sys::ghostty_terminal_grid_ref(self.handle, c_point, &mut out) };
        Error::from_result(rc).ok()?;
        if out.node.is_null() {
            return None;
        }
        Some(GridRef(out))
    }

    /// Capture an owned [`TrackedRef`] for `point` — a pin libghostty
    /// keeps up to date as the page list scrolls, prunes, and reflows.
    /// This is what selection endpoints are stored as; a raw
    /// [`GridRef`] or a [`PointTag::Screen`] coordinate both go stale.
    ///
    /// Returns [`Error::InvalidValue`] when `point` names no cell (an
    /// out-of-range row or column) and [`Error::OutOfMemory`] when the
    /// allocation fails.
    pub fn track(&self, point: Point) -> Result<TrackedRef> {
        TrackedRef::new(self, point)
    }

    /// Grid width in cells, as libghostty currently has it. `None` if
    /// the read fails — callers treat that as "don't clamp".
    pub(crate) fn cols(&self) -> Option<u16> {
        let mut out: u16 = 0;
        // SAFETY: handle non-null; `out` is a `uint16_t` local, the type
        // GHOSTTY_TERMINAL_DATA_COLS documents as its out parameter.
        let rc = unsafe {
            sys::ghostty_terminal_get(
                self.handle,
                sys::GhosttyTerminalData_GHOSTTY_TERMINAL_DATA_COLS,
                (&mut out) as *mut u16 as *mut _,
            )
        };
        Error::from_result(rc).ok()?;
        Some(out)
    }

    /// Resolve a [`GridRef`] back to a [`Point`] in the requested
    /// coordinate space. Returns `None` if the ref is invalid (e.g.
    /// the underlying row has been freed) or if the row has no
    /// representation in the requested space (e.g. asking for
    /// `Viewport` coordinates for a row currently outside the visible
    /// viewport).
    ///
    /// The `gref` must have been obtained from this same terminal via
    /// [`Self::grid_ref`] and must not have been invalidated by an
    /// intervening mutating terminal call (per libghostty's transience
    /// contract documented on [`GridRef`]).
    pub fn point_from_grid_ref(&self, gref: &GridRef, tag: PointTag) -> Option<Point> {
        let mut out = sys::GhosttyPointCoordinate::default();
        // SAFETY: handle non-null; gref came from this terminal's
        // grid_ref (per the docstring contract above); out is a
        // stack-owned local for libghostty to populate.
        let rc = unsafe {
            sys::ghostty_terminal_point_from_grid_ref(self.handle, &gref.0, tag.to_sys(), &mut out)
        };
        Error::from_result(rc).ok()?;
        Some(Point {
            tag,
            x: out.x,
            y: out.y,
        })
    }

    /// Return the OSC 8 hyperlink URI attached to the cell at viewport
    /// coordinates `(col, row)`, or `None` if the cell has no explicit
    /// hyperlink. Returns `None` for out-of-range coordinates too.
    ///
    /// Two-call buffer pattern per libghostty's C API: first call asks
    /// for the URI length with a null buffer (returns `OutOfSpace` and
    /// populates `out_len`); second call passes a buffer of the
    /// reported size.
    ///
    /// The grid_ref is captured + consumed immediately so libghostty's
    /// "valid only until next mutating call" contract isn't a concern
    /// for callers — we never hand the ref back out.
    pub fn hyperlink_at(&self, col: u16, row: u32) -> Option<String> {
        let gref = self.grid_ref(Point::viewport(col, row))?;

        let mut out_len: usize = 0;
        // First call: null buffer, returns OutOfSpace if there's a URI
        // or Success+out_len=0 if there isn't.
        // SAFETY: gref came from this terminal's grid_ref; out_len is
        // a real stack local.
        let rc = unsafe {
            sys::ghostty_grid_ref_hyperlink_uri(&gref.0, std::ptr::null_mut(), 0, &mut out_len)
        };
        match Error::from_result(rc) {
            Ok(()) => {
                // No hyperlink on this cell.
                if out_len == 0 {
                    return None;
                }
                // Success with non-zero len + a null buffer would be a
                // libghostty contract violation — return None
                // defensively rather than panic.
                return None;
            }
            Err(Error::OutOfSpace) => {
                // Expected — drop through to allocate + retry.
            }
            Err(_) => return None,
        }
        if out_len == 0 {
            return None;
        }

        let mut buf: Vec<u8> = vec![0; out_len];
        let mut written: usize = 0;
        // SAFETY: gref still valid (no mutating terminal call between
        // first + second probe); buf and written are stack-owned.
        let rc = unsafe {
            sys::ghostty_grid_ref_hyperlink_uri(&gref.0, buf.as_mut_ptr(), buf.len(), &mut written)
        };
        if Error::from_result(rc).is_err() {
            return None;
        }
        buf.truncate(written);
        // The URI is documented as a URI string. We trust libghostty
        // to write valid UTF-8 (URIs are ASCII per RFC 3986; pct-
        // encoded bytes are still ASCII). Fall back to None if a
        // future change introduces non-UTF-8 bytes.
        String::from_utf8(buf).ok()
    }

    /// Convert a `Point` from one coordinate space to another.
    /// Composition of [`Self::grid_ref`] and [`Self::point_from_grid_ref`]
    /// with the transient `GridRef` discarded immediately, which is the
    /// only safe way to translate coordinates without holding a
    /// `GridRef` across other terminal calls.
    ///
    /// Typical usage: store selection endpoints as `PointTag::Screen`
    /// (stable while the row remains in scrollback) and convert back
    /// to `PointTag::Viewport` each paint frame.
    pub fn convert_point(&self, point: Point, into: PointTag) -> Option<Point> {
        let gref = self.grid_ref(point)?;
        self.point_from_grid_ref(&gref, into)
    }

    fn set_color_opt(
        &mut self,
        option: sys::GhosttyTerminalOption,
        rgb: crate::ColorRgb,
    ) -> Result<()> {
        let c = sys::GhosttyColorRgb {
            r: rgb.r,
            g: rgb.g,
            b: rgb.b,
        };
        // SAFETY: c lives for the duration of the call; libghostty
        // copies the value internally.
        let rc =
            unsafe { sys::ghostty_terminal_set(self.handle, option, (&c) as *const _ as *const _) };
        Error::from_result(rc)
    }

    /// Set each `(slot, trampoline)` pair. If one fails, every slot in
    /// `callbacks` is cleared, so none is left set, and its error returned.
    fn set_callbacks(
        &mut self,
        callbacks: &[(sys::GhosttyTerminalOption, *const c_void)],
    ) -> Result<()> {
        let set = callbacks
            .iter()
            .try_for_each(|&(option, callback)| self.set_opt(option, callback));
        if set.is_err() {
            let _ = self.clear_callbacks(callbacks.iter().map(|&(option, _)| option));
        }
        set
    }

    /// Clear every slot in `options`, trying each even after a failure;
    /// the first error is returned.
    fn clear_callbacks(
        &mut self,
        options: impl IntoIterator<Item = sys::GhosttyTerminalOption>,
    ) -> Result<()> {
        let mut result = Ok(());
        for option in options {
            let cleared = self.set_opt(option, ptr::null());
            if result.is_ok() {
                result = cleared;
            }
        }
        result
    }

    /// Set a pointer-valued option (the userdata + callback slots).
    /// `value` may be null to clear a slot.
    fn set_opt(&mut self, option: sys::GhosttyTerminalOption, value: *const c_void) -> Result<()> {
        // SAFETY: handle non-null per the constructor; `value` is either
        // null (always valid to clear a slot) or a pointer valid for as
        // long as libghostty needs it — the `CallbackCtx`, which outlives
        // the handle, or a module-level `'static` trampoline.
        let rc = unsafe { sys::ghostty_terminal_set(self.handle, option, value) };
        Error::from_result(rc)
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        // Errors are ignored — there is nothing to recover to in Drop,
        // and the context outlives the handle regardless.
        let _ = self.clear_callbacks(std::iter::once(WRITE_PTY).chain(STATUS_CALLBACKS));
        // SAFETY: handle non-null per constructor; freeing is the
        // documented destructor.
        unsafe { sys::ghostty_terminal_free(self.handle) };
        // SAFETY: `ctx` came from `Box::leak` in `adopt`, and the only
        // other holder of the pointer, the handle, was freed above.
        // Dropping it releases both sinks.
        drop(unsafe { Box::from_raw(self.ctx.as_ptr()) });
    }
}
