//! Owned, self-updating pins into a terminal's page list.
//!
//! A [`crate::GridRef`] is a raw pin that libghostty invalidates on the
//! next mutating call. A [`TrackedRef`] is the owned variant: libghostty
//! rewrites it as the page list scrolls, prunes, and reflows, so it
//! keeps naming the *same cell content* on both axes instead of a fixed
//! coordinate. That is what makes a selection follow its rows through
//! scrollback eviction and a resize.
//!
//! Three invariants this wrapper adds on top of the C API:
//!
//! * **RAII.** [`Drop`] calls `ghostty_tracked_grid_ref_free` exactly
//!   once. Freeing after the owning terminal is gone is explicitly
//!   allowed by libghostty, so no drop ordering is required of callers.
//! * **Terminal identity.** libghostty does not check that the terminal
//!   handed to `_set` is the one that created the ref — pairing them
//!   wrongly is undefined behavior. Every operation here that takes a
//!   `&Terminal` compares it against the handle stored at creation and
//!   returns [`crate::Error::InvalidValue`] on a mismatch.
//! * **Snapshot confinement.** `ghostty_tracked_grid_ref_snapshot`
//!   hands back an *untracked* `GhosttyGridRef` with the usual
//!   "valid until the next terminal update" contract. [`GridRef`] has no
//!   lifetime parameter to enforce that, so [`TrackedRef::snapshot`] is
//!   `pub(crate)` and is called from exactly one place: inside
//!   `formatter::selection_text`, which pins, formats, and frees within
//!   a single synchronous call holding `&Terminal` (see that module's
//!   docs for why the interleaving is a UB hazard rather than a
//!   correctness one).
//!
//! A tracked ref is attached to the screen (primary or alternate) that
//! was active when it was created, and keeps resolving against *that*
//! screen even while the other one is displayed. Callers therefore have
//! to remember the screen themselves; [`TrackedRef::screen`] reports it
//! and [`TrackedRef::snapshot`] debug-asserts that it still matches
//! before handing a pin to the formatter, which requires endpoints on
//! the active screen.

use std::marker::PhantomData;
use std::ptr;

use crate::sys;
use crate::{ActiveScreen, Error, GridRef, Point, PointTag, Result, Terminal};

/// An owned pin that follows its cell through page-list changes.
///
/// Obtained from [`Terminal::track`]. Reports "no value" — `false` from
/// [`Self::has_value`], `None` from [`Self::point`] — once the tracked
/// content is discarded: pruned out of scrollback, dropped by a
/// terminal reset, or orphaned because the owning terminal was freed.
/// Content that is merely off screen still resolves; only the
/// `Viewport` space declines it.
pub struct TrackedRef {
    handle: sys::GhosttyTrackedGridRef,
    /// Raw handle of the terminal that created this ref, kept for the
    /// identity check described in the module docs.
    terminal: sys::GhosttyTerminal,
    screen: ActiveScreen,
    /// `!Sync` marker, matching [`Terminal`] — libghostty-vt is
    /// single-threaded and a tracked ref is part of that state.
    _not_sync: PhantomData<std::cell::Cell<()>>,
}

// SAFETY: same contract as `Terminal` — the handle may move between
// threads as long as only one thread touches the terminal (and anything
// pinned into it) at a time. `Sync` is deliberately not implemented.
unsafe impl Send for TrackedRef {}

impl std::fmt::Debug for TrackedRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TrackedRef")
            .field("screen", &self.screen)
            .field("has_value", &self.has_value())
            .finish()
    }
}

impl TrackedRef {
    pub(crate) fn new(terminal: &Terminal, point: Point) -> Result<Self> {
        let mut handle: sys::GhosttyTrackedGridRef = ptr::null_mut();
        // SAFETY: live terminal handle, a stack-owned point, and an
        // out-pointer we own.
        let rc = unsafe {
            sys::ghostty_terminal_grid_ref_track(terminal.handle(), point.to_sys(), &mut handle)
        };
        Error::from_result(rc)?;
        if handle.is_null() {
            return Err(Error::NullHandle);
        }
        Ok(Self {
            handle,
            terminal: terminal.handle(),
            screen: terminal.active_screen(),
            _not_sync: PhantomData,
        })
    }

    /// True when `terminal` is the terminal this ref was created from.
    /// Operations that pair the two must check this first — libghostty
    /// does not. The check is raw-pointer equality: it assumes a tab's
    /// terminal is allocated once and never replaced (true in both UIs
    /// today). A feature that swaps a live tab's terminal in place must
    /// also drop that tab's selections, or an allocator reusing the
    /// address would defeat this guard.
    pub fn is_owned_by(&self, terminal: &Terminal) -> bool {
        self.terminal == terminal.handle()
    }

    /// The screen this ref is attached to, captured at creation (or at
    /// the last successful [`Self::set`]).
    pub fn screen(&self) -> ActiveScreen {
        self.screen
    }

    /// Whether the tracked cell still exists. `false` once the content
    /// was pruned, reset away, or the owning terminal was freed.
    pub fn has_value(&self) -> bool {
        // SAFETY: handle came from a successful track and is freed only
        // in `Drop`; the C API tolerates a freed owning terminal.
        unsafe { sys::ghostty_tracked_grid_ref_has_value(self.handle) }
    }

    /// Resolve the tracked cell in `tag`'s coordinate space.
    ///
    /// `None` covers both "the content is gone" and "it has no
    /// representation in this space" — asking for `Viewport` while the
    /// row is scrolled out of view is the common case of the latter.
    pub fn point(&self, tag: PointTag) -> Option<Point> {
        let mut out = sys::GhosttyPointCoordinate::default();
        // SAFETY: live handle; `out` is a stack-owned local.
        let rc =
            unsafe { sys::ghostty_tracked_grid_ref_point(self.handle, tag.to_sys(), &mut out) };
        Error::from_result(rc).ok()?;
        Some(Point {
            tag,
            x: out.x,
            y: out.y,
        })
    }

    /// Re-point the ref at `point`, resolved against `terminal`'s
    /// currently active screen. Clears any prior "no value" state.
    ///
    /// Returns [`Error::InvalidValue`] when `terminal` is not the
    /// owning terminal or the point names no cell, and
    /// [`Error::OutOfMemory`] when the allocation fails — in which case
    /// libghostty leaves the ref untouched.
    pub fn set(&mut self, terminal: &Terminal, point: Point) -> Result<()> {
        if !self.is_owned_by(terminal) {
            return Err(Error::InvalidValue);
        }
        // SAFETY: identity checked above, so the handle pair is the one
        // libghostty expects; the point is a stack-owned value.
        let rc = unsafe {
            sys::ghostty_tracked_grid_ref_set(self.handle, terminal.handle(), point.to_sys())
        };
        Error::from_result(rc)?;
        // `_set` resolves against whichever screen is active now, which
        // may move the ref between page lists.
        self.screen = terminal.active_screen();
        Ok(())
    }

    /// Untracked pin for the cell, valid only until the next mutating
    /// terminal call.
    ///
    /// `pub(crate)` on purpose: the returned [`GridRef`] carries no
    /// lifetime, so the only way to keep the "no pin outlives a
    /// mutation" rule checkable by construction is to confine snapshots
    /// to the crate's one synchronous pin/format/free call. See the
    /// module docs and `formatter`.
    ///
    /// `Ok(None)` when the tracked content is gone — an empty
    /// selection, not a failure.
    pub(crate) fn snapshot(&self, terminal: &Terminal) -> Result<Option<GridRef>> {
        if !self.is_owned_by(terminal) {
            return Err(Error::InvalidValue);
        }
        debug_assert_eq!(
            terminal.active_screen(),
            self.screen,
            "snapshotting a tracked ref from an inactive screen: the \
             formatter requires endpoints on the active screen"
        );
        let mut out = sys::GhosttyGridRef {
            size: std::mem::size_of::<sys::GhosttyGridRef>(),
            node: ptr::null_mut(),
            x: 0,
            y: 0,
        };
        // SAFETY: live handle; `out` is a stack-owned, correctly sized
        // grid-ref local.
        let rc = unsafe { sys::ghostty_tracked_grid_ref_snapshot(self.handle, &mut out) };
        match Error::from_result(rc) {
            Ok(()) if out.node.is_null() => Ok(None),
            Ok(()) => Ok(Some(GridRef::from_sys(out))),
            Err(Error::NoValue) => Ok(None),
            Err(err) => Err(err),
        }
    }
}

impl Drop for TrackedRef {
    fn drop(&mut self) {
        // SAFETY: handle came from a successful track and is freed
        // exactly once. libghostty documents freeing after the owning
        // terminal as allowed.
        unsafe { sys::ghostty_tracked_grid_ref_free(self.handle) };
    }
}
