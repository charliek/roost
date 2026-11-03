//! Roost mobile core: the Rust half of the Flutter client.
//!
//! [`app::RoostApp`] is plan 089 C5 in plain Rust: saved hosts, their
//! connections and workspaces, and attached terminals, reported through
//! callback sinks. Each attached tab's libghostty terminal lives on one
//! owner thread ([`owner::TerminalOwner`]), fed the attach stream's
//! events and pulled for frames ([`frame`], C6). Dart never touches
//! libghostty.

pub mod app;
pub mod frame;
mod handle;
pub mod keys;
mod live;
pub mod owner;
pub mod registry;
pub mod terminal;
pub mod views;

pub use handle::TerminalHandle;

#[cfg(test)]
mod golden;
