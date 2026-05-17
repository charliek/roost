//! Safe Rust wrapper over libghostty-vt.
//!
//! libghostty-vt is the VT parser + screen state engine extracted from
//! Ghostty. We use it (a) on the daemon side for any server-side OSC
//! semantics that need parsing before the UI sees the bytes, and (b) on
//! both UIs for in-process VT parse + render.
//!
//! The FFI layer is gated behind the `ffi` cargo feature so the
//! workspace builds cleanly before `third_party/ghostty/build.sh` has
//! produced the static archive. CI builds with `--features ffi` after
//! running the Ghostty build script. Local dev can build without it
//! while iterating on the rest of the daemon.

#![allow(clippy::needless_pass_by_value)]

#[cfg(feature = "ffi")]
mod sys {
    #![allow(non_upper_case_globals)]
    #![allow(non_camel_case_types)]
    #![allow(non_snake_case)]
    #![allow(dead_code)]

    include!(concat!(env!("OUT_DIR"), "/ghostty_vt.rs"));
}

/// libghostty-vt's success return code. Defined locally rather than
/// referenced from `sys::` because bindgen's handling of the C-side
/// constant varies (it might be a `#define`, an enum variant, or a
/// typedef-enum value depending on the header), and any of those map
/// to a different Rust path. Success-is-zero is universal C convention,
/// so pinning the constant here is robust against bindgen output drift.
#[cfg(feature = "ffi")]
const GHOSTTY_SUCCESS: i32 = 0;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("libghostty-vt FFI not compiled in; rebuild with `--features ffi`")]
    FfiDisabled,
    #[error("libghostty-vt returned a null handle")]
    NullHandle,
    #[error("libghostty-vt returned error code {0}")]
    Ffi(i32),
}

/// Returns true if the crate was compiled with the `ffi` feature and
/// libghostty-vt is linked. Useful for tests that should be skipped when
/// the vendored archive isn't present.
pub const fn ffi_available() -> bool {
    cfg!(feature = "ffi")
}

// ============================================================================
// FFI smoke
// ============================================================================
//
// A minimal end-to-end exercise of the libghostty-vt FFI:
//   1. Construct a terminal at a known size.
//   2. Write a few VT bytes.
//   3. Free the terminal.
//
// The point isn't to test libghostty-vt itself — Ghostty has its own
// test suite — but to assert that our build pipeline works end-to-end:
// the static archive exists, bindgen produced a usable wrapper, the
// linker resolves the symbols, and a basic call sequence doesn't
// segfault. CI runs this with `--features ffi` after building
// libghostty-vt; the non-ffi default never touches the FFI layer.
//
// As more libghostty-vt capabilities get wrapped in real safe APIs
// (Phase 5/6/7), they'll move into dedicated modules; this single smoke
// stays as an early-warning canary on the build pipeline.

/// Run the FFI smoke. Returns Ok on success, an `Error::Ffi(code)` if
/// libghostty-vt rejects any call, or `Error::FfiDisabled` if the
/// crate was built without the `ffi` feature.
#[cfg(feature = "ffi")]
pub fn vt_smoke() -> Result<(), Error> {
    use std::ptr;

    let opts = sys::GhosttyTerminalOptions {
        cols: 80,
        rows: 24,
        max_scrollback: 0,
    };
    let mut term: sys::GhosttyTerminal = ptr::null_mut();

    // SAFETY: passing a null allocator (libghostty-vt's default), an
    // out-pointer we own, and a stack-allocated options struct.
    // Matches the legacy Go binding's call shape exactly.
    let rc = unsafe { sys::ghostty_terminal_new(ptr::null_mut(), &mut term, opts) };
    if rc != GHOSTTY_SUCCESS {
        return Err(Error::Ffi(rc));
    }
    if term.is_null() {
        return Err(Error::NullHandle);
    }

    let bytes: &[u8] = b"hello\r\n";
    // SAFETY: term is non-null per the check above; bytes is a valid
    // slice with a known length. ghostty_terminal_vt_write copies the
    // bytes internally, so the slice's lifetime ending here is fine.
    unsafe {
        sys::ghostty_terminal_vt_write(term, bytes.as_ptr(), bytes.len());
    }

    // SAFETY: term is non-null; ghostty_terminal_free is the documented
    // destructor. After this the handle is no longer valid.
    unsafe { sys::ghostty_terminal_free(term) };

    Ok(())
}

#[cfg(not(feature = "ffi"))]
pub fn vt_smoke() -> Result<(), Error> {
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
}
