//! The fork that turns `roost-session start` into a background daemon.
//!
//! # Why this runs first
//!
//! `fork(2)` in a multi-threaded process copies only the calling thread,
//! so every lock the other threads held is inherited locked and the
//! child deadlocks the first time it touches one. The daemon therefore
//! forks before *anything* that could start a thread — no tokio runtime,
//! no tracing subscriber with a background writer, no signal driver. That
//! ordering is the reason this module has no dependencies on the rest of
//! the crate beyond the verdict format.
//!
//! # The handshake
//!
//! Parent and child share a pipe. The child does the whole risky part of
//! the start — validate directories, take the locks, bind, hydrate — and
//! then writes exactly one verdict line and closes its end. The parent
//! blocks on that line, relays it to its own stdout, and exits with the
//! matching status. So `roostctl session start` returning 0 means the
//! socket is bound and answering, not merely that a fork succeeded.
//!
//! EOF before a verdict is a failure: the child died on its way up.

use std::fs::File;
use std::io::Write;
use std::os::fd::{FromRawFd, RawFd};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

use crate::consts::{parent_ready_timeout, MAX_VERDICT_BYTES};
use crate::readiness::{Readiness, Verdict};

/// Fork into the background. **Returns only in the child** — the parent
/// relays the child's verdict and calls `exit`.
///
/// The returned [`Readiness`] owns the write end of the handshake pipe;
/// reporting on it is what releases the parent.
pub fn daemonize() -> Result<Readiness> {
    let (read_fd, write_fd) = readiness_pipe()?;

    // SAFETY: no other thread exists yet (see the module docs), so the
    // child inherits a consistent address space and may run arbitrary
    // code rather than being restricted to async-signal-safe calls.
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        let err = std::io::Error::last_os_error();
        close(read_fd);
        close(write_fd);
        return Err(err).context("fork the session daemon");
    }

    if pid > 0 {
        close(write_fd);
        parent_relay_and_exit(read_fd, pid);
    }

    close(read_fd);
    detach_child(write_fd)
}

/// Both ends are close-on-exec.
///
/// The read end because the parent must not leak it, and the write end
/// for a subtler reason: the child spawns shells, and a shell that
/// inherited the write end would hold the pipe open after the child
/// died — turning the parent's prompt EOF into a full timeout wait.
fn readiness_pipe() -> Result<(RawFd, RawFd)> {
    let mut fds = [0 as RawFd; 2];
    // SAFETY: `fds` is a two-element array, which is exactly what
    // `pipe(2)` writes.
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error()).context("create the readiness pipe");
    }
    for fd in fds {
        set_cloexec(fd).context("mark the readiness pipe close-on-exec")?;
    }
    Ok((fds[0], fds[1]))
}

fn set_cloexec(fd: RawFd) -> std::io::Result<()> {
    // SAFETY: plain fcntl on an fd this function owns.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if flags < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: as above.
    if unsafe { libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

fn close(fd: RawFd) {
    // SAFETY: the fd is owned by this module and closed exactly once on
    // each path.
    unsafe { libc::close(fd) };
}

/// The parent half: wait for the verdict, relay it, exit. Never returns.
fn parent_relay_and_exit(read_fd: RawFd, child_pid: libc::pid_t) -> ! {
    let verdict = match read_verdict(read_fd, parent_ready_timeout()) {
        Ok(Some(line)) => Verdict::parse(&line),
        Ok(None) => Verdict::Error(format!(
            "the session (pid {child_pid}) exited before reporting readiness; \
             see the session log"
        )),
        Err(reason) => Verdict::Error(format!(
            "no readiness verdict from the session (pid {child_pid}): {reason}"
        )),
    };
    close(read_fd);
    // stdout, unconditionally: the caller reads one machine-parseable
    // line whether the start worked or not, and distinguishes the two by
    // the exit status.
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "{verdict}");
    let _ = out.flush();
    std::process::exit(verdict.exit_code());
}

/// Read one newline-terminated verdict, bounded by `timeout`.
///
/// `Ok(None)` is EOF before any verdict — the child died. `Err` is a
/// timeout or a read failure, which leaves the child's fate unknown.
fn read_verdict(fd: RawFd, timeout: Duration) -> Result<Option<String>, String> {
    let deadline = Instant::now() + timeout;
    let timed_out = || format!("timed out after {}s", timeout.as_secs());
    let mut buffered = Vec::new();
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(timed_out());
        }
        let mut poll_fd = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        let millis = i32::try_from(remaining.as_millis()).unwrap_or(i32::MAX);
        // SAFETY: one initialized `pollfd`, count matching.
        let ready = unsafe { libc::poll(&mut poll_fd, 1, millis) };
        if ready < 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(format!("poll: {err}"));
        }
        if ready == 0 {
            return Err(timed_out());
        }

        let mut chunk = [0u8; 256];
        // SAFETY: reading at most `chunk.len()` bytes into `chunk`.
        let read = unsafe { libc::read(fd, chunk.as_mut_ptr().cast(), chunk.len()) };
        if read < 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(format!("read: {err}"));
        }
        if read == 0 {
            return Ok(None);
        }
        buffered.extend_from_slice(&chunk[..read as usize]);
        if let Some(end) = buffered.iter().position(|byte| *byte == b'\n') {
            return Ok(Some(String::from_utf8_lossy(&buffered[..end]).into_owned()));
        }
        if buffered.len() > MAX_VERDICT_BYTES {
            return Err("verdict line exceeded its size cap".into());
        }
    }
}

/// The child half: leave the caller's session and terminal behind.
fn detach_child(write_fd: RawFd) -> Result<Readiness> {
    // SAFETY: the child is single-threaded and owns its process group.
    if unsafe { libc::setsid() } < 0 {
        return Err(std::io::Error::last_os_error()).context("setsid");
    }

    // `/` so the daemon never pins a filesystem the user wants to
    // unmount. The launch cwd was captured into memory before this, and
    // every PTY is spawned with an explicit directory, so nothing
    // downstream ever reads this one.
    // SAFETY: a NUL-terminated literal.
    if unsafe { libc::chdir(c"/".as_ptr()) } < 0 {
        return Err(std::io::Error::last_os_error()).context("chdir /");
    }

    // Move the pipe clear of the stdio slots before redirecting them —
    // a caller that launched us with fd 0/1/2 already closed would
    // otherwise have its readiness pipe silently replaced by /dev/null.
    let write_fd = move_above_stdio(write_fd).context("reserve the readiness pipe fd")?;
    redirect_stdio_to_devnull().context("redirect stdio to /dev/null")?;

    // SAFETY: `write_fd` is a live fd this function is handing over
    // exclusively; `File` owns and closes it from here on.
    Ok(Readiness::Pipe(unsafe { File::from_raw_fd(write_fd) }))
}

fn move_above_stdio(fd: RawFd) -> std::io::Result<RawFd> {
    if fd > 2 {
        return Ok(fd);
    }
    // SAFETY: duplicating an fd we own to the lowest free slot >= 3.
    let moved = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 3) };
    if moved < 0 {
        return Err(std::io::Error::last_os_error());
    }
    close(fd);
    Ok(moved)
}

fn redirect_stdio_to_devnull() -> std::io::Result<()> {
    // SAFETY: a NUL-terminated literal path, opened read-write so the
    // one descriptor serves stdin as well as stdout/stderr.
    let null = unsafe { libc::open(c"/dev/null".as_ptr(), libc::O_RDWR) };
    if null < 0 {
        return Err(std::io::Error::last_os_error());
    }
    for slot in [libc::STDIN_FILENO, libc::STDOUT_FILENO, libc::STDERR_FILENO] {
        // SAFETY: both fds are live; dup2 closes `slot` first.
        if unsafe { libc::dup2(null, slot) } < 0 {
            let err = std::io::Error::last_os_error();
            close(null);
            return Err(err);
        }
    }
    if null > 2 {
        close(null);
    }
    Ok(())
}
