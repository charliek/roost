//! Roost mobile core: the Rust half of the Flutter client.
//!
//! Each attached tab's libghostty terminal lives on one owner thread
//! ([`owner::TerminalOwner`]), fed the attach stream's events and pulled
//! for frames ([`frame`], plan 089 C6). Dart never touches libghostty.

pub mod frame;
pub mod keys;
pub mod owner;
pub mod terminal;

#[cfg(test)]
mod golden;
