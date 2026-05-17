//! Safe Rust wrapper over libghostty-vt.
//!
//! libghostty-vt is the VT parser + screen state engine extracted from
//! Ghostty. Roost uses it (a) on the daemon side for any server-side
//! OSC semantics that need parsing before the UI sees the bytes, and
//! (b) on both UIs for in-process VT parse + render. The Mac UI links
//! libghostty-vt via Swift's bridging header; the Linux UI
//! (`crates/roost-linux/`) consumes it through this crate.
//!
//! The FFI layer is gated behind the `ffi` cargo feature so the
//! workspace builds cleanly before `third_party/ghostty/build.sh` has
//! produced the static archive. CI builds with `--features ffi` after
//! running the Ghostty build script. Local dev can build without it
//! while iterating on the rest of the daemon.
//!
//! The public API is a hybrid wrapper:
//!   * `Terminal`, `RenderState`, `KeyEncoder` own their FFI handles
//!     RAII-style; their constructors hide the `*mut Impl` pointers and
//!     their `Drop` calls the matching `ghostty_*_free`.
//!   * Data accessors (cell iteration, render-state data reads, palette
//!     writes) pass through thinly. We re-export the bindgen-generated
//!     `Key`, `Mods`, `KeyAction`, and color types so call sites speak
//!     the same vocabulary as the C API without writing `unsafe`.
//!   * `pub mod ffi` exposes the raw bindgen output for anything not
//!     yet wrapped — matches how `mac/Sources/Roost/KeyEncoder.swift`
//!     still calls a few C symbols directly. Keep this list small; the
//!     idiomatic path is to add a wrapper here when a new consumer
//!     needs one.

#![allow(clippy::needless_pass_by_value)]

#[cfg(feature = "ffi")]
mod sys {
    #![allow(non_upper_case_globals)]
    #![allow(non_camel_case_types)]
    #![allow(non_snake_case)]
    #![allow(dead_code)]

    include!(concat!(env!("OUT_DIR"), "/ghostty_vt.rs"));
}

/// Raw bindgen output. Use sparingly — the typed wrappers in
/// [`terminal`], [`render_state`], and [`key_encoder`] are the
/// recommended path. Exposed for consumers that need a symbol not yet
/// wrapped (e.g. OSC parser hooks, kitty graphics) without forcing them
/// to wait on a wrapper-PR cycle.
#[cfg(feature = "ffi")]
pub mod ffi {
    pub use super::sys::*;
}

use thiserror::Error;

/// libghostty-vt's success return code, mirrored locally so wrapper
/// callsites don't need to import bindgen names. Defined alongside
/// `Error::from_result` so the conversion path is in one place.
#[cfg(feature = "ffi")]
const GHOSTTY_SUCCESS: i32 = 0;

#[derive(Debug, Error)]
pub enum Error {
    #[error("libghostty-vt FFI not compiled in; rebuild with `--features ffi`")]
    FfiDisabled,
    #[error("libghostty-vt returned a null handle")]
    NullHandle,
    #[error("libghostty-vt: out of memory")]
    OutOfMemory,
    #[error("libghostty-vt: invalid value")]
    InvalidValue,
    #[error("libghostty-vt: out of space (buffer too small)")]
    OutOfSpace,
    #[error("libghostty-vt: no value")]
    NoValue,
    #[error("libghostty-vt returned error code {0}")]
    Other(i32),
}

#[cfg(feature = "ffi")]
impl Error {
    /// Map a `GhosttyResult` integer to either `Ok(())` (success) or
    /// the matching variant. Centralized so each wrapper callsite stays
    /// `?`-friendly without bindgen constants leaking out.
    pub(crate) fn from_result(rc: i32) -> Result<()> {
        match rc {
            GHOSTTY_SUCCESS => Ok(()),
            -1 => Err(Error::OutOfMemory),
            -2 => Err(Error::InvalidValue),
            -3 => Err(Error::OutOfSpace),
            -4 => Err(Error::NoValue),
            other => Err(Error::Other(other)),
        }
    }
}

pub type Result<T> = std::result::Result<T, Error>;

/// Returns true if the crate was compiled with the `ffi` feature and
/// libghostty-vt is linked. Useful for tests that should be skipped when
/// the vendored archive isn't present.
pub const fn ffi_available() -> bool {
    cfg!(feature = "ffi")
}

#[cfg(feature = "ffi")]
mod key_encoder;
#[cfg(feature = "ffi")]
mod render_state;
#[cfg(feature = "ffi")]
mod terminal;

#[cfg(feature = "ffi")]
pub use key_encoder::{Key, KeyAction, KeyEncoder, KeyEvent, Mods};
#[cfg(feature = "ffi")]
pub use render_state::{Cell, ColorRgb, Colors, CursorInfo, CursorVisualStyle, RenderState};
#[cfg(feature = "ffi")]
pub use terminal::{ActiveScreen, ScrollViewport, Terminal, TerminalOptions};

// ============================================================================
// FFI smoke
// ============================================================================
//
// End-to-end exercise of the safe API:
//   1. Construct a Terminal at a known size.
//   2. Write a few VT bytes via the safe writer.
//   3. Allocate a RenderState and update it from the terminal.
//   4. Drop everything (RAII frees the handles).
//
// CI runs this with `--features ffi` after building libghostty-vt;
// the non-ffi default skips the call.

/// Run the FFI smoke. Returns Ok on success, an `Error::Other(code)` if
/// libghostty-vt rejects any call, or `Error::FfiDisabled` if the crate
/// was built without the `ffi` feature.
#[cfg(feature = "ffi")]
pub fn vt_smoke() -> Result<()> {
    let mut term = Terminal::new(TerminalOptions {
        cols: 80,
        rows: 24,
        max_scrollback: 0,
    })?;
    term.vt_write(b"hello\r\n");
    let mut rs = RenderState::new()?;
    rs.update(&term)?;
    Ok(())
}

#[cfg(not(feature = "ffi"))]
pub fn vt_smoke() -> Result<()> {
    Err(Error::FfiDisabled)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ffi_flag_matches_cfg() {
        // Catches a class of regression where someone refactors
        // `ffi_available` to a literal and breaks the runtime signal
        // the daemon relies on to decide whether to expose VT-side
        // helpers. The test exists at all because both feature-flag
        // states need their own check.
        assert_eq!(ffi_available(), cfg!(feature = "ffi"));
    }

    /// CI runs this with `--features ffi` after building libghostty-vt
    /// via `third_party/ghostty/build.sh`. Without the feature the test
    /// is a no-op so the default `cargo test` still passes on dev
    /// machines that haven't built the static archive.
    #[test]
    fn vt_smoke_round_trip() {
        if !ffi_available() {
            eprintln!("[roost-vt] skipping vt_smoke (build with --features ffi to enable)");
            return;
        }
        vt_smoke().expect("vt_smoke should round-trip when ffi is enabled");
    }

    /// `vt_smoke` should be idempotent — running it twice in one process
    /// proves the RAII frees actually release resources.
    #[test]
    fn vt_smoke_runs_twice() {
        if !ffi_available() {
            return;
        }
        vt_smoke().expect("first vt_smoke");
        vt_smoke().expect("second vt_smoke");
    }
}
