//! Roost Linux UI — library surface.
//!
//! Phase 6a M3a (daemon-removal refactor): the post-daemon
//! infrastructure for the Linux UI — PTY supervisor, workspace
//! state, JSON IPC server, single-instance lock, and the
//! `LocalClient` adapter that the gtk4-rs UI will consume in M3b.
//!
//! The legacy gRPC client (`client`), the gRPC-bound `tab_session`,
//! and the gRPC-bound `events` modules still live in the bin's
//! own module tree (declared in `main.rs`) through M3a. M3b
//! rewires `app.rs` onto `daemon` + `local_client` and drops those
//! gRPC modules along with `tonic`/`roost-proto`.
//!
//! Test surface: `crates/roost-linux/tests/*.rs` consumes
//! [`daemon::Workspace`], [`daemon::PtySupervisor`],
//! [`ipc::IpcHandler`], and [`local_client::LocalClient`] without
//! needing a glib main loop or a GTK display.

#![deny(unsafe_op_in_unsafe_fn)]

pub mod daemon;
pub mod ipc;
pub mod local_client;
pub mod reconcile;
pub mod single_instance;
