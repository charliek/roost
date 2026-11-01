//! What the box can say about its pty supply when a spawn fails (#444,
//! #628): the errno alone cannot tell an exhausted pool from a transient
//! refusal. Test-only, and shared by source: the lib's tests reach it as
//! `crate::pty_census`, `tests/pty_shutdown_test.rs` through `#[path]`.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::io::Read as _;
use std::process::{Command, Stdio};
use std::time::Duration;

pub(crate) fn rlimit_nofile() -> Result<libc::rlimit, String> {
    let mut limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: `getrlimit` writes one `rlimit`, which is what it is given.
    let rc = unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) };
    if rc == 0 {
        Ok(limit)
    } else {
        Err(format!("getrlimit: {}", std::io::Error::last_os_error()))
    }
}

/// What the box could say about its pty supply at the moment a spawn
/// failed. Every field is fallible *into the message*: a panic raised
/// while collecting would replace the `openpty` errno — the one thing
/// that pins which ceiling was hit — with its own.
pub(crate) struct PtyCensus {
    /// Tabs the calling test already had live, `None` where the caller
    /// does not track it.
    pub(crate) tabs_live: Option<usize>,
    pub(crate) fd_limit: Result<String, String>,
    pub(crate) open_fds: Result<String, String>,
    /// What this platform can say about its pty pool, each row labelled
    /// for exactly what it counts.
    pub(crate) pool: Vec<(&'static str, Result<String, String>)>,
    pub(crate) holders: Result<String, String>,
}

impl PtyCensus {
    pub(crate) fn render(&self) -> String {
        let mut out = String::from("pty census:\n");
        let tabs = self
            .tabs_live
            .map_or_else(|| "unknown".to_string(), |count| count.to_string());
        let _ = writeln!(out, "  tabs live in this test: {tabs}");
        let _ = writeln!(out, "  RLIMIT_NOFILE: {}", shown(&self.fd_limit));
        let _ = writeln!(
            out,
            "  open fds (/dev/fd entries): {}",
            shown(&self.open_fds)
        );
        for (label, value) in &self.pool {
            let _ = writeln!(out, "  {label}: {}", shown(value));
        }
        let _ = writeln!(out, "  candidate holders (lsof): {}", shown(&self.holders));
        out
    }
}

fn shown(value: &Result<String, String>) -> String {
    match value {
        Ok(value) => value.clone(),
        Err(why) => format!("unavailable: {why}"),
    }
}

pub(crate) fn pty_census(tabs_live: Option<usize>) -> PtyCensus {
    PtyCensus {
        tabs_live,
        fd_limit: rlimit_nofile()
            .map(|limit| format!("soft {} hard {}", limit.rlim_cur, limit.rlim_max)),
        open_fds: count_dir_entries("/dev/fd", |_| true).map(|count| count.to_string()),
        pool: pty_pool(),
        holders: pty_holders(),
    }
}

pub(crate) fn count_dir_entries(path: &str, keep: impl Fn(&str) -> bool) -> Result<usize, String> {
    std::fs::read_dir(path)
        .map_err(|err| format!("{path}: {err}"))?
        .try_fold(0usize, |count, entry| {
            let name = entry.map_err(|err| format!("{path}: {err}"))?.file_name();
            Ok(count + usize::from(keep(&name.to_string_lossy())))
        })
}

#[cfg(target_os = "macos")]
fn pty_pool() -> Vec<(&'static str, Result<String, String>)> {
    // The node count tracks allocations on a Mac but is not the same
    // thing, so the label says nodes and the message never claims more.
    vec![
        (
            "/dev/ttys* device nodes",
            count_dir_entries("/dev", |name| name.starts_with("ttys"))
                .map(|count| count.to_string()),
        ),
        (
            "kern.tty.ptmx_max",
            sysctl_int("kern.tty.ptmx_max").map(|max| max.to_string()),
        ),
    ]
}

#[cfg(not(target_os = "macos"))]
fn pty_pool() -> Vec<(&'static str, Result<String, String>)> {
    vec![
        (
            "/proc/sys/kernel/pty/nr",
            read_trimmed("/proc/sys/kernel/pty/nr"),
        ),
        (
            "/proc/sys/kernel/pty/max",
            read_trimmed("/proc/sys/kernel/pty/max"),
        ),
        (
            "/dev/pts entries",
            count_dir_entries("/dev/pts", |_| true).map(|count| count.to_string()),
        ),
    ]
}

#[cfg(not(target_os = "macos"))]
fn read_trimmed(path: &str) -> Result<String, String> {
    std::fs::read_to_string(path)
        .map(|text| text.trim().to_string())
        .map_err(|err| format!("{path}: {err}"))
}

#[cfg(target_os = "macos")]
fn sysctl_int(name: &str) -> Result<libc::c_int, String> {
    let cname = std::ffi::CString::new(name).map_err(|err| err.to_string())?;
    let mut value: libc::c_int = 0;
    let mut size = std::mem::size_of::<libc::c_int>();
    // SAFETY: `sysctlbyname` writes at most `size` bytes into `value`,
    // and `size` is exactly one `c_int`.
    let rc = unsafe {
        libc::sysctlbyname(
            cname.as_ptr(),
            std::ptr::from_mut(&mut value).cast(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    if rc == 0 {
        Ok(value)
    } else {
        Err(format!("{name}: {}", std::io::Error::last_os_error()))
    }
}

/// macOS keeps `lsof` in `/usr/sbin`, which is not always on a test
/// process's `PATH`.
#[cfg(target_os = "macos")]
pub(crate) const LSOF: &str = "/usr/sbin/lsof";
#[cfg(not(target_os = "macos"))]
pub(crate) const LSOF: &str = "lsof";
/// Short on purpose: this runs on the failure path, where an exhausted
/// pool has to report in seconds rather than after the test's children
/// die. A truncated holder list beats a census that costs more than the
/// failure it explains.
const LSOF_BUDGET: Duration = Duration::from_secs(5);
const HOLDER_CAP: usize = 40;
/// The devices a pty holder is named by. The parser filters `lsof`'s name
/// records on these.
const PTY_DEVICES: [&str; 3] = ["/dev/ttys", "/dev/ptmx", "/dev/pts"];

#[cfg(target_os = "macos")]
fn lsof_target_paths() -> Vec<String> {
    let mut paths = vec!["/dev/ptmx".to_string()];
    // `+d /dev` matches nothing on macOS, so the pty device nodes have
    // to be named explicitly.
    if let Ok(entries) = std::fs::read_dir("/dev") {
        paths.extend(entries.flatten().filter_map(|entry| {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            name.starts_with("ttys").then(|| format!("/dev/{name}"))
        }));
    }
    paths
}

#[cfg(target_os = "macos")]
fn lsof_command() -> Command {
    let mut cmd = Command::new(LSOF);
    cmd.args(["-n", "-w", "-F", "pcfn"])
        .args(lsof_target_paths());
    cmd
}

#[cfg(not(target_os = "macos"))]
fn lsof_command() -> Command {
    let mut cmd = Command::new(LSOF);
    cmd.args(["-n", "-w", "-F", "pcfn", "+d", "/dev/pts", "/dev/ptmx"]);
    cmd
}

/// Who else holds a pty descriptor, best effort. `lsof` is the only tool
/// that enumerates descriptor holders — `ps`'s controlling-tty column
/// names processes attached to a tty, which is a different set and not
/// the one that keeps a macOS pty slot alive. It is not installed
/// everywhere and can be slow on a busy box, so it is bounded and its
/// absence is just another unavailable line.
fn pty_holders() -> Result<String, String> {
    lsof_field_output().map(|bytes| render_holders(&bytes))
}

/// `lsof -F pcfn` over this platform's pty devices, bounded by
/// `LSOF_BUDGET`.
fn lsof_field_output() -> Result<Vec<u8>, String> {
    let mut child = lsof_command()
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|err| format!("{LSOF}: {err}"))?;
    let mut stdout = match child.stdout.take() {
        Some(stdout) => stdout,
        None => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!("{LSOF}: stdout was not piped"));
        }
    };
    let (tx, rx) = std::sync::mpsc::channel();
    // `lsof`'s output outgrows a pipe buffer, so it has to be drained
    // while it runs or it would block forever on a full pipe and only
    // ever be killed at the budget. `Builder::spawn` rather than
    // `thread::spawn`, whose panic when the OS refuses a thread would
    // replace the errno this census exists to report.
    let drain = std::thread::Builder::new()
        .name("pty-census-lsof".to_string())
        .spawn(move || {
            let mut bytes = Vec::new();
            let _ = tx.send(stdout.read_to_end(&mut bytes).map(|_| bytes));
        });
    let drain = match drain {
        Ok(drain) => drain,
        Err(err) => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!("{LSOF} drain thread: {err}"));
        }
    };
    let outcome = match rx.recv_timeout(LSOF_BUDGET) {
        // stdout is at EOF, so the child is done writing and `wait`
        // returns promptly.
        Ok(Ok(bytes)) => child
            .wait()
            .map(|_| bytes)
            .map_err(|err| format!("{LSOF}: {err}")),
        Ok(Err(err)) => {
            let _ = child.kill();
            let _ = child.wait();
            Err(format!("{LSOF}: {err}"))
        }
        // `Child::kill` signals through the handle, so — unlike a saved
        // raw pid — it cannot reach a pid the child was already reaped out
        // of and the OS recycled.
        Err(_) => {
            let _ = child.kill();
            let _ = child.wait();
            Err(format!("{LSOF} did not answer within {LSOF_BUDGET:?}"))
        }
    };
    // The drain ends when stdout EOFs, which the kill above guarantees. A
    // join error is one more thing the census must not panic on.
    let _ = drain.join();
    outcome
}

/// Field mode (`-F pcfn`) is parsed instead of columns: Linux lsof
/// inserts a TASKCMD column for threaded processes that shifts FD out of a
/// fixed position, but tagged records (`p`/`c`/`f`/`n`) are immune to
/// that. A many-threaded process can still repeat the same descriptor once
/// per thread, so tally each holder's distinct fds, not lines.
pub(crate) fn render_holders(bytes: &[u8]) -> String {
    let mut by_holder: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let stdout = String::from_utf8_lossy(bytes);
    let mut pid: Option<&str> = None;
    let mut command: Option<&str> = None;
    let mut fd: Option<&str> = None;
    for line in stdout.lines() {
        // The tag comes from the bytes and the value from a checked
        // slice: a field value may itself contain a newline, so a line
        // can start with a multi-byte character and byte 1 need not be a
        // char boundary. Slicing one would panic inside the census and
        // replace the errno it exists to report.
        let (Some(&tag), Some(rest)) = (line.as_bytes().first(), line.get(1..)) else {
            continue;
        };
        match tag {
            // A process record starts over: carrying the previous one's
            // command or descriptor into it would attribute this pid's
            // ptys to the last process seen.
            b'p' => {
                pid = Some(rest);
                command = None;
                fd = None;
            }
            b'c' => command = Some(rest),
            b'f' => fd = Some(rest),
            // One name per descriptor: consume the fd so a second name
            // line cannot record the same descriptor again.
            b'n' => {
                let (Some(pid), Some(command), Some(fd)) = (pid, command, fd.take()) else {
                    continue;
                };
                if PTY_DEVICES.iter().any(|device| rest.contains(device)) {
                    by_holder
                        .entry(format!("{command}({pid})"))
                        .or_default()
                        .insert(fd.to_string());
                }
            }
            _ => {}
        }
    }
    if by_holder.is_empty() {
        return "none".to_string();
    }
    let total = by_holder.len();
    // Biggest first: a box that ran out of ptys ran out because of
    // whoever is at the top, and the cap must not spend its lines on an
    // alphabetically lucky tail.
    let mut holders: Vec<(String, usize)> = by_holder
        .into_iter()
        .map(|(holder, fds)| (holder, fds.len()))
        .collect();
    holders.sort_by(|left, right| right.1.cmp(&left.1).then_with(|| left.0.cmp(&right.0)));
    let mut lines: Vec<String> = holders
        .into_iter()
        .take(HOLDER_CAP)
        .map(|(holder, fds)| format!("{holder}: {fds} fds"))
        .collect();
    if total > HOLDER_CAP {
        lines.push(format!("(+{} more)", total - HOLDER_CAP));
    }
    lines.join("\n    ")
}
