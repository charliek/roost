//! Peer-credential lookup for the IPC socket.
//!
//! Socket mode bits (`0600`, set at bind) stop another user opening the
//! socket **on this machine**. They stop nothing once the socket is
//! forwarded — an `ssh -R` remote-forwarded Unix socket is opened by
//! sshd running as the forwarding user, so the file mode says nothing
//! about who is on the other end. The peer's effective UID, read from
//! the kernel, does.
//!
//! Enforcement is opt-in per server ([`crate::IpcServer::require_uid`]);
//! the UI sockets keep today's behavior. The session profile turns it
//! on in HS-1.
//!
//! Every lookup failure is a **reject**, never an allow: a connection
//! whose owner we cannot name is exactly the connection not to trust.

use std::os::unix::io::AsRawFd;

use anyhow::Context;
use tokio::net::UnixStream;

/// This process's effective UID — what [`peer_uid`] is compared against
/// when a server is configured with `require_same_uid()`.
pub fn current_euid() -> u32 {
    // SAFETY: `geteuid` reads process-global state, takes no arguments
    // and cannot fail; there is no unsafe precondition to uphold.
    unsafe { libc::geteuid() }
}

/// The effective UID of the process on the other end of `stream`.
///
/// Linux reads `SO_PEERCRED`, macOS `getpeereid(3)`. Both report the
/// peer's *effective* UID as of the connect, which is what an access
/// decision needs.
#[cfg(target_os = "linux")]
pub fn peer_uid(stream: &UnixStream) -> anyhow::Result<u32> {
    let mut cred = libc::ucred {
        pid: 0,
        uid: 0,
        gid: 0,
    };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: `stream` owns the fd for the whole call, so it cannot be
    // closed underneath `getsockopt`. `len` truthfully describes the
    // `ucred` whose address we pass, and that is the only memory the
    // kernel writes.
    let rc = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            std::ptr::addr_of_mut!(cred).cast::<libc::c_void>(),
            &mut len,
        )
    };
    if rc != 0 {
        return Err(std::io::Error::last_os_error())
            .context("SO_PEERCRED lookup on an ipc connection");
    }
    if len as usize != std::mem::size_of::<libc::ucred>() {
        anyhow::bail!("SO_PEERCRED returned {len} bytes, expected a full struct ucred");
    }
    Ok(cred.uid)
}

/// See the Linux sibling above for the contract.
#[cfg(target_os = "macos")]
pub fn peer_uid(stream: &UnixStream) -> anyhow::Result<u32> {
    let mut uid: libc::uid_t = 0;
    let mut gid: libc::gid_t = 0;
    // SAFETY: `stream` owns the fd for the whole call. Both out-params
    // are live locals of exactly the types `getpeereid` writes.
    let rc = unsafe { libc::getpeereid(stream.as_raw_fd(), &mut uid, &mut gid) };
    if rc != 0 {
        return Err(std::io::Error::last_os_error()).context("getpeereid on an ipc connection");
    }
    Ok(uid)
}

/// Fail-closed on platforms Roost does not ship. Keeps the crate
/// compiling everywhere without ever handing out an unchecked allow.
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub fn peer_uid(_stream: &UnixStream) -> anyhow::Result<u32> {
    anyhow::bail!("peer-credential lookup is not implemented on this platform")
}
