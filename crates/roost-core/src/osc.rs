//! OSC scanner re-export shim.
//!
//! The implementation lives in `crates/roost-osc/` since Phase 7
//! commit 2 — it's now shared with `crates/roost-linux/`. Daemon-side
//! callers continue to use `crate::osc::*` unchanged; this file makes
//! that import path keep working.
//!
//! The merge of `feature/rust-port`'s `aebd408` carries the OSC UTF-8
//! fix into `crates/roost-osc/src/lib.rs` (where the scanner body
//! buffer is now a `Vec<u8>` decoded via `from_utf8_lossy` at dispatch
//! time). The fix mirrors the matching Mac-side change in
//! `mac/Sources/Roost/OscScanner.swift`.

pub use roost_osc::*;
