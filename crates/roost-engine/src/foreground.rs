//! Native reads of the processes behind a tab: who has its terminal, who
//! leads that group, and whether a stamped process still runs.
//!
//! Linux reads `/proc`; macOS asks libproc. Nothing here holds a lock or
//! a pid: a read can wait (a stat on Linux takes the target's exec lock,
//! `proc_pidinfo` on macOS waits on a process mid-exec), so every caller
//! reads off the UI thread and outside the reap latch, and the start time
//! in each stamp is what tells a reused pid apart.

#[cfg(any(target_os = "linux", target_os = "macos"))]
use std::os::fd::AsRawFd;
use std::os::fd::BorrowedFd;

use roost_ipc::process::{self, Foreground, Liveness, Stamp};

#[cfg(test)]
pub(crate) type ProcReadHook = Box<dyn FnMut(&'static str)>;

#[cfg(test)]
thread_local! {
    /// Run at the start of every native read of a process on this
    /// thread, with what it reads, so a test can see which locks that
    /// read runs under.
    pub(crate) static PROC_READ_HOOK: std::cell::RefCell<Option<ProcReadHook>> =
        const { std::cell::RefCell::new(None) };
}

/// Where every native read of a process begins. Does nothing outside
/// tests.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn proc_read_starts(what: &'static str) {
    #[cfg(test)]
    PROC_READ_HOOK.with_borrow_mut(|hook| {
        if let Some(hook) = hook {
            hook(what);
        }
    });
    #[cfg(not(test))]
    let _ = what;
}

/// A process's run state, as far as the process layer tells them apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Sched {
    Running,
    /// Stopped by job control. A traced process (Linux `t`) is running.
    Stopped,
    /// Exited and not yet reaped (Linux `Z`, or `X` caught mid-teardown).
    Zombie,
}

/// One read of a process. Linux's start time has no sub-second part, so
/// it reads `(starttime, 0)`. `comm` is the kernel's short name: Linux
/// keeps 15 bytes of it, macOS 16 (32 for `pbi_name`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProcStamp {
    pub(crate) pid: i64,
    pub(crate) pgrp: i64,
    pub(crate) session: i64,
    pub(crate) tty: i64,
    pub(crate) tpgid: i64,
    pub(crate) start: (u64, u64),
    pub(crate) sched: Sched,
    pub(crate) comm: String,
}

/// Whether the stamps taken before and after a leader's read are one
/// process, leading its own group, in the child's session and on the
/// child's terminal. The start time is what tells a reused pid apart.
pub(crate) fn same_leader(
    before: &ProcStamp,
    after: &ProcStamp,
    child_sid: i64,
    child_tty: i64,
) -> bool {
    [before, after].iter().all(|stamp| {
        stamp.pgrp == stamp.pid && stamp.session == child_sid && stamp.tty == child_tty
    }) && before.pid == after.pid
        && before.start == after.start
}

/// What reading a stamped pid answered, before it is judged against the
/// stamp ([`classify`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Read {
    Stat(ProcStamp),
    /// The platform says no process has the pid (macOS `ESRCH`).
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    Gone,
    /// Nothing answered for the pid, which can also mean another user's
    /// process hidden from us (Linux `hidepid`): `kill(pid, 0)` decides.
    Missing,
    /// The read failed some other way, or did not parse.
    Failed,
}

/// [`Liveness`] of `stamp` from what reading its pid answered.
/// `kill_errno` is `kill(pid, 0)`'s errno, `0` for success, asked only
/// for a [`Read::Missing`].
///
/// The process is gone when it is a zombie, when another process has its
/// pid (the start time), or when it left the tab's session (a `setsid`
/// daemon is no longer the tab's). Its name is never compared: an exec
/// shim keeps the stamp it was given.
pub(crate) fn classify(stamp: &Stamp, read: Read, kill_errno: impl FnOnce() -> i32) -> Liveness {
    match read {
        Read::Stat(found) => {
            if found.sched == Sched::Zombie
                || found.start != stamp.start
                || found.session != stamp.sid
            {
                Liveness::Gone
            } else if found.sched == Sched::Stopped {
                Liveness::Stopped
            } else {
                Liveness::Running
            }
        }
        Read::Gone => Liveness::Gone,
        Read::Missing => match kill_errno() {
            libc::ESRCH => Liveness::Gone,
            _ => Liveness::Unknown,
        },
        Read::Failed => Liveness::Unknown,
    }
}

/// Whether `stamp`'s process still runs, read now.
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(crate) fn liveness(stamp: &Stamp) -> Liveness {
    let Ok(pid) = u32::try_from(stamp.pid) else {
        return Liveness::Unknown;
    };
    classify(stamp, read_pid(pid), || kill_errno(pid))
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub(crate) fn liveness(_stamp: &Stamp) -> Liveness {
    Liveness::Unknown
}

/// One read of a tab's processes for its process layer: the spawn it
/// read, who had the terminal, the leader a wanted stamp found, and each
/// stamp's [`Liveness`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProcessSample {
    pub(crate) incarnation: u64,
    pub(crate) foreground: Foreground,
    pub(crate) leader: Option<Stamp>,
    pub(crate) liveness: Vec<(Stamp, Liveness)>,
}

impl ProcessSample {
    pub(crate) fn liveness_of(&self, stamp: &Stamp) -> Option<Liveness> {
        self.liveness
            .iter()
            .find(|(read, _)| read == stamp)
            .map(|&(_, liveness)| liveness)
    }
}

/// `kill(pid, 0)`'s errno, `0` when it would deliver. Signal 0 sends
/// nothing; it only asks whether the pid exists and may be signalled.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn kill_errno(pid: u32) -> i32 {
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return libc::ESRCH;
    };
    // SAFETY: signal 0 delivers nothing; `kill` only checks the pid.
    if unsafe { libc::kill(pid, 0) } == 0 {
        0
    } else {
        std::io::Error::last_os_error()
            .raw_os_error()
            .unwrap_or(libc::EINVAL)
    }
}

/// The foreground process group of the terminal behind `fd`, a PTY
/// master: a non-blocking `ioctl`, safe on the UI thread. `None` when
/// the terminal has none, or the call fails (the PTY's child is gone).
pub(crate) fn terminal_pgrp(fd: BorrowedFd<'_>) -> Option<i64> {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        // SAFETY: `tcgetpgrp` only reads the descriptor it is handed,
        // borrowed open for the length of the call.
        let pgrp = unsafe { libc::tcgetpgrp(fd.as_raw_fd()) };
        (pgrp > 0).then(|| i64::from(pgrp))
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = fd;
        None
    }
}

/// [`Foreground`] for a terminal whose foreground group is `pgrp`, under
/// a child `child` that `runs_a_shell` says is a shell or not. portable-pty
/// makes the child a session and group leader, so its own group in front
/// is its pid. A child that is not a shell (a command tab) never reads as
/// the shell: it is the program in front, not a shell that took the
/// terminal back.
pub(crate) fn foreground_of(pgrp: Option<i64>, child: u32, runs_a_shell: bool) -> Foreground {
    match pgrp {
        Some(pgrp) if pgrp == i64::from(child) && runs_a_shell => Foreground::Shell,
        Some(pgrp) => Foreground::Group(pgrp),
        None => Foreground::Unknown,
    }
}

/// The child's own idea of its terminal's foreground group, its `tpgid`:
/// the fallback when `tcgetpgrp` on the master fails. A `/proc` or libproc
/// read, so never on the UI thread.
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(crate) fn tpgid_of(child: u32) -> Option<i64> {
    proc_stamp(child)
        .map(|own| own.tpgid)
        .filter(|&tpgid| tpgid > 0)
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub(crate) fn tpgid_of(_child: u32) -> Option<i64> {
    None
}

/// The shells whose own group in front means "back at a prompt".
const SHELLS: &[&str] = &[
    "sh", "bash", "zsh", "fish", "dash", "ksh", "mksh", "oksh", "pdksh", "ash", "yash", "tcsh",
    "csh", "nu", "elvish", "xonsh", "posh",
];

/// Whether `argv` (as spawned, `argv[0]` first) starts an interactive
/// shell: a known shell given only options. An operand means a command —
/// a script, or what `-c` runs — and so does fish's `--command=…`, the one
/// form that carries its command inside the option. Anything not
/// recognized reads as no shell, which keeps a mark doing what it always
/// did.
pub(crate) fn runs_a_shell<S: AsRef<std::ffi::OsStr>>(argv: &[S]) -> bool {
    let Some(program) = argv.first() else {
        return false;
    };
    let name = std::path::Path::new(program.as_ref())
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("");
    let name = name.strip_prefix('-').unwrap_or(name);
    SHELLS.contains(&name)
        && argv[1..].iter().all(|arg| {
            let Some(arg) = arg.as_ref().to_str() else {
                return false;
            };
            match arg.strip_prefix("--") {
                Some(long) => !long.is_empty() && !long.starts_with("command="),
                None => arg.len() > 1 && arg.starts_with('-'),
            }
        })
}

/// The leader of the terminal's foreground process group, when that is
/// not the child itself — a job the shell is running, or a nested shell —
/// with the [`ProcStamp`] taken when it was found.
///
/// The child's own `tpgid` names the group, with no master fd to keep:
/// portable-pty makes every child a session leader with the pty as its
/// controlling terminal. The leader is not our child, though, and
/// nothing holds its pid, so a read of it counts only when [`same_leader`]
/// matches the stamps taken before and after the read. Asking the
/// terminal for its foreground group again would not do: a dead job's
/// group stays the foreground one until the shell calls `tcsetpgrp`, and
/// its pid can be reused meanwhile. A leader this user can't read
/// (`sudo -s`) has no cwd, like any failed read.
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(crate) struct ForegroundLeader {
    process: LeaderProc,
    before: ProcStamp,
    child_sid: i64,
    child_tty: i64,
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
impl ForegroundLeader {
    pub(crate) fn find(child: u32) -> Option<Self> {
        let own = proc_stamp(child)?;
        let pid = u32::try_from(own.tpgid)
            .ok()
            .filter(|&pid| pid != 0 && pid != child)?;
        let process = LeaderProc::open(pid)?;
        let before = process.stamp()?;
        Some(Self {
            process,
            before,
            child_sid: i64::from(child),
            child_tty: own.tty,
        })
    }

    pub(crate) fn cwd(&self) -> Option<String> {
        self.process.cwd()
    }

    /// Whether a second stamp still matches the first, by [`same_leader`].
    pub(crate) fn unchanged(&self) -> bool {
        self.confirmed().is_some()
    }

    /// A second stamp, when it matches the first by [`same_leader`].
    fn confirmed(&self) -> Option<ProcStamp> {
        self.process
            .stamp()
            .filter(|after| same_leader(&self.before, after, self.child_sid, self.child_tty))
    }
}

/// The [`Stamp`] a layer that just became live takes: the leader of the
/// group in front of `child`'s terminal, confirmed by [`same_leader`] on
/// both sides of its name read. `None` when the shell's own group is in
/// front, the leader can't be read or confirmed, or it is a multiplexer or
/// a remote session ([`process::never_stamped`]).
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(crate) fn stamp_leader(child: u32) -> Option<Stamp> {
    let leader = ForegroundLeader::find(child)?;
    let after = leader.confirmed()?;
    if process::never_stamped(&after.comm) {
        return None;
    }
    Some(Stamp {
        pid: after.pid,
        start: after.start,
        pgid: after.pgrp,
        sid: after.session,
    })
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub(crate) fn stamp_leader(_child: u32) -> Option<Stamp> {
    None
}

/// Current working directory of `pid`. Linux reads `/proc/<pid>/cwd`;
/// macOS asks libproc for `PROC_PIDVNODEPATHINFO`.
#[cfg(target_os = "linux")]
pub(crate) fn cwd_of_pid(pid: u32) -> Option<String> {
    proc_read_starts("cwd");
    std::fs::read_link(format!("/proc/{pid}/cwd"))
        .ok()
        .and_then(|p| p.to_str().map(str::to_owned))
}

#[cfg(target_os = "macos")]
pub(crate) fn cwd_of_pid(pid: u32) -> Option<String> {
    use std::ffi::CStr;
    use std::mem::{size_of, MaybeUninit};

    proc_read_starts("cwd");
    let mut info = MaybeUninit::<libc::proc_vnodepathinfo>::zeroed();
    let size = i32::try_from(size_of::<libc::proc_vnodepathinfo>()).ok()?;
    // SAFETY: `info` is a writable buffer of exactly `size` bytes for the
    // structure requested by PROC_PIDVNODEPATHINFO. We read it only when
    // libproc reports that it initialized the complete structure. The SDK
    // defines `vip_path` as a NUL-terminated MAXPATHLEN char array.
    let written = unsafe {
        libc::proc_pidinfo(
            i32::try_from(pid).ok()?,
            libc::PROC_PIDVNODEPATHINFO,
            0,
            info.as_mut_ptr().cast(),
            size,
        )
    };
    if written != size {
        return None;
    }
    // SAFETY: the full structure was initialized above and `vip_path` is a
    // fixed C string buffer supplied by libproc.
    let path = unsafe {
        CStr::from_ptr(
            info.assume_init_ref()
                .pvi_cdir
                .vip_path
                .as_ptr()
                .cast::<libc::c_char>(),
        )
        .to_str()
        .ok()?
    };
    (!path.is_empty()).then(|| path.to_string())
}

/// A `/proc/<pid>/stat` line's [`ProcStamp`]. The fields are counted
/// after the LAST `)`: the command name before it can hold spaces and
/// parens of its own.
#[cfg(target_os = "linux")]
pub(crate) fn parse_proc_stat(stat: &str) -> Option<ProcStamp> {
    let (head, tail) = stat.rsplit_once(')')?;
    let (pid, comm) = head.split_once(" (")?;
    // `tail` starts at field 3 in proc(5)'s numbering.
    let fields: Vec<&str> = tail.split_whitespace().collect();
    let field = |n: usize| fields.get(n - 3).copied();
    let sched = match field(3)? {
        "T" => Sched::Stopped,
        "Z" | "X" | "x" => Sched::Zombie,
        _ => Sched::Running,
    };
    Some(ProcStamp {
        pid: pid.trim().parse().ok()?,
        pgrp: field(5)?.parse().ok()?,
        session: field(6)?.parse().ok()?,
        tty: field(7)?.parse().ok()?,
        tpgid: field(8)?.parse().ok()?,
        start: (field(22)?.parse().ok()?, 0),
        sched,
        comm: comm.to_owned(),
    })
}

#[cfg(target_os = "linux")]
fn proc_stamp(pid: u32) -> Option<ProcStamp> {
    match read_pid(pid) {
        Read::Stat(stamp) => Some(stamp),
        Read::Gone | Read::Missing | Read::Failed => None,
    }
}

/// `pid`'s short name, for a test waiting on an `exec`.
#[cfg(all(test, target_os = "linux"))]
pub(crate) fn comm_of(pid: u32) -> Option<String> {
    proc_stamp(pid).map(|found| found.comm)
}

/// `/proc/<pid>/stat`, read and parsed. A missing entry (`ENOENT`, or
/// `ESRCH` for one that vanished mid-read) is [`Read::Missing`]: a
/// `hidepid` mount hides another user's process the same way.
#[cfg(target_os = "linux")]
fn read_pid(pid: u32) -> Read {
    proc_read_starts("stat");
    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(stat) => parse_proc_stat(&stat).map_or(Read::Failed, Read::Stat),
        Err(error) if matches!(error.raw_os_error(), Some(libc::ENOENT | libc::ESRCH)) => {
            Read::Missing
        }
        Err(_) => Read::Failed,
    }
}

/// A leader read through one `/proc/<pid>` directory fd — its stamps and
/// its cwd alike — which never reaches a later process given the same
/// pid.
#[cfg(target_os = "linux")]
struct LeaderProc(std::fs::File);

#[cfg(target_os = "linux")]
impl LeaderProc {
    fn open(pid: u32) -> Option<Self> {
        proc_read_starts("open");
        std::fs::File::open(format!("/proc/{pid}")).ok().map(Self)
    }

    fn stamp(&self) -> Option<ProcStamp> {
        use std::os::fd::FromRawFd;

        proc_read_starts("stat");
        // SAFETY: `self.0` is an open directory fd for the whole call,
        // and the name is a NUL-terminated literal.
        let fd = unsafe {
            libc::openat(
                self.0.as_raw_fd(),
                c"stat".as_ptr(),
                libc::O_RDONLY | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return None;
        }
        // SAFETY: `openat` just returned this fd, and nothing else owns it.
        let file = unsafe { std::fs::File::from_raw_fd(fd) };
        parse_proc_stat(&std::io::read_to_string(file).ok()?)
    }

    fn cwd(&self) -> Option<String> {
        proc_read_starts("cwd");
        let mut buf = [0u8; libc::PATH_MAX as usize];
        // SAFETY: `buf` is writable for its full length, `self.0` is an
        // open directory fd, and the name is a NUL-terminated literal.
        let len = unsafe {
            libc::readlinkat(
                self.0.as_raw_fd(),
                c"cwd".as_ptr(),
                buf.as_mut_ptr().cast(),
                buf.len(),
            )
        };
        // A link that fills the buffer may have been cut short.
        let len = usize::try_from(len).ok().filter(|&len| len < buf.len())?;
        std::str::from_utf8(&buf[..len]).ok().map(str::to_owned)
    }
}

#[cfg(target_os = "macos")]
fn proc_stamp(pid: u32) -> Option<ProcStamp> {
    match read_pid(pid) {
        Read::Stat(stamp) if stamp.session >= 0 => Some(stamp),
        _ => None,
    }
}

/// `proc_pidinfo(PROC_PIDTBSDINFO)` and `getsid`, as one read. A pid
/// libproc has no process for (`ESRCH`) is [`Read::Gone`]; every other
/// failure is [`Read::Missing`], for `kill(pid, 0)` to settle. A zombie
/// answers its `SZOMB` status whatever `getsid` says.
#[cfg(target_os = "macos")]
fn read_pid(pid: u32) -> Read {
    use std::mem::{size_of, MaybeUninit};

    proc_read_starts("stat");
    let Ok(raw) = i32::try_from(pid) else {
        return Read::Failed;
    };
    let mut info = MaybeUninit::<libc::proc_bsdinfo>::zeroed();
    let Ok(size) = i32::try_from(size_of::<libc::proc_bsdinfo>()) else {
        return Read::Failed;
    };
    // SAFETY: `info` is a writable buffer of exactly `size` bytes for the
    // structure requested by PROC_PIDTBSDINFO, read only when libproc
    // reports that it initialized all of it.
    let written = unsafe {
        libc::proc_pidinfo(
            raw,
            libc::PROC_PIDTBSDINFO,
            0,
            info.as_mut_ptr().cast(),
            size,
        )
    };
    if written != size {
        let errno = std::io::Error::last_os_error().raw_os_error();
        // Whether a zombie answers `SZOMB` or `ESRCH` is not documented;
        // both read as gone, and this says which one it was.
        tracing::debug!(pid, written, ?errno, "proc_pidinfo answered no bsdinfo");
        return if written <= 0 && errno == Some(libc::ESRCH) {
            Read::Gone
        } else {
            Read::Missing
        };
    }
    // SAFETY: libproc initialized the full structure above.
    let info = unsafe { info.assume_init() };
    let sched = match info.pbi_status {
        libc::SZOMB => Sched::Zombie,
        libc::SSTOP => Sched::Stopped,
        _ => Sched::Running,
    };
    // `proc_bsdinfo` carries no session id.
    // SAFETY: `getsid` reads nothing but the pid it is given.
    let session = unsafe { libc::getsid(raw) };
    if session < 0 && sched != Sched::Zombie {
        return match std::io::Error::last_os_error().raw_os_error() {
            Some(libc::ESRCH) => Read::Gone,
            _ => Read::Failed,
        };
    }
    // A name that fills its array has no NUL.
    let name = |chars: &[libc::c_char]| {
        let bytes: Vec<u8> = chars
            .iter()
            .take_while(|&&char| char != 0)
            .map(|&char| char as u8)
            .collect();
        String::from_utf8_lossy(&bytes).into_owned()
    };
    let long = name(&info.pbi_name);
    Read::Stat(ProcStamp {
        pid: i64::from(info.pbi_pid),
        pgrp: i64::from(info.pbi_pgid),
        session: i64::from(session),
        tty: i64::from(info.e_tdev),
        tpgid: i64::from(info.e_tpgid),
        start: (info.pbi_start_tvsec, info.pbi_start_tvusec),
        sched,
        comm: if long.is_empty() {
            name(&info.pbi_comm)
        } else {
            long
        },
    })
}

/// A leader read by pid. Nothing pins the pid; the stamps either side of
/// its cwd read are what tell a reused one apart.
#[cfg(target_os = "macos")]
struct LeaderProc(u32);

#[cfg(target_os = "macos")]
impl LeaderProc {
    fn open(pid: u32) -> Option<Self> {
        Some(Self(pid))
    }

    fn stamp(&self) -> Option<ProcStamp> {
        proc_stamp(self.0)
    }

    fn cwd(&self) -> Option<String> {
        cwd_of_pid(self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    use std::os::unix::process::CommandExt;
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    use std::time::{Duration, Instant};

    fn leader(child_sid: i64, child_tty: i64) -> ProcStamp {
        ProcStamp {
            pid: 700,
            pgrp: 700,
            session: child_sid,
            tty: child_tty,
            tpgid: 700,
            start: (1_700_000_000, 250),
            sched: Sched::Running,
            comm: "claude".into(),
        }
    }

    fn stamp_for(found: &ProcStamp) -> Stamp {
        Stamp {
            pid: found.pid,
            start: found.start,
            pgid: found.pgrp,
            sid: found.session,
        }
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn cwd_of_pid_reads_current_process() {
        let got = cwd_of_pid(std::process::id()).expect("own cwd via platform process API");
        assert_eq!(
            std::path::Path::new(&got).canonicalize().unwrap(),
            std::env::current_dir().unwrap().canonicalize().unwrap()
        );
    }

    #[test]
    fn same_leader_refuses_another_start_session_or_terminal() {
        let (child_sid, child_tty) = (500, 34816);
        let leader = leader(child_sid, child_tty);
        assert!(
            same_leader(&leader, &leader, child_sid, child_tty),
            "the precondition: a leader read twice unchanged is kept"
        );

        let restarted = ProcStamp {
            start: (1_700_000_000, 251),
            ..leader.clone()
        };
        assert!(
            !same_leader(&leader, &restarted, child_sid, child_tty),
            "a different start time is a reused pid"
        );
        for (what, stamp) in [
            (
                "a different session",
                ProcStamp {
                    session: child_sid + 1,
                    ..leader.clone()
                },
            ),
            (
                "a different terminal",
                ProcStamp {
                    tty: child_tty + 1,
                    ..leader.clone()
                },
            ),
            (
                "not its group's leader",
                ProcStamp {
                    pgrp: 701,
                    ..leader.clone()
                },
            ),
        ] {
            assert!(!same_leader(&stamp, &stamp, child_sid, child_tty), "{what}");
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_proc_stat_line_is_read_after_its_last_paren() {
        let stat = "4242 (a) b (c)) S 4000 4242 4000 34817 4242 4194560 1 2 3 4 5 6 7 8 20 0 1 0 \
                    987654 1000 200";
        assert_eq!(
            parse_proc_stat(stat),
            Some(ProcStamp {
                pid: 4242,
                pgrp: 4242,
                session: 4000,
                tty: 34817,
                tpgid: 4242,
                start: (987_654, 0),
                sched: Sched::Running,
                comm: "a) b (c)".into(),
            })
        );
    }

    /// U7: each run state `/proc` reports, judged against a stamp taken
    /// from the same line.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_proc_stat_state_reads_as_its_liveness() {
        let line = |state: &str, start: u64, session: i64| {
            format!(
                "700 (claude) {state} 500 700 {session} 34817 700 4194560 1 2 3 4 5 6 7 8 20 0 1 0 \
                 {start} 1000 200"
            )
        };
        let stamp = Stamp {
            pid: 700,
            start: (12_345, 0),
            pgid: 700,
            sid: 500,
        };
        let judged = |stat: String| {
            let read = parse_proc_stat(&stat).map_or(Read::Failed, Read::Stat);
            classify(&stamp, read, || panic!("a read stat needs no kill"))
        };
        for (state, want) in [
            ("R", Liveness::Running),
            ("S", Liveness::Running),
            ("D", Liveness::Running),
            ("t", Liveness::Running),
            ("T", Liveness::Stopped),
            ("Z", Liveness::Gone),
            ("X", Liveness::Gone),
        ] {
            assert_eq!(judged(line(state, 12_345, 500)), want, "state {state}");
        }
        assert_eq!(
            judged(line("S", 12_346, 500)),
            Liveness::Gone,
            "another start is another process"
        );
        assert_eq!(
            judged(line("S", 12_345, 501)),
            Liveness::Gone,
            "a process that left the tab's session is not the tab's"
        );
        assert_eq!(
            judged("700 (claude) S".into()),
            Liveness::Unknown,
            "a line that does not parse says nothing"
        );
    }

    /// U8: a read that found nothing asks `kill(pid, 0)`, and only `ESRCH`
    /// means gone: a `hidepid` mount answers `ENOENT` for another user's
    /// live process, and `kill` then says `EPERM`.
    #[test]
    fn a_missing_pid_is_gone_only_when_kill_says_no_such_process() {
        let stamp = stamp_for(&leader(500, 34816));
        let missing = |errno: i32| classify(&stamp, Read::Missing, || errno);
        assert_eq!(missing(libc::ESRCH), Liveness::Gone);
        assert_eq!(missing(libc::EPERM), Liveness::Unknown);
        assert_eq!(missing(0), Liveness::Unknown);
        assert_eq!(
            classify(&stamp, Read::Gone, || panic!(
                "a platform's gone needs no kill"
            )),
            Liveness::Gone
        );
        assert_eq!(
            classify(&stamp, Read::Failed, || panic!(
                "a failed read asks nothing"
            )),
            Liveness::Unknown
        );
    }

    #[test]
    fn an_interactive_shell_is_a_shell_and_a_command_is_not() {
        for argv in [
            &["/bin/bash"][..],
            &["bash", "--posix", "-l"],
            &["/bin/bash", "--norc", "--noprofile"],
            &["bash", "--norc", "--noprofile", "-i"],
            &["/usr/bin/zsh", "-l"],
            &["-zsh"],
            &["fish", "--login"],
            &["/bin/sh"],
            &["dash", "-i"],
        ] {
            assert!(runs_a_shell(argv), "{argv:?}");
        }
        for argv in [
            &[][..],
            &["/bin/sh", "-c", "exec sleep 300"],
            &["bash", "-lc", "claude"],
            &["bash", "script.sh"],
            &["fish", "--command=ls"],
            &["fish", "--command", "ls"],
            &["bash", "--rcfile", "x"],
            &["claude"],
            &["ssh", "host"],
            &["sleep", "30"],
        ] {
            assert!(!runs_a_shell(argv), "{argv:?}");
        }
    }

    #[test]
    fn only_a_shell_child_in_front_reads_as_the_shell() {
        assert_eq!(foreground_of(Some(500), 500, true), Foreground::Shell);
        assert_eq!(
            foreground_of(Some(500), 500, false),
            Foreground::Group(500),
            "a command tab's own program is in front, not a shell"
        );
        assert_eq!(foreground_of(Some(700), 500, true), Foreground::Group(700));
        assert_eq!(foreground_of(None, 500, true), Foreground::Unknown);
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn this_process_stamps_as_itself() {
        let stamp = proc_stamp(std::process::id()).expect("our own stamp");
        // SAFETY: plain queries of this process's own ids.
        let (pgrp, session) = unsafe { (libc::getpgrp(), libc::getsid(0)) };
        assert_eq!(
            (stamp.pid, stamp.pgrp, stamp.session),
            (
                i64::from(std::process::id()),
                i64::from(pgrp),
                i64::from(session)
            )
        );
    }

    /// A child of this test, killed and reaped whatever the test does.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    struct Child(std::process::Child);

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    impl Child {
        /// `script` under `sh -c`, leading a group of its own, with its
        /// stdin and stdout piped to the test.
        fn sh(script: &str) -> Self {
            Self::in_group(script, 0)
        }

        fn in_group(script: &str, group: i32) -> Self {
            Self(
                std::process::Command::new("/bin/sh")
                    .args(["-c", script])
                    .stdin(std::process::Stdio::piped())
                    .stdout(std::process::Stdio::piped())
                    .process_group(group)
                    .spawn()
                    .expect("spawn sh"),
            )
        }

        fn pid(&self) -> u32 {
            self.0.id()
        }

        fn stamp(&self) -> Stamp {
            stamp_for(&proc_stamp(self.pid()).expect("read the child"))
        }

        /// The first line the child prints.
        fn line(&mut self) -> String {
            use std::io::BufRead;
            let mut line = String::new();
            std::io::BufReader::new(self.0.stdout.as_mut().unwrap())
                .read_line(&mut line)
                .expect("read the child's line");
            line.trim().to_owned()
        }

        fn signal(&self, signal: i32) {
            signal_pid(self.pid(), signal);
        }

        fn reap(&mut self) {
            self.0.wait().expect("reap");
        }
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    impl Drop for Child {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn signal_pid(pid: u32, signal: i32) {
        // SAFETY: a signal to a pid this test spawned and still owns.
        assert_eq!(unsafe { libc::kill(pid as libc::pid_t, signal) }, 0);
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn wait_until(what: &str, mut done: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !done() {
            assert!(Instant::now() < deadline, "timed out waiting until {what}");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// P1: an exec shim keeps its stamp. `exec` changes the name and the
    /// program, not the process: the pid, the start time and the session
    /// the stamp is made of all survive it.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn an_exec_shim_keeps_its_stamp() {
        use std::io::Write;

        let mut shim = Child::sh("read go; exec sleep 30");
        wait_until("the shim waits for its line", || {
            proc_stamp(shim.pid()).is_some_and(|found| found.comm == "sh")
        });
        let stamp = shim.stamp();
        writeln!(shim.0.stdin.as_mut().unwrap(), "go").unwrap();
        wait_until("the shim became sleep", || {
            proc_stamp(shim.pid()).is_some_and(|found| found.comm == "sleep")
        });
        assert_eq!(liveness(&stamp), Liveness::Running);
    }

    /// P2: a wrapper that waits for its agent ends with it: gone while it
    /// is a zombie, and gone once reaped.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn a_waiting_wrapper_whose_agent_is_killed_is_gone_once_it_exits() {
        let mut wrapper = Child::sh("sleep 30 & echo $!; wait; exit 0");
        let agent: u32 = wrapper.line().parse().expect("the agent's pid");
        let stamp = wrapper.stamp();
        assert_eq!(liveness(&stamp), Liveness::Running, "the precondition");

        signal_pid(agent, libc::SIGKILL);
        wait_until("the unreaped wrapper reads as gone", || {
            liveness(&stamp) == Liveness::Gone
        });
        wrapper.reap();
        assert_eq!(liveness(&stamp), Liveness::Gone, "reaped");
    }

    /// P3: a wrapper that outlives its agent is still running: the stamp
    /// follows the leader, not what it ran.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn a_wrapper_that_outlives_its_agent_is_running() {
        let mut wrapper = Child::sh("sleep 30 & echo $!; wait; read never");
        let agent: u32 = wrapper.line().parse().expect("the agent's pid");
        let stamp = wrapper.stamp();
        let agent_stamp = stamp_for(&proc_stamp(agent).expect("read the agent"));

        signal_pid(agent, libc::SIGKILL);
        wait_until("the agent is gone", || {
            liveness(&agent_stamp) == Liveness::Gone
        });
        assert_eq!(liveness(&stamp), Liveness::Running);
    }

    /// P4: job control's stop and continue.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn a_stopped_process_reads_as_stopped_until_it_continues() {
        let agent = Child::sh("exec sleep 30");
        let stamp = agent.stamp();

        agent.signal(libc::SIGSTOP);
        wait_until("it reads as stopped", || {
            liveness(&stamp) == Liveness::Stopped
        });
        agent.signal(libc::SIGCONT);
        wait_until("it reads as running", || {
            liveness(&stamp) == Liveness::Running
        });
    }

    /// P5: a killed leader nobody has reaped yet is gone, while a mate in
    /// its group runs on.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn a_killed_unreaped_leader_is_gone_while_its_group_mate_runs() {
        let leader = Child::sh("exec sleep 30");
        let mate = Child::in_group("exec sleep 30", leader.pid() as i32);
        let (leader_stamp, mate_stamp) = (leader.stamp(), mate.stamp());
        assert_eq!(mate_stamp.pgid, leader_stamp.pgid, "the precondition");

        leader.signal(libc::SIGKILL);
        wait_until("the zombie reads as gone", || {
            liveness(&leader_stamp) == Liveness::Gone
        });
        assert_eq!(liveness(&mate_stamp), Liveness::Running);
    }

    /// P6: a stamp whose start time is not the process's is another
    /// process that happens to have the pid.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn a_forged_start_time_reads_as_gone() {
        let agent = Child::sh("exec sleep 30");
        let mut stamp = agent.stamp();
        assert_eq!(liveness(&stamp), Liveness::Running, "the precondition");
        stamp.start.0 += 1;
        assert_eq!(liveness(&stamp), Liveness::Gone);
    }

    /// P7: a process we may not signal is unknown, never gone, through the
    /// seam a hidden `/proc` entry takes. pid 1 is init or launchd.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn a_process_we_may_not_signal_is_unknown_never_gone() {
        let read = read_pid(1);
        eprintln!("pid 1 read as {read:?}");
        let stamp = match &read {
            Read::Stat(found) => stamp_for(found),
            _ => Stamp {
                pid: 1,
                start: (0, 0),
                pgid: 1,
                sid: 1,
            },
        };
        assert_eq!(
            classify(&stamp, Read::Missing, || kill_errno(1)),
            Liveness::Unknown,
            "kill(1, 0) is EPERM for a user, success for root: neither is gone"
        );
        assert_ne!(liveness(&stamp), Liveness::Gone);
    }
}
