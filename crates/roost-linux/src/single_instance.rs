//! Cross-platform single-instance lock for the Linux UI.
//!
//! M3a of the daemon-removal refactor. GApplication's D-Bus
//! uniqueness check doesn't work on macOS (no system D-Bus session
//! bus by default), so we use the same flock-on-pidfile mechanism
//! the Mac UI will pick up in M4. The bind sequence is intentionally
//! TOCTOU-safe:
//!
//! 1. Open `<state_dir>/roost.lock` (`O_CREAT | O_RDWR`).
//! 2. `flock(LOCK_EX | LOCK_NB)`. Fails → another live instance
//!    owns it; read the PID, return [`AcquireError::AlreadyHeld`].
//! 3. Truncate + write our PID to the lock file (best-effort —
//!    the flock is the source of truth, the PID is just for
//!    diagnostics + an "activate the running window" hint).
//!
//! The returned [`InstanceLock`] holds the open file descriptor.
//! Dropping it releases the flock.
//!
//! M6 hardens this with the explicit stale-socket recovery loop.

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};

use fs2::FileExt;

/// Live single-instance lock. Drop releases the flock.
#[derive(Debug)]
pub struct InstanceLock {
    /// The locked file. Held to keep the flock alive.
    _file: File,
    /// Pathlist for diagnostics + cleanup.
    path: PathBuf,
}

impl InstanceLock {
    pub fn lock_path(&self) -> &Path {
        &self.path
    }
}

impl Drop for InstanceLock {
    fn drop(&mut self) {
        // Best-effort unlink. We don't error here — the flock is
        // already released by the file drop, which is the signal
        // that matters. Leaving the file behind across a clean
        // shutdown means the next launch overwrites it.
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Outcome of a [`acquire`] attempt.
#[derive(Debug, thiserror::Error)]
pub enum AcquireError {
    #[error("another instance is alive (pid {0})")]
    AlreadyHeld(i32),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

/// Attempt to claim the single-instance lock at `lock_path`.
///
/// On success returns an [`InstanceLock`] that must be held for
/// the lifetime of the UI process. On contention returns
/// [`AcquireError::AlreadyHeld`] with the previous holder's PID
/// (or `0` if the PID could not be read).
pub fn acquire(lock_path: impl AsRef<Path>) -> Result<InstanceLock, AcquireError> {
    let lock_path = lock_path.as_ref().to_path_buf();
    if let Some(parent) = lock_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut file = OpenOptions::new()
        .create(true)
        // Don't truncate at open — we may still need to read the
        // prior holder's PID below if the flock attempt fails.
        // Truncation happens explicitly via `set_len(0)` after we
        // successfully acquire the lock.
        .truncate(false)
        .read(true)
        .write(true)
        .open(&lock_path)?;

    // `fs2::FileExt::try_lock_exclusive` is `flock(LOCK_EX | LOCK_NB)`.
    if let Err(err) = file.try_lock_exclusive() {
        // Read whatever PID the previous holder wrote (best-effort).
        let pid = read_pid(&file).unwrap_or(0);
        // Suppress the unused fd warning on platforms where we
        // don't reference `_raw_fd` directly.
        let _raw_fd = file.as_raw_fd();
        return Err(match err.kind() {
            std::io::ErrorKind::WouldBlock => AcquireError::AlreadyHeld(pid),
            _ => AcquireError::Io(err),
        });
    }

    // We own the lock — write our PID into the file. Truncate
    // first to clear stale PID bytes from a prior holder.
    file.set_len(0)?;
    file.seek(SeekFrom::Start(0))?;
    let pid = std::process::id();
    writeln!(file, "{pid}")?;
    file.flush()?;

    Ok(InstanceLock {
        _file: file,
        path: lock_path,
    })
}

fn read_pid(file: &File) -> std::io::Result<i32> {
    let mut buf = String::new();
    let mut clone = file.try_clone()?;
    clone.seek(SeekFrom::Start(0))?;
    clone.read_to_string(&mut buf)?;
    buf.trim()
        .parse::<i32>()
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn first_acquire_succeeds() {
        let dir = tempdir().unwrap();
        let lock = acquire(dir.path().join("roost.lock")).unwrap();
        assert!(lock.lock_path().exists());
    }

    #[test]
    fn second_acquire_is_already_held() {
        let dir = tempdir().unwrap();
        let _first = acquire(dir.path().join("roost.lock")).unwrap();
        match acquire(dir.path().join("roost.lock")) {
            Err(AcquireError::AlreadyHeld(pid)) => {
                assert_eq!(pid, std::process::id() as i32);
            }
            other => panic!("expected AlreadyHeld, got {other:?}"),
        }
    }

    #[test]
    fn drop_releases_so_next_acquire_succeeds() {
        let dir = tempdir().unwrap();
        let first = acquire(dir.path().join("roost.lock")).unwrap();
        drop(first);
        // The file is unlinked on drop; a fresh acquire creates it.
        let second = acquire(dir.path().join("roost.lock")).unwrap();
        assert!(second.lock_path().exists());
    }

    #[test]
    fn stale_pid_from_a_previous_run_is_overwritten() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("roost.lock");
        // Simulate a previous run that crashed — its lock file is
        // present, with a stale PID, but nothing has flock'd it.
        std::fs::write(&path, "999999\n").unwrap();
        let lock = acquire(&path).unwrap();
        let contents = std::fs::read_to_string(lock.lock_path()).unwrap();
        // Our PID is now in the file.
        assert_eq!(
            contents.trim(),
            std::process::id().to_string(),
            "stale PID should be overwritten",
        );
    }
}
