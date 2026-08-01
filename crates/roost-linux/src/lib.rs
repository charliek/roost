//! GTK/libadwaita adapter for Roost's shared Rust engine.
//!
//! Compatibility modules keep established internal paths stable, but their
//! implementations live in `roost-engine` and `roost-ui-model`. This crate
//! owns GTK widgets, Cairo/Pango drawing, clipboard and notification ports,
//! and event-loop marshalling—not authoritative application state.

#![deny(unsafe_op_in_unsafe_fn)]

pub mod daemon;
pub mod ipc;
pub mod local_client;
pub mod mouse_routing;
pub mod reconcile;
pub mod single_instance;
pub mod word_selection;
