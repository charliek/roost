//! Cross-platform single-instance lock for a Roost UI profile.
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
//! Dropping it flock(LOCK_UN)s explicitly and then closes — closing
//! alone is not enough, because the lock belongs to the open file
//! description and any fork()ed child that inherited the fd would keep
//! it alive until exec (issue #324).
//!
//! One residual window survives that and cannot be closed from here:
//! if the process is SIGKILLed (so `Drop` never runs) while a just-
//! forked child has not yet reached `exec`, that child's inherited
//! description keeps the lock until it does. The window is the length
//! of a fork→exec, it self-heals, and closing it would mean changing
//! how every subprocess in the tree is spawned.
//!
//! M6 hardens this with the explicit stale-socket recovery loop.

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
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
        // Closing the fd is NOT enough (issue #324). flock(2) locks
        // live on the open file description, so a fork()ed child that
        // inherited the fd keeps the lock alive until it execs — and
        // in that window our drop silently fails to release. LOCK_UN
        // clears the lock on the description itself, which every
        // inheriting fd shares, so release is unconditional.
        let _ = self._file.unlock();

        // We deliberately do NOT unlink the lock file here: another
        // process may already have opened it by name and be waiting on
        // the flock. Stale lock files left behind after a clean exit
        // are harmless — the next `acquire()` overwrites the PID
        // contents after successfully taking the flock.
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
    let file = OpenOptions::new()
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
        return Err(match err.kind() {
            std::io::ErrorKind::WouldBlock => AcquireError::AlreadyHeld(pid),
            _ => AcquireError::Io(err),
        });
    }

    // Wrap the locked file BEFORE anything fallible, so every `?`
    // below releases through `Drop`'s LOCK_UN rather than dropping a
    // bare `File` that a forked child could still be holding open.
    let mut lock = InstanceLock {
        _file: file,
        path: lock_path,
    };

    // We own the lock — write our PID into the file. Truncate
    // first to clear stale PID bytes from a prior holder.
    lock._file.set_len(0)?;
    lock._file.seek(SeekFrom::Start(0))?;
    let pid = std::process::id();
    writeln!(lock._file, "{pid}")?;
    lock._file.flush()?;

    Ok(lock)
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
    use std::io::BufRead;
    use std::os::unix::io::AsRawFd;
    use tempfile::tempdir;

    /// Kills and reaps on every exit path, so a panic mid-test can't
    /// leave a child holding an inherited lock description.
    struct ReapOnDrop(std::process::Child);

    impl Drop for ReapOnDrop {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

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

    // Regression guard for #324. flock(2) locks live on the *open file
    // description*, not on the fd or the process, so a fork()ed child
    // that inherited the fd keeps the lock alive until it execs. Before
    // the explicit LOCK_UN in `Drop`, a sibling test in this binary
    // spawning a subprocess during our lock window made `drop` a no-op
    // and the next acquire() see WouldBlock -> AlreadyHeld(our own pid).
    // Clearing FD_CLOEXEC makes that window deterministic instead of
    // ~2%-of-runs.
    #[test]
    fn drop_releases_even_when_a_forked_child_inherited_the_fd() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("roost.lock");
        let lock = acquire(&path).unwrap();

        // SAFETY: plain fcntl on an fd we own; clearing FD_CLOEXEC is
        // what makes the child inherit the lock's open file description.
        let cleared = unsafe { libc::fcntl(lock._file.as_raw_fd(), libc::F_SETFD, 0) };
        assert_eq!(cleared, 0, "failed to clear FD_CLOEXEC on the lock fd");

        let _child = ReapOnDrop(
            std::process::Command::new("/bin/sleep")
                .arg("30")
                .spawn()
                .expect("spawn /bin/sleep"),
        );

        drop(lock);

        match acquire(&path) {
            Ok(second) => assert!(second.lock_path().exists()),
            Err(err) => panic!("drop must release the flock even with an inherited fd: {err:?}"),
        }
    }

    // The same-process test above exercises RAII drop; this one
    // exercises process exit, which is the property the UI actually
    // relies on after a crash. Neither subsumes the other.
    #[test]
    fn a_dead_process_releases_the_lock() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("roost.lock");

        let holder = std::process::Command::new(std::env::current_exe().unwrap())
            .arg("--exact")
            .arg("single_instance::tests::hold_the_lock_until_killed")
            .arg("--ignored")
            .arg("--nocapture")
            .env("ROOST_TEST_LOCK_PATH", &path)
            .stdout(std::process::Stdio::piped())
            .spawn()
            .expect("spawn the lock holder");
        let mut holder = ReapOnDrop(holder);

        // The child prints a line once it owns the flock; waiting on
        // that instead of sleeping keeps the test deterministic. Its
        // libtest banner comes out first, so scan rather than take the
        // first line.
        let stdout = std::io::BufReader::new(holder.0.stdout.take().unwrap());
        let ready = stdout
            .lines()
            .map_while(Result::ok)
            .any(|line| line.trim() == "locked");
        assert!(ready, "the holder never reported that it took the lock");

        match acquire(&path) {
            Err(AcquireError::AlreadyHeld(pid)) => {
                assert_eq!(pid as u32, holder.0.id(), "should report the holder's pid");
            }
            other => panic!("expected AlreadyHeld while the child lives, got {other:?}"),
        }

        holder.0.kill().unwrap();
        holder.0.wait().unwrap();
        acquire(&path).expect("the lock must be free once the holder is gone");
    }

    /// Helper process for [`a_dead_process_releases_the_lock`]: takes
    /// the lock, announces it, then waits to be killed. `#[ignore]`
    /// keeps it out of the normal run; without the env var (a bare
    /// `--include-ignored` sweep) it is a no-op rather than a hang.
    #[test]
    #[ignore = "helper process for a_dead_process_releases_the_lock"]
    fn hold_the_lock_until_killed() {
        let Ok(path) = std::env::var("ROOST_TEST_LOCK_PATH") else {
            return;
        };
        let _lock = acquire(&path).expect("holder must take the lock");
        println!("locked");
        std::io::Write::flush(&mut std::io::stdout()).unwrap();
        // Bounded so a stranded helper cannot outlive the suite.
        std::thread::sleep(std::time::Duration::from_secs(30));
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
