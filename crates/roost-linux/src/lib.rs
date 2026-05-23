//! Roost Linux UI — library surface (M3a).
//!
//! Phase 6a M3a (daemon-removal refactor): the new infrastructure
//! for the post-daemon Linux UI lives here — the PTY supervisor,
//! workspace state machine, JSON IPC server, and single-instance
//! lock. M3b rewires the gtk4-rs UI (`app.rs`, `tab_session.rs`)
//! to consume these modules and deletes the gRPC client.
//!
//! This split keeps the workspace buildable across the refactor:
//! M3a adds the modules without consumers; M3b flips the
//! consumers. The legacy modules (`app`, `tab_session`,
//! `terminal_view`, etc.) stay declared in `main.rs` only — they
//! still talk gRPC to `roost-core` through M3a and switch over in
//! M3b.
//!
//! Test surface: `crates/roost-linux/tests/*.rs` consumes
//! [`daemon::Workspace`], [`daemon::PtySupervisor`], and
//! [`ipc::IpcHandler`] without needing a glib main loop or a GTK
//! display.

#![deny(unsafe_op_in_unsafe_fn)]

pub mod daemon;
pub mod ipc;
pub mod single_instance;
