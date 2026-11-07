//! The flutter_rust_bridge surface: [`app::BridgeApp`] and
//! [`terminal::BridgeTerminal`] wrap [`crate::app::RoostApp`] and
//! [`crate::TerminalHandle`] as opaque Dart objects, their callback sinks
//! become Dart streams, and [`types`] holds the plain values that cross.
//!
//! Which thread a call runs on is chosen per call. `#[frb(sync)]` runs on
//! the Dart isolate's thread, in call order: the input calls (keys, text,
//! paste, scroll, resize) are channel sends to the owner thread, and
//! running them in order is what keeps typed characters in order. Calls
//! that wait (a frame pull, `app_paused`, disk writes) run on the bridge's
//! thread pool, never on `RoostApp`'s own runtime, which they would block.

pub mod app;
pub mod terminal;
pub mod types;
