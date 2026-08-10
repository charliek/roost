//! Cross-platform single-instance locks for a Roost UI profile.
//!
//! M3a of the daemon-removal refactor. GApplication's D-Bus
//! uniqueness check doesn't work on macOS (no system D-Bus session
//! bus by default), so we use the same flock-on-pidfile mechanism
//! the Mac UI picked up in M4. One acquisition is:
//!
//! 1. Open the lock file (`O_CREAT | O_RDWR`).
//! 2. `flock(LOCK_EX | LOCK_NB)`. Fails → another live instance
//!    owns it; read the PID, return [`AcquireError::AlreadyHeld`].
//! 3. Truncate + write our PID to the lock file (best-effort —
//!    the flock is the source of truth, the PID is just for
//!    diagnostics + an "activate the running window" hint).
//!
//! # Two locks, not one
//!
//! A UI process owns two independent resources, and they move
//! independently, so [`acquire_locks`] takes one lock for each:
//!
//! * the **socket/bind lock** (`BundleProfile::socket_lock_path`,
//!   `<socket dir>/roost.lock`) guards the probe→unlink→bind sequence
//!   and the socket's lifetime, and follows `XDG_RUNTIME_DIR`;
//! * the **state lock** (`BundleProfile::state_lock_path`,
//!   `<state dir>/state.lock`) guards `state.json`, and follows
//!   `ROOST_STATE_DIR`.
//!
//! The original single lock lived beside the socket. That was right
//! about the socket and wrong about state: two processes with the same
//! `ROOST_STATE_DIR` and different runtime dirs both started and wrote
//! one `state.json`. Moving the one lock would have been a
//! single-instance regression rather than a fix — see the module docs
//! on `roost_ipc::paths`. Neither lock is legacy; dropping the runtime
//! one reopens the socket race permanently.
//!
//! **Acquisition order is load-bearing**: socket lock first, then
//! state lock, and release in the reverse order. If one process took
//! state-then-socket while another took socket-then-state, both could
//! refuse and neither would start.
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

use std::fs::{File, OpenOptions, TryLockError};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

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

/// Both locks a UI process must own to run: the socket/bind lock and
/// the state lock.
///
/// Field order is load-bearing. Rust drops fields in declaration
/// order, so `state` before `socket` releases in the reverse of the
/// order [`acquire_locks`] takes them. Do not reorder.
#[derive(Debug)]
pub struct InstanceLocks {
    /// `None` only in the degenerate case where both paths name the
    /// same file — a second `flock` on it would contend with the first
    /// (locks belong to the open file description), so we hold one.
    state: Option<InstanceLock>,
    socket: InstanceLock,
}

impl InstanceLocks {
    pub fn socket_lock_path(&self) -> &Path {
        self.socket.lock_path()
    }

    /// The state lock's path, or `None` when it collapsed onto the
    /// socket lock's file and the single held lock guards both.
    pub fn state_lock_path(&self) -> Option<&Path> {
        self.state.as_ref().map(InstanceLock::lock_path)
    }
}

/// Outcome of an [`acquire_locks`] attempt.
#[derive(Debug, thiserror::Error)]
pub enum LocksError {
    /// Another instance is serving the socket in this runtime dir.
    /// Callers activate that instance and exit 0.
    #[error("another instance owns the socket (pid {pid}); lock: {}", path.display())]
    SocketHeld { pid: i32, path: PathBuf },
    /// We took the socket lock, so nobody is on our socket — which
    /// makes this by construction the cross-runtime-dir case: some
    /// other process shares our state dir but not our socket, so we
    /// cannot activate it and cannot safely write `state.json`.
    /// Callers refuse to start.
    #[error("another instance owns this state directory (pid {pid}); lock: {}", path.display())]
    StateHeld { pid: i32, path: PathBuf },
    #[error("lock {}: {source}", path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

/// Claim both single-instance locks, socket first.
///
/// Failing the state lock releases the socket lock before returning,
/// so a refusing process never sits on a lock a peer is waiting for.
///
/// Note that acquiring the state lock creates the state dir eagerly
/// (`create_dir_all` below), where `persist_state` used to create it
/// lazily on first write. A UI that starts and never persists now
/// leaves an empty state dir behind.
pub fn acquire_locks(
    socket_lock_path: impl AsRef<Path>,
    state_lock_path: impl AsRef<Path>,
) -> Result<InstanceLocks, LocksError> {
    let socket_lock_path = socket_lock_path.as_ref().to_path_buf();
    let state_lock_path = state_lock_path.as_ref().to_path_buf();

    let socket = match acquire(&socket_lock_path) {
        Ok(lock) => lock,
        Err(AcquireError::AlreadyHeld(pid)) => {
            return Err(LocksError::SocketHeld {
                pid,
                path: socket_lock_path,
            })
        }
        Err(AcquireError::Io(source)) => {
            return Err(LocksError::Io {
                path: socket_lock_path,
                source,
            })
        }
    };

    // Defence in depth for the case where both paths name one file.
    // `BundleProfile` gives the two locks different filenames, so
    // getting here takes a caller passing one path twice, or a state
    // dir symlinked onto the socket dir with matching names. Either
    // way the second acquisition would contend with the first: `flock`
    // is per-open-file-description, so our own second open+flock
    // returns WouldBlock and we would refuse to start against
    // ourselves.
    if same_file(&socket_lock_path, &state_lock_path) {
        return Ok(InstanceLocks {
            state: None,
            socket,
        });
    }

    let state = match acquire(&state_lock_path) {
        Ok(lock) => lock,
        // `socket` drops at the end of this scope — after the error is
        // built, before the caller sees it — which is the reverse
        // release the acquisition order requires.
        Err(AcquireError::AlreadyHeld(pid)) => {
            return Err(LocksError::StateHeld {
                pid,
                path: state_lock_path,
            })
        }
        Err(AcquireError::Io(source)) => {
            return Err(LocksError::Io {
                path: state_lock_path,
                source,
            })
        }
    };

    Ok(InstanceLocks {
        state: Some(state),
        socket,
    })
}

/// Do these two paths name the same file? Compares canonicalized
/// parents (the lock files themselves may not exist yet) so a
/// symlinked state dir pointing at the socket dir is caught too. Falls
/// back to a literal comparison when a parent can't be canonicalized —
/// which means it doesn't exist, which means it isn't the directory
/// we just created for the socket lock.
fn same_file(a: &Path, b: &Path) -> bool {
    if a == b {
        return true;
    }
    with_canonical_parent(a) == with_canonical_parent(b)
}

fn with_canonical_parent(path: &Path) -> PathBuf {
    match (path.parent(), path.file_name()) {
        (Some(parent), Some(name)) => match std::fs::canonicalize(parent) {
            Ok(parent) => parent.join(name),
            Err(_) => path.to_path_buf(),
        },
        _ => path.to_path_buf(),
    }
}

/// Attempt to claim one single-instance lock at `lock_path`.
///
/// On success returns an [`InstanceLock`] that must be held for
/// the lifetime of the UI process. On contention returns
/// [`AcquireError::AlreadyHeld`] with the previous holder's PID
/// (or `0` if the PID could not be read).
///
/// UIs call [`acquire_locks`] instead; this is the primitive it and
/// the tests are built on.
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

    // `File::try_lock` is `flock(LOCK_EX | LOCK_NB)` on unix, and unlike
    // the io::Error it replaces it distinguishes contention from a real
    // failure in the type rather than by errno.
    if let Err(err) = file.try_lock() {
        return Err(match err {
            // Read whatever PID the previous holder wrote (best-effort).
            TryLockError::WouldBlock => AcquireError::AlreadyHeld(read_pid(&file).unwrap_or(0)),
            TryLockError::Error(err) => AcquireError::Io(err),
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

    /// The bug the second lock exists to fix: same state dir, different
    /// runtime dirs. Before this, both processes started and wrote one
    /// `state.json`.
    #[test]
    fn same_state_dir_with_different_runtime_dirs_contends() {
        let runtime_a = tempdir().unwrap();
        let runtime_b = tempdir().unwrap();
        let state = tempdir().unwrap();
        let state_lock = state.path().join("state.lock");

        let _first = acquire_locks(runtime_a.path().join("roost.lock"), &state_lock).unwrap();
        match acquire_locks(runtime_b.path().join("roost.lock"), &state_lock) {
            Err(LocksError::StateHeld { pid, path }) => {
                assert_eq!(pid, std::process::id() as i32);
                assert_eq!(path, state_lock);
            }
            other => panic!("expected StateHeld, got {other:?}"),
        }
    }

    /// The regression a naive "just move the lock next to state.json"
    /// would have introduced: different state dirs, one socket. Both
    /// would bind, the second unlinking the first's socket.
    #[test]
    fn same_socket_with_different_state_dirs_contends() {
        let runtime = tempdir().unwrap();
        let state_a = tempdir().unwrap();
        let state_b = tempdir().unwrap();
        let socket_lock = runtime.path().join("roost.lock");

        let _first = acquire_locks(&socket_lock, state_a.path().join("state.lock")).unwrap();
        match acquire_locks(&socket_lock, state_b.path().join("state.lock")) {
            Err(LocksError::SocketHeld { pid, path }) => {
                assert_eq!(pid, std::process::id() as i32);
                assert_eq!(path, socket_lock);
            }
            other => panic!("expected SocketHeld, got {other:?}"),
        }
    }

    /// Reverse release. A process that refuses because the state dir is
    /// taken must not sit on the socket lock — the instance that owns
    /// the state dir may be about to want it.
    #[test]
    fn refusing_on_the_state_lock_releases_the_socket_lock() {
        let runtime = tempdir().unwrap();
        let state = tempdir().unwrap();
        let socket_lock = runtime.path().join("roost.lock");
        let contended_state = state.path().join("state.lock");
        let _holder = acquire(&contended_state).unwrap();

        match acquire_locks(&socket_lock, &contended_state) {
            Err(LocksError::StateHeld { .. }) => {}
            other => panic!("expected StateHeld, got {other:?}"),
        }

        acquire(&socket_lock).expect("the socket lock must have been released on refusal");
    }

    /// R1. When both paths name one file, a second `flock` would
    /// contend with the first — `flock` is per-open-file-description,
    /// which `second_acquire_is_already_held` proves. Degrade to a
    /// single acquisition rather than refusing to start against
    /// ourselves.
    #[test]
    fn one_shared_path_degrades_to_a_single_acquisition() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("roost.lock");
        let locks = acquire_locks(&path, &path).expect("must not contend with itself");
        assert_eq!(locks.socket_lock_path(), path);
        assert_eq!(locks.state_lock_path(), None);
    }

    /// Same degradation, reached the way it could actually happen: a
    /// state dir that is a symlink to the socket dir, with lock names
    /// that happen to match.
    #[test]
    fn a_symlinked_state_dir_degrades_to_a_single_acquisition() {
        let dir = tempdir().unwrap();
        let runtime = dir.path().join("runtime");
        std::fs::create_dir(&runtime).unwrap();
        let linked = dir.path().join("state-link");
        std::os::unix::fs::symlink(&runtime, &linked).unwrap();

        let locks = acquire_locks(runtime.join("roost.lock"), linked.join("roost.lock"))
            .expect("a symlinked state dir must not contend with itself");
        assert_eq!(locks.state_lock_path(), None);
    }

    /// Distinct directories are the normal case and must NOT degrade —
    /// otherwise the same-path guard would silently disable the state
    /// lock everywhere.
    #[test]
    fn distinct_paths_take_both_locks() {
        let runtime = tempdir().unwrap();
        let state = tempdir().unwrap();
        let locks = acquire_locks(
            runtime.path().join("roost.lock"),
            state.path().join("state.lock"),
        )
        .unwrap();
        assert!(locks.state_lock_path().is_some());
        assert!(locks.socket_lock_path().exists());
        assert!(locks.state_lock_path().unwrap().exists());
    }

    #[test]
    fn dropping_both_locks_frees_them_for_the_next_start() {
        let runtime = tempdir().unwrap();
        let state = tempdir().unwrap();
        let socket_lock = runtime.path().join("roost.lock");
        let state_lock = state.path().join("state.lock");

        let locks = acquire_locks(&socket_lock, &state_lock).unwrap();
        drop(locks);

        acquire_locks(&socket_lock, &state_lock).expect("both locks must be free after a drop");
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
