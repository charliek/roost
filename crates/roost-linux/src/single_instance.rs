//! Cross-platform single-instance lock for the Linux UI.
//!
//! M3a of the daemon-removal refactor. GApplication's D-Bus
//! uniqueness check doesn't work on macOS (no system D-Bus session
//! bus by default), so we use the same flock-on-pidfile mechanism
//! the Mac UI will pick up in M4. The bind sequence is intentionally
//! TOCTOU-safe:
//!
//! 1. Open the lock file (`O_CREAT | O_RDWR`). The caller passes
//!    `BundleProfile::lock_path()`, which lives next to the socket
//!    (`<socket dir>/roost.lock`), NOT under `state_dir` — so a
//!    `ROOST_STATE_DIR` override doesn't move the lock.
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
        // Drop releases the flock via `_file`'s File drop — that's
        // the only signal that matters for "this instance is gone."
        //
        // We deliberately do NOT unlink the lock file here, because
        // the file handle is dropped AFTER this body returns (drop
        // order: fields drop in declaration order *after* the drop
        // impl runs, with `_file` listed first → released first,
        // but `remove_file(&path)` still risks racing with another
        // process that has already opened the file by name).
        //
        // Stale lock files left behind after a clean exit are
        // harmless: the next `acquire()` overwrites the PID
        // contents after successfully taking the flock. Callers
        // that want explicit cleanup can call `release()`.
    }
}

impl InstanceLock {
    /// Explicit consuming cleanup: drop the file handle (releases
    /// the flock), then unlink the lock file. Safe because the
    /// flock is gone before we touch the path.
    pub fn release(self) -> std::io::Result<()> {
        let path = self.path.clone();
        drop(self);
        match std::fs::remove_file(&path) {
            Ok(_) => Ok(()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(err),
        }
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
        // Drop releases the flock; the file may or may not still
        // exist (we no longer unlink on Drop because that races
        // with concurrent acquires by name). Either way the next
        // acquire takes the flock cleanly.
        let second = acquire(dir.path().join("roost.lock")).unwrap();
        assert!(second.lock_path().exists());
    }

    #[test]
    fn release_unlinks_the_lock_file() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("roost.lock");
        let lock = acquire(&path).unwrap();
        assert!(path.exists());
        lock.release().unwrap();
        assert!(!path.exists(), "release() must unlink the lock file");
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
