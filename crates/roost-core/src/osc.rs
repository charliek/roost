//! OSC scanner re-export shim.
//!
//! The implementation lives in `crates/roost-osc/` since Phase 7
//! commit 2 — it's now shared with `crates/roost-linux/`. Daemon-side
//! callers continue to use `crate::osc::*` unchanged; this file makes
//! that import path keep working.

pub use roost_osc::*;
