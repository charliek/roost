//! Unlink the socket we bound — and only that one.
//!
//! A session's last act is to remove its socket file, and the naive
//! `remove_file(path)` is wrong for the same reason the bind-side probe
//! is careful: by the time shutdown runs, the path may name a *different*
//! socket. A session that crashed and was restarted, a `roostctl` that
//! cleared a stale entry, an operator who moved things around — any of
//! them can leave a live successor sitting at the path this process is
//! about to tidy up. Unlinking it would kill a healthy session and leave
//! every client dialling a name with nothing behind it.
//!
//! So identity, not the name, decides: `(dev, ino)` is recorded from the
//! socket immediately after `bind`, and the unlink happens only if the
//! path still resolves to that exact inode.

use std::os::unix::fs::MetadataExt;
use std::path::Path;

use anyhow::{Context, Result};

/// A bound socket's filesystem identity. Two sockets at the same path
/// across a rebind differ here even though the path is byte-identical.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SocketIdentity {
    dev: u64,
    ino: u64,
}

impl SocketIdentity {
    /// Read the identity of whatever is at `path` right now.
    ///
    /// `symlink_metadata`, not `metadata`: a symlink dropped over the
    /// path must read as the symlink's own identity (and so fail to
    /// match), never as its target's.
    pub fn of(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let meta =
            std::fs::symlink_metadata(path).with_context(|| format!("stat {}", path.display()))?;
        Ok(Self {
            dev: meta.dev(),
            ino: meta.ino(),
        })
    }
}

/// Outcome of the guarded unlink, so the caller can log which of the
/// three genuinely different things happened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unlinked {
    /// The path still named our socket and it is gone now.
    Removed,
    /// Nothing is at the path — somebody already cleaned up.
    Absent,
    /// Something else is at the path. Left strictly alone.
    Foreign,
}

/// Remove `path` if and only if it still resolves to `expected`.
pub fn unlink_if_ours(path: impl AsRef<Path>, expected: SocketIdentity) -> Result<Unlinked> {
    let path = path.as_ref();
    let current = match SocketIdentity::of(path) {
        Ok(current) => current,
        Err(err) => {
            let io = err
                .downcast_ref::<std::io::Error>()
                .map(std::io::Error::kind);
            if io == Some(std::io::ErrorKind::NotFound) {
                return Ok(Unlinked::Absent);
            }
            return Err(err);
        }
    };
    if current != expected {
        return Ok(Unlinked::Foreign);
    }
    // A residual stat→unlink window survives, and cannot be closed
    // without an unlink-by-inode primitive neither Linux nor macOS
    // offers. It is microseconds wide, inside a directory
    // `validate_runtime_dir` proved only we can write to, at a moment
    // when this session has already stopped serving.
    match std::fs::remove_file(path) {
        Ok(()) => Ok(Unlinked::Removed),
        // Lost a race with whoever else was tidying up; the postcondition
        // (our socket is not at this path) holds either way.
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Unlinked::Absent),
        Err(err) => Err(err).with_context(|| format!("unlink {}", path.display())),
    }
}
