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
use std::ptr;
use std::sync::{Arc, Mutex};

use crate::sys;
use crate::{Error, Result};

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
/// `ghostty_terminal_new`; `max_scrollback` is a separate
/// `ghostty_terminal_set` the constructor applies right after (upstream
/// retired the options struct that used to carry all three).
#[derive(Debug, Clone, Copy)]
pub struct TerminalOptions {
    pub cols: u16,
    pub rows: u16,
    /// Number of rows of off-screen scrollback to retain. Both UIs
    /// use 2000.
    pub max_scrollback: usize,
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
    /// screen row is stable only while nothing has been evicted from the
    /// top: once `max_scrollback` saturates, every evicted row shifts all
    /// stored screen coordinates down by one relative to the content they
    /// named. Still the best space available for long-lived selection
    /// endpoints — libghostty's tracked pins would fix the drift, but the
    /// C API does not export them.
    Screen,
    /// Scrollback history only — the area above the active region.
    /// 0 = top of scrollback.
    History,
}

#[cfg(feature = "ffi")]
impl PointTag {
    fn to_sys(self) -> sys::GhosttyPointTag {
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
/// store `GridRef` directly. Convert to a [`Point`] with
/// [`PointTag::Screen`] via [`Terminal::convert_point`] and store
/// that — see [`PointTag::Screen`] for how far that stability actually
/// goes (it ends when scrollback saturates and rows start being
/// evicted).
#[cfg(feature = "ffi")]
#[derive(Debug, Clone, Copy)]
pub struct GridRef(sys::GhosttyGridRef);

#[cfg(feature = "ffi")]
impl GridRef {
    /// Raw pin, for in-crate wrappers that hand it straight back to
    /// libghostty (see `formatter::selection_text`). Same transience
    /// contract as the type itself.
    pub(crate) fn as_sys(&self) -> sys::GhosttyGridRef {
        self.0
    }
}

pub struct Terminal {
    handle: sys::GhosttyTerminal,
    /// Backing store for engine-emitted device-query replies. Installed
    /// via [`Terminal::set_write_pty_buffer`]; the `Arc` is retained
    /// here so the `Arc::as_ptr` handed to libghostty as `OPT_USERDATA`
    /// stays valid for as long as the callback is live. `None` until a
    /// buffer is installed.
    write_pty_buffer: Option<Arc<Mutex<Vec<u8>>>>,
    /// `!Sync` marker — libghostty-vt is single-threaded. Using
    /// `*const ()` makes the type `Send + !Sync`, which matches the
    /// Mac UI's `@MainActor`-only contract.
    _not_sync: PhantomData<std::cell::Cell<()>>,
}

/// Trampoline installed as `GHOSTTY_TERMINAL_OPT_WRITE_PTY`. libghostty
/// invokes this synchronously from inside `ghostty_terminal_vt_write`
/// **and** `ghostty_terminal_resize` (mode 2048 in-band size reports),
/// handing back the `OPT_USERDATA` pointer we installed alongside it —
/// an `Arc::as_ptr(&arc)` i.e. a `*const Mutex<Vec<u8>>` into an Arc
/// allocation pinned by [`Terminal::write_pty_buffer`].
///
/// Hygiene contract (all load-bearing for FFI soundness):
/// * Guards null `userdata`/`data` and `len == 0` — early return.
/// * Poison-tolerant lock (`unwrap_or_else(|p| p.into_inner())`) so a
///   panic in another holder can never surface as an unwind across this
///   `extern "C"` frame.
/// * Append-only. It must **never** call back into `vt_write`
///   (`terminal.h:949-951` no-reentrancy contract).
unsafe extern "C" fn write_pty_trampoline(
    _terminal: sys::GhosttyTerminal,
    userdata: *mut c_void,
    data: *const u8,
    len: usize,
) {
    if userdata.is_null() || data.is_null() || len == 0 {
        return;
    }
    let mutex = userdata as *const Mutex<Vec<u8>>;
    // SAFETY: `userdata` is the `Arc::as_ptr` installed together with
    // this callback; the owning `Arc` is pinned in the `Terminal`'s
    // `write_pty_buffer` field for the whole time the callback is live
    // (cleared before the Arc is dropped — see `clear_write_pty`).
    let mutex = unsafe { &*mutex };
    // Poison-tolerant: recover the guard rather than propagate a panic
    // across the C boundary.
    let mut guard = mutex.lock().unwrap_or_else(|p| p.into_inner());
    // SAFETY: libghostty guarantees `data`/`len` describe a valid,
    // initialized byte range for the duration of this call.
    let bytes = unsafe { std::slice::from_raw_parts(data, len) };
    guard.extend_from_slice(bytes);
}

// SAFETY: the underlying libghostty-vt handle can move between threads
// as long as only one thread touches it at a time. Sync is intentionally
// not implemented (enforced via PhantomData above).
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
        let terminal = Self {
            handle,
            write_pty_buffer: None,
            _not_sync: PhantomData,
        };
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
        Ok(terminal)
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
    /// appends every emitted byte to `buf`; the caller drains it (see
    /// the drain contract below). This is what makes DA1/DA2, DSR,
    /// DECRQM, XTVERSION, the Kitty keyboard query, and mode-2048
    /// in-band size reports actually reach the PTY.
    ///
    /// # Drain contract
    ///
    /// Callers MUST drain `buf` after **every** [`Self::vt_write`] AND
    /// after **every** [`Self::resize`]: mode 2048 (in-band size
    /// reports) fires the callback synchronously inside
    /// `ghostty_terminal_resize`, i.e. *outside* `vt_write`. Draining
    /// only after `vt_write` would silently drop resize-triggered
    /// reports.
    ///
    /// Callers MUST NOT hold `buf`'s lock across a call to
    /// [`Self::vt_write`] or [`Self::resize`]: the trampoline locks the
    /// same mutex synchronously inside those calls, so a held guard
    /// self-deadlocks (or aborts, on a recursive-lock-panicking
    /// implementation). Lock only to take/drain the bytes.
    ///
    /// # `OPT_USERDATA` exclusivity
    ///
    /// This API takes **exclusive** ownership of
    /// `GHOSTTY_TERMINAL_OPT_USERDATA`, which libghostty documents as a
    /// *single shared slot* for the userdata of ALL of its callbacks
    /// (`terminal.h:63`). Wiring any second callback later (bell, title,
    /// size, enquiry, color-scheme, …) cannot simply set its own
    /// userdata — it must be multiplexed through one shared context
    /// struct. That refactor is deliberately deferred until a second
    /// callback actually exists.
    ///
    /// # Semantics
    ///
    /// * **Install** stores the `Arc` in the field (pinning the
    ///   allocation `Arc::as_ptr` points at), sets `OPT_USERDATA` to
    ///   that pointer, then installs the trampoline as `OPT_WRITE_PTY`.
    /// * **Transactional:** if the trampoline install fails, the
    ///   userdata is reset to null and the field cleared before the
    ///   error is returned — never left half-installed.
    /// * **Replacement:** calling again first tears down the current
    ///   callback (clearing `OPT_WRITE_PTY`, then `OPT_USERDATA`, then
    ///   dropping the old `Arc`) and installs the new buffer — the old
    ///   `Arc`'s strong count drops naturally.
    pub fn set_write_pty_buffer(&mut self, buf: Arc<Mutex<Vec<u8>>>) -> Result<()> {
        // Replacement: tear down the live callback first so it can never
        // be observed pointing at the old Arc while the new one is being
        // wired in. Drops the previous field Arc.
        if self.write_pty_buffer.is_some() {
            self.clear_write_pty()?;
        }

        // Pointer to the `Mutex<Vec<u8>>` inside the Arc allocation —
        // stable for the Arc's lifetime, which the field now pins.
        let userdata = Arc::as_ptr(&buf) as *const c_void;
        self.write_pty_buffer = Some(buf);

        // 1. userdata slot — targets an allocation pinned by the field
        // we just set.
        if let Err(err) = self.set_opt(
            sys::GhosttyTerminalOption_GHOSTTY_TERMINAL_OPT_USERDATA,
            userdata,
        ) {
            self.write_pty_buffer = None;
            return Err(err);
        }

        // 2. callback slot. The header types callback-valued options as
        // `const void *`; pass the trampoline's fn pointer cast to it.
        let callback = write_pty_trampoline as *const c_void;
        if let Err(err) = self.set_opt(
            sys::GhosttyTerminalOption_GHOSTTY_TERMINAL_OPT_WRITE_PTY,
            callback,
        ) {
            // Transactional rollback: unset userdata, drop the field, so
            // no stale userdata is left with no callback (or vice versa).
            let _ = self.set_opt(
                sys::GhosttyTerminalOption_GHOSTTY_TERMINAL_OPT_USERDATA,
                ptr::null(),
            );
            self.write_pty_buffer = None;
            return Err(err);
        }

        Ok(())
    }

    /// Uninstall the `write_pty` callback and drop the reply buffer.
    ///
    /// Clears `OPT_WRITE_PTY` (to null) **before** `OPT_USERDATA` and
    /// before dropping the `Arc`, so the callback can never be live with
    /// a stale userdata pointer.
    pub fn clear_write_pty(&mut self) -> Result<()> {
        // 1. Kill the callback first.
        self.set_opt(
            sys::GhosttyTerminalOption_GHOSTTY_TERMINAL_OPT_WRITE_PTY,
            ptr::null(),
        )?;
        // 2. Then clear the userdata pointer it referenced.
        self.set_opt(
            sys::GhosttyTerminalOption_GHOSTTY_TERMINAL_OPT_USERDATA,
            ptr::null(),
        )?;
        // 3. Finally drop the Arc (safe now that nothing points at it).
        self.write_pty_buffer = None;
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

    /// Read a DEC mode bit (e.g. mode 2004 for bracketed paste).
    /// Returns `false` if the mode is not currently set or if the mode
    /// number is unknown to libghostty.
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
        // Anything other than the alternate screen collapses to primary;
        // safer fallback for the scroll handler.
        if out == sys::GhosttyTerminalScreen_GHOSTTY_TERMINAL_SCREEN_ALTERNATE {
            ActiveScreen::Alternate
        } else {
            ActiveScreen::Primary
        }
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
        use crate::ColorRgb;
        // libghostty's PaletteC is `[256]RGB.C`, layout-compatible with
        // `[GhosttyColorRgb; 256]` (the top-of-file size/align guards
        // pin `ColorRgb` <-> `GhosttyColorRgb`). `get` copies into this
        // caller-owned buffer.
        let mut raw = [sys::GhosttyColorRgb { r: 0, g: 0, b: 0 }; 256];
        // SAFETY: handle non-null; `raw` is a 256-entry buffer matching
        // the PaletteC layout libghostty writes.
        let rc = unsafe {
            sys::ghostty_terminal_get(
                self.handle,
                sys::GhosttyTerminalData_GHOSTTY_TERMINAL_DATA_COLOR_PALETTE,
                raw.as_mut_ptr() as *mut _,
            )
        };
        Error::from_result(rc)?;
        Ok(raw.map(|c| ColorRgb::new(c.r, c.g, c.b)))
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
        let c_point = sys::GhosttyPoint {
            tag: point.tag.to_sys(),
            value: sys::GhosttyPointValue {
                coordinate: sys::GhosttyPointCoordinate {
                    x: point.x,
                    y: point.y,
                },
            },
        };
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

    /// Set a pointer-valued option (the userdata + callback slots).
    /// `value` may be null to clear a slot.
    fn set_opt(&mut self, option: sys::GhosttyTerminalOption, value: *const c_void) -> Result<()> {
        // SAFETY: handle non-null per the constructor; `value` is either
        // null (always valid to clear a slot) or a pointer valid for as
        // long as libghostty needs it — the userdata pinned by
        // `write_pty_buffer`, or the module-level `'static` trampoline.
        let rc = unsafe { sys::ghostty_terminal_set(self.handle, option, value) };
        Error::from_result(rc)
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        // Clear the write_pty callback + userdata before free so
        // libghostty can never invoke the trampoline against a freed
        // handle or a dropped Arc. Reuses the clear ordering
        // (callback → userdata → drop Arc); errors are ignored — there
        // is nothing to recover to in Drop.
        if self.write_pty_buffer.is_some() {
            let _ = self.clear_write_pty();
        }
        // SAFETY: handle non-null per constructor; freeing is the
        // documented destructor.
        unsafe { sys::ghostty_terminal_free(self.handle) };
    }
}
