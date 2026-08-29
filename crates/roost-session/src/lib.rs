//! `roost-session` — a Roost workspace with no user interface.
//!
//! The same engine a UI embeds — workspace, persistence, PTY supervisor,
//! IPC dispatch — running as a background daemon whose only surface is
//! its socket. It is what HS-1 of the host-sessions direction needs: a
//! place for tabs to *live* on a machine, so a client can come and go
//! (and eventually arrive over SSH) without the shells caring.
//!
//! # What makes it a session rather than a headless UI
//!
//! Two things. `IpcHandler::with_session` promotes the socket:
//! `session.identify`, `session.stop`, `session.connect` and
//! `tab.attach` start answering, tab sizes default to this session's
//! stated geometry rather than a window's, and every mutating op becomes
//! gated on the stop latch. `PtySupervisor::enable_server_vt` gives
//! every tab an authoritative server terminal, which is what makes a
//! headless `tab.dump`, an answered device query, and an attach stream
//! possible at all. The ops that still need a real UI — screenshots,
//! the palette, clipboard — fail with `internal: no UI attached`, which
//! is exactly true.
//!
//! # Layout
//!
//! * [`start`] — the startup order, from the launch-cwd hint to the
//!   locks. Ordering constraints are documented there, because that is
//!   where breaking one bites.
//! * [`daemonize`] — the fork and the parent/child readiness handshake.
//! * [`readiness`] — the one line a starting session says about itself.
//! * [`serve`] — bind, hydrate, answer, stop. In-process and
//!   fork-free, so tests drive it directly.
//! * [`hydrate`] — the saved layout back into live shells.
//! * [`socket_guard`] — unlink our socket, never a successor's.
//! * [`identity`], [`logging`], [`consts`] — session id + timestamp, the
//!   file appender, and every named constant.

pub mod consts;
pub mod daemonize;
pub mod hydrate;
pub mod identity;
pub mod logging;
pub mod readiness;
pub mod serve;
pub mod socket_guard;
pub mod start;

pub use readiness::{Readiness, Verdict};
pub use serve::{serve, SessionConfig};
pub use start::{capture_launch_cwd, report, set_process_umask, start, Outcome};
