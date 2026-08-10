//! Is anything listening on this socket path?
//!
//! One answer, shared by everything that has to decide it: the UI's
//! bind path ([`crate::server::IpcServer::bind`]), `roostctl doctor`,
//! and — mirrored, not shared — `mac/Sources/Roost/IPCServer.swift`.
//! Before this module each site had its own copy and they disagreed.
//!
//! The rule is **fail-safe**: only `ECONNREFUSED` (nothing is queued
//! for accept) or a genuinely absent path mean the socket is stale.
//! Everything else means assume-live. The case that forces this is
//! Linux's `connect(2)` on an `AF_UNIX` stream socket whose accept
//! backlog is full: it returns `EAGAIN`, and the listener behind it is
//! very much alive. A predicate that read "success means live" (which
//! is correct on Darwin, where the backlog case does not arise the
//! same way) would classify that busy listener as stale and unlink it.
//!
//! A probe is not what makes unlinking safe. Probe-then-unlink is
//! inherently TOCTOU — A probes refused, B binds and starts serving, A
//! unlinks B's socket. Safety comes from holding the socket/bind lock
//! (`BundleProfile::socket_lock_path`) across the whole
//! probe→unlink→bind sequence. The probe is the second line: it stops
//! a process that holds the lock for the *wrong* socket directory, or
//! whose lock file was removed underneath it, from stealing a live
//! socket.

use std::path::Path;
use std::time::Duration;

use tokio::net::UnixStream;

/// How long a liveness probe waits for `connect(2)` before giving up.
/// A Unix-domain connect completes (or fails) in the kernel without
/// blocking on the peer, so exceeding this is already anomalous — and
/// [`SocketState::Indeterminate`] keeps that anomaly on the safe side.
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(2);

/// What a [`probe`] found at a socket path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SocketState {
    /// Nothing at the path.
    Missing,
    /// Something is there, but it isn't a socket. Carries a
    /// human-readable file-type name.
    NotASocket(&'static str),
    /// A listener answered.
    Live,
    /// A socket file outlived its listener: `connect` was refused.
    Stale,
    /// Neither refused nor accepted — a full accept backlog
    /// (`EAGAIN` on Linux), `EPERM`, a timeout, an unreadable parent.
    /// Callers about to unlink MUST treat this as live.
    Indeterminate(String),
}

impl SocketState {
    /// True only for the two states that prove no listener can lose a
    /// socket by our unlinking it.
    pub fn safe_to_unlink(&self) -> bool {
        matches!(self, SocketState::Missing | SocketState::Stale)
    }
}

/// Classify `path`: stat it, then (if it is a socket) `connect` to it.
pub async fn probe(path: &Path, timeout: Duration) -> SocketState {
    match std::fs::symlink_metadata(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return SocketState::Missing,
        Err(e) => return SocketState::Indeterminate(e.to_string()),
        Ok(meta) => {
            if !is_socket(meta.file_type()) {
                return SocketState::NotASocket(describe_file_type(meta.file_type()));
            }
        }
    }
    match tokio::time::timeout(timeout, UnixStream::connect(path)).await {
        Ok(Ok(_)) => SocketState::Live,
        Ok(Err(e)) => classify_connect_error(&e),
        Err(_) => SocketState::Indeterminate("connect timed out".into()),
    }
}

/// The errno rule on its own, so it can be tested without conjuring a
/// listener with a full accept backlog.
pub fn classify_connect_error(err: &std::io::Error) -> SocketState {
    match err.kind() {
        std::io::ErrorKind::ConnectionRefused => SocketState::Stale,
        std::io::ErrorKind::NotFound => SocketState::Missing,
        _ => SocketState::Indeterminate(err.to_string()),
    }
}

#[cfg(unix)]
pub fn is_socket(file_type: std::fs::FileType) -> bool {
    use std::os::unix::fs::FileTypeExt;
    file_type.is_socket()
}

#[cfg(not(unix))]
pub fn is_socket(_file_type: std::fs::FileType) -> bool {
    false
}

#[cfg(unix)]
pub fn describe_file_type(file_type: std::fs::FileType) -> &'static str {
    use std::os::unix::fs::FileTypeExt;
    if file_type.is_symlink() {
        "symlink"
    } else if file_type.is_dir() {
        "directory"
    } else if file_type.is_file() {
        "regular file"
    } else if file_type.is_fifo() {
        "fifo"
    } else if file_type.is_char_device() || file_type.is_block_device() {
        "device"
    } else {
        "unknown file type"
    }
}

#[cfg(not(unix))]
pub fn describe_file_type(_file_type: std::fs::FileType) -> &'static str {
    "unknown file type"
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Error, ErrorKind};

    #[test]
    fn only_connection_refused_reads_as_stale() {
        assert_eq!(
            classify_connect_error(&Error::from(ErrorKind::ConnectionRefused)),
            SocketState::Stale
        );
        assert_eq!(
            classify_connect_error(&Error::from(ErrorKind::NotFound)),
            SocketState::Missing
        );
    }

    /// The rule that matters on Linux: `connect(2)` to an `AF_UNIX`
    /// stream socket with a full accept backlog fails with `EAGAIN`,
    /// and the listener is alive. Treating that as stale would unlink
    /// a live UI's socket out from under it.
    #[test]
    fn a_full_accept_backlog_is_live_not_stale() {
        let eagain = Error::from_raw_os_error(libc::EAGAIN);
        let state = classify_connect_error(&eagain);
        assert!(
            !state.safe_to_unlink(),
            "EAGAIN (backlog full) must never authorize an unlink, got {state:?}"
        );
    }

    #[test]
    fn unexpected_errnos_stay_on_the_safe_side() {
        for kind in [
            ErrorKind::PermissionDenied,
            ErrorKind::TimedOut,
            ErrorKind::Interrupted,
            ErrorKind::AddrInUse,
        ] {
            let state = classify_connect_error(&Error::from(kind));
            assert!(
                !state.safe_to_unlink(),
                "{kind:?} must not authorize an unlink, got {state:?}"
            );
        }
    }

    #[test]
    fn only_missing_and_stale_authorize_an_unlink() {
        assert!(SocketState::Missing.safe_to_unlink());
        assert!(SocketState::Stale.safe_to_unlink());
        assert!(!SocketState::Live.safe_to_unlink());
        assert!(!SocketState::NotASocket("regular file").safe_to_unlink());
        assert!(!SocketState::Indeterminate("whatever".into()).safe_to_unlink());
    }

    #[tokio::test]
    async fn a_missing_path_is_missing() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            probe(&dir.path().join("nope.sock"), PROBE_TIMEOUT).await,
            SocketState::Missing
        );
    }

    #[tokio::test]
    async fn a_regular_file_is_not_a_socket() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("roost.sock");
        std::fs::write(&path, b"not a socket").unwrap();
        assert_eq!(
            probe(&path, PROBE_TIMEOUT).await,
            SocketState::NotASocket("regular file")
        );
    }

    #[tokio::test]
    async fn a_bound_listener_is_live_and_its_corpse_is_stale() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("roost.sock");
        let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        assert_eq!(probe(&path, PROBE_TIMEOUT).await, SocketState::Live);
        // Dropping a listener does NOT unlink its path — which is
        // exactly how a stale socket comes to exist after a SIGKILL.
        drop(listener);
        assert_eq!(probe(&path, PROBE_TIMEOUT).await, SocketState::Stale);
    }
}
