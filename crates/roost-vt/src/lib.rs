//! Safe Rust wrapper over libghostty-vt.
//!
//! libghostty-vt is the VT parser + screen state engine extracted from
//! Ghostty. We use it (a) on the daemon side for any server-side OSC
//! semantics that need parsing before the UI sees the bytes, and (b) on
//! both UIs for in-process VT parse + render.
//!
//! This crate is intentionally minimal in Phase 2: the FFI layer is
//! gated behind the `ffi` feature so the workspace builds cleanly before
//! `third_party/ghostty/build.sh` has produced the static archive. The
//! safe API on top will grow as the daemon and UIs need it.

#![allow(clippy::needless_pass_by_value)]

#[cfg(feature = "ffi")]
mod sys {
    #![allow(non_upper_case_globals)]
    #![allow(non_camel_case_types)]
    #![allow(non_snake_case)]
    #![allow(dead_code)]

    include!(concat!(env!("OUT_DIR"), "/ghostty_vt.rs"));
}

use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("libghostty-vt FFI not compiled in; rebuild with `--features ffi`")]
    FfiDisabled,
    #[error("libghostty-vt returned a null handle")]
    NullHandle,
}

/// Returns true if the crate was compiled with the `ffi` feature and
/// libghostty-vt is linked. Useful for tests that should be skipped when
/// the vendored archive isn't present.
pub const fn ffi_available() -> bool {
    cfg!(feature = "ffi")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ffi_flag_matches_cfg() {
        // Just confirms the helper compiles under both feature flag states.
        let _ = ffi_available();
    }
}
