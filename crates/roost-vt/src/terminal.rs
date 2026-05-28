//! Safe wrapper around `ghostty_terminal_*`.
//!
//! `Terminal` owns a `*mut GhosttyTerminalImpl` handle. Construction
//! allocates via `ghostty_terminal_new`; `Drop` releases via
//! `ghostty_terminal_free`. The handle is `Send` (the FFI is thread-safe
//! at the "owned by one thread at a time" level) but explicitly `!Sync`
//! — libghostty-vt must not be touched from more than one thread
//! concurrently, mirroring the Go binary's main-thread invariant
//! (`CLAUDE.md` "Threading") and the Mac UI's `@MainActor` discipline.

use std::marker::PhantomData;
use std::ptr;

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

/// Construction parameters for a new terminal. Matches
/// `GhosttyTerminalOptions` 1:1.
#[derive(Debug, Clone, Copy)]
pub struct TerminalOptions {
    pub cols: u16,
    pub rows: u16,
    /// Number of rows of off-screen scrollback to retain. The Mac UI
    /// uses 2000 to match the Go binary's `cmd/roost/session.go`.
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
    /// Full screen including scrollback. 0 = top of scrollback. Rows
    /// here are stable as long as the row has not aged out of the
    /// scrollback buffer, which makes this the recommended coordinate
    /// space for storing long-lived selection endpoints.
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
/// that — screen coordinates remain stable until the row ages out of
/// scrollback.
#[cfg(feature = "ffi")]
#[derive(Debug, Clone, Copy)]
pub struct GridRef(sys::GhosttyGridRef);

pub struct Terminal {
    handle: sys::GhosttyTerminal,
    /// `!Sync` marker — libghostty-vt is single-threaded. Using
    /// `*const ()` makes the type `Send + !Sync`, which matches the
    /// Mac UI's `@MainActor`-only contract.
    _not_sync: PhantomData<std::cell::Cell<()>>,
}

// SAFETY: the underlying libghostty-vt handle can move between threads
// as long as only one thread touches it at a time. Sync is intentionally
// not implemented (enforced via PhantomData above).
unsafe impl Send for Terminal {}

impl Terminal {
    /// Allocate a fresh terminal. Mirrors
    /// `ghostty_terminal_new(NULL, &out, options)` and panics-free: any
    /// non-success result is returned as an `Error`.
    pub fn new(options: TerminalOptions) -> Result<Self> {
        let opts = sys::GhosttyTerminalOptions {
            cols: options.cols,
            rows: options.rows,
            max_scrollback: options.max_scrollback,
        };
        let mut handle: sys::GhosttyTerminal = ptr::null_mut();
        // SAFETY: passing a null allocator (libghostty's default), an
        // out-pointer we own, and a stack-allocated options struct.
        let rc = unsafe { sys::ghostty_terminal_new(ptr::null_mut(), &mut handle, opts) };
        Error::from_result(rc)?;
        if handle.is_null() {
            return Err(Error::NullHandle);
        }
        Ok(Self {
            handle,
            _not_sync: PhantomData,
        })
    }

    /// Raw FFI handle. Pass-through for crates that need to call a
    /// not-yet-wrapped symbol (e.g. `crates/roost-linux/`'s key encoder
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

    /// Read a DEC mode bit (e.g. mode 2004 for bracketed paste).
    /// Returns `false` if the mode is not currently set or if the mode
    /// number is unknown to libghostty.
    pub fn mode_get(&self, mode: u16) -> bool {
        let mut out: bool = false;
        // SAFETY: handle non-null; out is a real local.
        let rc = unsafe { sys::ghostty_terminal_mode_get(self.handle, mode as _, &mut out) };
        // Treat any non-success as "false" — the Mac UI does the same.
        Error::from_result(rc).ok();
        out
    }

    /// True if the active screen is the alternate buffer (vim, less,
    /// htop, etc.). The Linux/Mac UIs use this to decide between local
    /// scrollback and arrow-key translation for the wheel.
    pub fn active_screen(&self) -> ActiveScreen {
        let mut out: u32 = 0;
        // SAFETY: handle non-null; out is a real local.
        let rc = unsafe {
            sys::ghostty_terminal_get(
                self.handle,
                sys::GhosttyTerminalData_GHOSTTY_TERMINAL_DATA_ACTIVE_SCREEN,
                (&mut out) as *mut u32 as *mut _,
            )
        };
        if Error::from_result(rc).is_err() {
            return ActiveScreen::Primary;
        }
        // libghostty exposes 0=primary, 1=alt. Anything else collapses
        // to primary; safer fallback for the scroll handler.
        if out == 1 {
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
}

impl Drop for Terminal {
    fn drop(&mut self) {
        // SAFETY: handle non-null per constructor; freeing is the
        // documented destructor.
        unsafe { sys::ghostty_terminal_free(self.handle) };
    }
}
