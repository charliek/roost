//! The Linux native seam — what this crate says to the desktop past winit
//! and iced.
//!
//! [`crate::macos`]'s two rules hold here unchanged:
//!
//! * **Main thread only.** Every entry point runs inside an iced update or
//!   an `iced::window::run` callback, on the thread winit's event loop runs
//!   on.
//! * **Nothing retained escapes.** Callers hand in plain data and get plain
//!   data back; no protocol object crosses out of this module.
//!
//! First consumer: [`wayland`], the window raise a notification click asks
//! for (#351).

pub(crate) mod wayland;
