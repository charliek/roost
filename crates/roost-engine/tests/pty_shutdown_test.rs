//! `PtySupervisor::shutdown_all`: tear every PTY down, account for every
//! tab, and never outlive the deadline by more than the SIGKILL tail.
//!
//! The report is keyed on the session map — the per-spawn wait task
//! removes a tab's entry when `child.wait()` returns — rather than on the
//! lifecycle broadcast, whose capacity is 64. `every_tab_is_accounted_for_
//! past_the_lifecycle_channel_capacity` is the test that pins the
//! difference: with more simultaneous exits than the channel holds, a
//! report built from events would lose tabs, and one built from the map
//! cannot.
//!
//! Children are `exec`'d so the PTY's direct child is the only process
//! holding the slave fd. A surviving descendant would keep the reader
//! task parked in `poll()`, and dropping the tokio runtime waits on
//! in-flight blocking tasks — the test would hang instead of failing.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::io::Read as _;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use roost_engine::{PtyError, PtyOutputEvent, PtySupervisor, ShutdownReport};
use tokio::sync::broadcast::{self, error::TryRecvError};
use tokio::time::sleep;

/// A shell that dies on SIGHUP, as one process (no descendant to hold
/// the PTY open).
const COOPERATIVE: &str = "exec sleep 100";
/// A shell that ignores SIGHUP and stays ignoring it across `exec`
/// (POSIX: an ignored disposition survives the new process image), so
/// only SIGKILL ends it. `printf` first so the test can wait for the
/// trap to be installed before hanging the tab up — a SIGHUP that
/// arrives during startup would kill it and make the tab look
/// cooperative.
const SIGHUP_IMMUNE: &str = "trap '' HUP; printf R; exec sleep 100";

fn socket() -> std::path::PathBuf {
    std::path::PathBuf::from("/tmp/roost-pty-shutdown.sock")
}

/// Every live tab holds four master-side descriptors in this process —
/// the master, a reader dup, a writer dup and the EOF-on-drop writer — so
/// the 70 simultaneous tabs of the heaviest test plus the racers and a
/// baseline need ~350. A macOS login session's default soft limit is 256.
const FD_BUDGET_NEEDED: libc::rlim_t = 350;
/// Comfortably past what the tests need and far below every platform's
/// per-process ceiling (`kern.maxfilesperproc` on macOS, 10240 there).
const FD_BUDGET_TARGET: libc::rlim_t = 1024;

/// The two PTY-heavy tests must not overlap: run together they ask for
/// ~140 pty pairs at once, against a system-wide macOS pool
/// (`kern.tty.ptmx_max`, 511 on a CI runner) shared with everything else
/// on the box. Serialised, this binary peaks at ~75 pairs.
///
/// Each `#[tokio::test]` builds its own runtime, and one static tokio
/// mutex shared across those runtimes is sound because its wakers are
/// `Send`: whichever runtime releases the guard may wake a waiter parked
/// on another.
///
/// The guard bounds concurrent *spawning*, not every descriptor: a torn
/// down tab's blocking reader can still hold its master dup until the
/// runtime drains it, so a brief tail of closing dups outlives the guard
/// and the peak is ~75 pairs plus that tail rather than a hard 75.
/// Bounding it exactly would mean gating on this process's fd or pty
/// count, which the binary's other tests move concurrently — a flaky gate
/// rather than a precise one, so both heavy tests settle the tail
/// best-effort (`settle_open_fds`) instead.
static PTY_HEAVY: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Best effort after a teardown: wait for this process's descriptor count
/// to stop falling, so most of the closing master dups land before the
/// `PTY_HEAVY` guard is released. Never an assertion and never fatal —
/// the binary's other tests open descriptors of their own while this runs,
/// which is exactly why the count cannot be a bound.
async fn settle_open_fds() {
    let open_fds = || count_dir_entries("/dev/fd", |_| true);
    let deadline = Instant::now() + Duration::from_secs(5);
    let Ok(mut previous) = open_fds() else {
        return;
    };
    while Instant::now() < deadline {
        sleep(Duration::from_millis(25)).await;
        let Ok(current) = open_fds() else {
            return;
        };
        if current >= previous {
            return;
        }
        previous = current;
    }
}

fn rlimit_nofile() -> Result<libc::rlimit, String> {
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

/// Raise this process's `RLIMIT_NOFILE` soft limit to what the PTY-heavy
/// tests need, rather than shrink the tests: 65+ simultaneous live tabs
/// is the property under test. Raise-only and idempotent — two tests
/// calling it concurrently settle on the same value, so it needs no lock.
fn ensure_fd_budget() {
    let current = rlimit_nofile().unwrap_or_else(|err| panic!("{err}"));
    assert!(
        current.rlim_max >= FD_BUDGET_NEEDED,
        "RLIMIT_NOFILE hard limit is {} (soft {}), below the {FD_BUDGET_NEEDED} \
         descriptors this test needs",
        current.rlim_max,
        current.rlim_cur
    );
    let target = FD_BUDGET_TARGET.min(current.rlim_max);
    if current.rlim_cur >= target {
        return;
    }
    let raised = libc::rlimit {
        rlim_cur: target,
        rlim_max: current.rlim_max,
    };
    // SAFETY: `setrlimit` reads one `rlimit`, which is what it is given.
    if unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &raised) } != 0 {
        let err = std::io::Error::last_os_error();
        assert!(
            current.rlim_cur >= FD_BUDGET_NEEDED,
            "raising the RLIMIT_NOFILE soft limit from {} to {target} failed: {err} \
             (hard {})",
            current.rlim_cur,
            current.rlim_max
        );
    }
}

/// What the box could say about its pty supply at the moment a spawn
/// failed. Every field is fallible *into the message*: a panic raised
/// while collecting would replace the `openpty` errno — the one thing
/// that pins which ceiling was hit — with its own.
struct PtyCensus {
    /// Tabs the calling test already had live, `None` where the caller
    /// does not track it.
    tabs_live: Option<usize>,
    fd_limit: Result<String, String>,
    open_fds: Result<String, String>,
    /// What this platform can say about its pty pool, each row labelled
    /// for exactly what it counts.
    pool: Vec<(&'static str, Result<String, String>)>,
    holders: Result<String, String>,
}

impl PtyCensus {
    fn render(&self) -> String {
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

fn pty_census(tabs_live: Option<usize>) -> PtyCensus {
    PtyCensus {
        tabs_live,
        fd_limit: rlimit_nofile()
            .map(|limit| format!("soft {} hard {}", limit.rlim_cur, limit.rlim_max)),
        open_fds: count_dir_entries("/dev/fd", |_| true).map(|count| count.to_string()),
        pool: pty_pool(),
        holders: pty_holders(),
    }
}

fn count_dir_entries(path: &str, keep: impl Fn(&str) -> bool) -> Result<usize, String> {
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
const LSOF: &str = "/usr/sbin/lsof";
#[cfg(not(target_os = "macos"))]
const LSOF: &str = "lsof";
/// Short on purpose: this runs on the failure path, after a 5s teardown,
/// and the whole point of that teardown is that an exhausted pool reports
/// in seconds rather than after the `sleep 100` children die. A truncated
/// holder list beats a census that costs more than the failure it explains.
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
fn render_holders(bytes: &[u8]) -> String {
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

/// A spawn failure carries the errno *and* the census, so a PTY that
/// cannot be allocated names the ceiling it hit and who else was holding
/// one.
fn try_spawn_tab(
    sup: &PtySupervisor,
    tab_id: i64,
    script: &str,
    tabs_live: Option<usize>,
) -> Result<broadcast::Receiver<PtyOutputEvent>, String> {
    sup.spawn(
        tab_id,
        "/tmp",
        &["/bin/sh".into(), "-c".into(), script.into()],
        80,
        24,
        &socket(),
    )
    .map_err(|err| {
        // `{err:#}` walks the anyhow chain: the errno is the whole point
        // of the message, and the outermost context alone drops it.
        format!(
            "spawn of tab {tab_id} failed: {err:#}\n{}",
            pty_census(tabs_live).render()
        )
    })
}

fn spawn_tab(
    sup: &PtySupervisor,
    tab_id: i64,
    script: &str,
) -> broadcast::Receiver<PtyOutputEvent> {
    try_spawn_tab(sup, tab_id, script, None).unwrap_or_else(|err| panic!("{err}"))
}

/// Wait for the child's readiness byte, so the shutdown that follows
/// cannot race the shell's own startup.
async fn wait_ready(rx: &mut broadcast::Receiver<PtyOutputEvent>, tab_id: i64) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        match rx.try_recv() {
            Ok(PtyOutputEvent::Bytes { data, .. }) if data.contains(&b'R') => return,
            Ok(_) | Err(TryRecvError::Lagged(_)) => {}
            Err(TryRecvError::Closed) => break,
            Err(TryRecvError::Empty) => sleep(Duration::from_millis(5)).await,
        }
    }
    panic!("tab {tab_id} never signalled readiness");
}

/// The report's three vectors must partition the ids that were live when
/// shutdown started: every id exactly once, none invented, none dropped.
/// A duplicate shows up as a length mismatch against the target list.
fn assert_partitions(report: &ShutdownReport, targets: &[i64]) {
    let mut seen: Vec<i64> = report
        .reaped
        .iter()
        .chain(&report.killed)
        .chain(&report.abandoned)
        .copied()
        .collect();
    seen.sort_unstable();
    let mut expected = targets.to_vec();
    expected.sort_unstable();
    assert_eq!(
        seen, expected,
        "every live tab must appear exactly once in {report:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cooperative_children_are_all_reaped() {
    let sup = PtySupervisor::new();
    let targets: Vec<i64> = (100..104).collect();
    let _rx: Vec<_> = targets
        .iter()
        .map(|id| spawn_tab(&sup, *id, COOPERATIVE))
        .collect();

    let report = sup.shutdown_all(Duration::from_secs(10)).await;

    assert_eq!(report.reaped, targets, "SIGHUP alone should suffice");
    assert!(
        report.killed.is_empty(),
        "no SIGKILL was needed: {report:?}"
    );
    assert!(report.abandoned.is_empty(), "{report:?}");
    assert_partitions(&report, &targets);
    for id in &targets {
        assert!(!sup.has(*id), "tab {id} outlived shutdown");
    }
}

/// A child that ignores SIGHUP is still gone when shutdown returns, and
/// the report says how: `killed`, disjoint from `reaped`.
///
/// The deadline is deliberately shorter than the per-tab `KILL_GRACE`
/// watchdog (200ms) so the escalation under test is the one
/// `shutdown_all` performs, not the one `terminate_child` already spawns.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_sighup_immune_child_is_sigkilled_and_reported_killed() {
    let sup = PtySupervisor::new();
    let mut rx = spawn_tab(&sup, 200, SIGHUP_IMMUNE);
    wait_ready(&mut rx, 200).await;

    let report = sup.shutdown_all(Duration::from_millis(50)).await;

    assert_eq!(report.killed, vec![200], "{report:?}");
    assert!(
        report.reaped.is_empty(),
        "killed and reaped must be disjoint: {report:?}"
    );
    assert!(
        report.abandoned.is_empty(),
        "SIGKILL should have landed well inside the tail: {report:?}"
    );
    assert_partitions(&report, &[200]);
    assert!(!sup.has(200));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_empty_supervisor_reports_nothing_immediately() {
    let sup = PtySupervisor::new();
    let started = Instant::now();

    let report = sup.shutdown_all(Duration::from_secs(30)).await;

    assert_eq!(report, ShutdownReport::default());
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "nothing to wait for, yet shutdown took {:?}",
        started.elapsed()
    );
}

/// Deadline semantics: shutdown waits out the full soft deadline for a
/// child that will not exit, then escalates and returns inside the
/// SIGKILL tail. It must neither cut the cooperative window short nor
/// wait unbounded on a child that ignores the hangup.
///
/// Deadline under `KILL_GRACE` again, for the same reason as the test
/// above: past 200ms the per-tab watchdog would do the killing and the
/// tab would report as `reaped`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shutdown_returns_between_the_deadline_and_the_kill_tail() {
    let sup = PtySupervisor::new();
    let mut rx = spawn_tab(&sup, 300, SIGHUP_IMMUNE);
    wait_ready(&mut rx, 300).await;

    let deadline = Duration::from_millis(100);
    let started = Instant::now();
    let report = sup.shutdown_all(deadline).await;
    let elapsed = started.elapsed();

    assert_eq!(report.killed, vec![300], "{report:?}");
    assert!(
        elapsed >= deadline,
        "shutdown escalated before the deadline expired: {elapsed:?}"
    );
    // Deadline + the 500ms post-SIGKILL tail, with slack for a loaded
    // machine. The bound that matters is that it is bounded at all.
    assert!(
        elapsed < deadline + Duration::from_secs(5),
        "shutdown ran past its deadline plus the kill tail: {elapsed:?}"
    );
}

/// The other half of the escalation story: `killed` means *shutdown*
/// escalated. Given a deadline past the per-tab `KILL_GRACE` watchdog,
/// the same SIGHUP-immune child is force-killed by `terminate_child`'s
/// own watchdog and reported as `reaped` — the deadline never expired,
/// so shutdown never sent a signal of its own.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_generous_deadline_lets_the_per_tab_watchdog_do_the_killing() {
    let sup = PtySupervisor::new();
    let mut rx = spawn_tab(&sup, 350, SIGHUP_IMMUNE);
    wait_ready(&mut rx, 350).await;

    let report = sup.shutdown_all(Duration::from_secs(10)).await;

    assert_eq!(report.reaped, vec![350], "{report:?}");
    assert!(report.killed.is_empty(), "{report:?}");
    assert_partitions(&report, &[350]);
}

/// The lifecycle broadcast holds 64 events; this exits more tabs than
/// that at once, so any subscriber is free to lag. The report is built
/// from session-map removals, so lagging costs latency and nothing else
/// — every id must still be accounted for exactly once.
///
/// The test cannot black-box observe a `Lagged` — that is the point of
/// the design, not a gap in the test. What it pins is the consequence:
/// at this scale a report derived from the channel would lose tabs, and
/// one derived from the map cannot.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn every_tab_is_accounted_for_past_the_lifecycle_channel_capacity() {
    ensure_fd_budget();
    let _serialised = PTY_HEAVY.lock().await;
    let sup = PtySupervisor::new();
    let targets: Vec<i64> = (400..470).collect();
    let mut _rx = Vec::with_capacity(targets.len());
    for (live, id) in targets.iter().enumerate() {
        match try_spawn_tab(&sup, *id, COOPERATIVE, Some(live)) {
            Ok(receiver) => _rx.push(receiver),
            Err(failure) => {
                // Tear down before reporting: every tab already spawned
                // parks a blocking reader task on a master dup, and
                // dropping the runtime waits on those tasks while the
                // `exec sleep 100` children live — panicking here
                // directly would deliver the message ~100s later.
                sup.shutdown_all(Duration::from_secs(5)).await;
                panic!("{failure}");
            }
        }
    }

    let report = sup.shutdown_all(Duration::from_secs(30)).await;

    assert_partitions(&report, &targets);
    assert_eq!(
        report.reaped.len(),
        targets.len(),
        "every tab should have been reaped cooperatively: {report:?}"
    );
    assert!(report.abandoned.is_empty(), "{report:?}");
    for id in &targets {
        assert!(!sup.has(*id), "tab {id} outlived shutdown");
    }
    settle_open_fds().await;
}

/// The no-more-spawns latch is permanent: once shutdown has walked the
/// session map, a tab that acquired a PTY afterwards would never be torn
/// down by anyone.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn spawn_is_rejected_once_shutdown_has_started() {
    let sup = PtySupervisor::new();
    let _rx = spawn_tab(&sup, 500, COOPERATIVE);

    let report = sup.shutdown_all(Duration::from_secs(10)).await;
    assert_eq!(report.reaped, vec![500]);

    let err = sup
        .spawn(
            501,
            "/tmp",
            &["/bin/sh".into(), "-c".into(), COOPERATIVE.into()],
            80,
            24,
            &socket(),
        )
        .expect_err("spawn after shutdown must be refused");
    let pty_err = err
        .downcast_ref::<PtyError>()
        .expect("expected PtyError in anyhow chain");
    assert!(
        matches!(pty_err, PtyError::ShuttingDown(501)),
        "unexpected error: {pty_err}"
    );
    assert!(!sup.has(501), "a refused spawn must leave no session");
}

/// `close()` frees its slot synchronously — it removes the session entry
/// and lets the waiter reap in the background. That is exactly the
/// signal `shutdown_all` reads as "this child was reaped", so a close
/// racing a shutdown would report a still-running child as reaped. While
/// the latch is set, close() must send its hangup and leave the entry
/// for the waiter.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn close_during_shutdown_leaves_the_entry_for_the_waiter() {
    let sup = std::sync::Arc::new(PtySupervisor::new());
    let mut rx = spawn_tab(&sup, 900, SIGHUP_IMMUNE);
    wait_ready(&mut rx, 900).await;

    let shutdown = tokio::spawn({
        let sup = std::sync::Arc::clone(&sup);
        async move { sup.shutdown_all(Duration::from_secs(10)).await }
    });
    // The latch is set first thing in `shutdown_all`, and the child
    // ignores SIGHUP — so it cannot be reaped before the per-tab
    // watchdog fires at 200ms. The entry is still there to protect.
    sleep(Duration::from_millis(30)).await;
    sup.close(900);
    assert!(
        sup.has(900),
        "close() dropped a live child's entry mid-shutdown; shutdown would call it reaped"
    );

    let report = shutdown.await.expect("shutdown task");
    assert_partitions(&report, &[900]);
    assert!(
        !sup.has(900),
        "the waiter should have removed the entry: {report:?}"
    );
}

/// The latch alone is not enough, because a spawn can be *past* it: it
/// reserved its slot before shutdown started and is still building its
/// PTY when the sweep snapshots the session map. If it then installed
/// that session, nothing would ever tear the child down — the sweep has
/// already walked past, and the caller believes it owns a live tab.
///
/// So every racer must end in exactly one of two states: refused with
/// `ShuttingDown`, or present in exactly one bucket of the report. A
/// session that survives in the map without appearing in the report is
/// the leak.
///
/// A zero deadline is what opens the window: the wait for in-flight
/// spawns gives up before its first poll, so the sweep snapshots the map
/// with racers still inside `openpty`/`fork`. A deadline of even a few
/// milliseconds lets every spawn finish first and the race never
/// happens (verified: this test passes against the unfixed promotion
/// path when the drain is allowed to succeed).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn spawns_racing_the_sweep_never_leak_a_session() {
    ensure_fd_budget();
    let _serialised = PTY_HEAVY.lock().await;
    for round in 0..6i64 {
        let sup = std::sync::Arc::new(PtySupervisor::new());
        let base = 600 + round * 100;
        // A few tabs already live, so the sweep has real work to do and
        // shutdown does not finish before the racers get moving.
        let baseline: Vec<_> = (base..base + 3)
            .map(|id| spawn_tab(&sup, id, COOPERATIVE))
            .collect();

        let racers: Vec<_> = (base + 10..base + 18)
            .map(|id| {
                let sup = std::sync::Arc::clone(&sup);
                // `spawn` blocks on openpty + fork, so it belongs on the
                // blocking pool rather than a runtime worker.
                tokio::task::spawn_blocking(move || {
                    let result = sup.spawn(
                        id,
                        "/tmp",
                        &["/bin/sh".into(), "-c".into(), COOPERATIVE.into()],
                        80,
                        24,
                        &socket(),
                    );
                    (id, result.err().map(|err| err.to_string()))
                })
            })
            .collect();

        // Long enough for the racers to have reserved their slots and be
        // inside the PTY build, short enough that most have not promoted.
        sleep(Duration::from_millis(2)).await;
        let report = sup.shutdown_all(Duration::ZERO).await;

        for racer in racers {
            let (id, err) = racer.await.expect("racer task");
            match err {
                Some(err) => assert!(
                    err.contains("shutting down"),
                    "racer {id} failed for the wrong reason: {err}\n{}",
                    pty_census(Some(baseline.len())).render()
                ),
                None => {
                    let buckets = [&report.reaped, &report.killed, &report.abandoned]
                        .iter()
                        .filter(|bucket| bucket.contains(&id))
                        .count();
                    assert_eq!(
                        buckets, 1,
                        "racer {id} spawned successfully but shutdown never swept it: {report:?}"
                    );
                }
            }
        }
        // A tab still in the map is only acceptable if shutdown said so.
        for id in base..base + 18 {
            assert!(
                !sup.has(id) || report.abandoned.contains(&id),
                "round {round}: tab {id} outlived shutdown unreported: {report:?}"
            );
        }
        settle_open_fds().await;
    }
}

/// Whether `lsof` can be run here at all. `-v` prints its version and
/// exits, so this cannot hang and enumerates nothing: the census's own
/// bounded run is a load-sensitive measurement and must be sampled once
/// per test, never twice and compared.
fn lsof_runs_here() -> bool {
    let Ok(mut child) = Command::new(LSOF)
        .arg("-v")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    else {
        // `ErrorKind::NotFound` — no `lsof` on this box — is the case
        // that occurs; any other spawn failure is equally a tool that
        // cannot be run here.
        return false;
    };
    let _ = child.wait();
    true
}

/// The census only ever prints when a spawn has already failed, so it is
/// read on the happy path too — cross-platform collection that runs only
/// on the failure path rots silently until the one moment it matters.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_census_reads_this_platform() {
    // One live tab, so a working parser is guaranteed a holder to find
    // and "no holders" is a real failure rather than an accepted
    // outcome. One pty pair, so this test needs no `PTY_HEAVY` guard.
    let sup = PtySupervisor::new();
    let _rx = spawn_tab(&sup, 1200, COOPERATIVE);
    let census = pty_census(Some(1));
    // Assert only after the teardown: a panic with the tab still live
    // would be delivered when the `exec sleep 100` child dies, not now
    // (this file's header).
    sup.shutdown_all(Duration::from_secs(5)).await;
    let lsof_runs = lsof_runs_here();

    let fd_limit = census.fd_limit.expect("RLIMIT_NOFILE");
    assert!(fd_limit.contains("soft "), "{fd_limit}");
    let open_fds: usize = census
        .open_fds
        .expect("open fds")
        .parse()
        .expect("open fds is a count");
    assert!(open_fds > 0, "this process has descriptors open");
    assert!(!census.pool.is_empty(), "no pty pool reading on this OS");
    for (label, value) in &census.pool {
        assert!(value.is_ok(), "pool field {label}: {value:?}");
    }

    // Every way the holders reading can fail renders as "unavailable: …",
    // so the census's own text cannot say whether `lsof` is installed at
    // all — the one reading allowed to be missing outright. The probe
    // above settles that without sampling the bounded run a second time.
    let holders = match census.holders {
        Ok(holders) => holders,
        Err(why) if lsof_runs => {
            // The bounded run is best effort by design: on a loaded
            // runner it may give up, and a timeout is the *only* failure
            // this test tolerates from an installed `lsof`. A spawn
            // failure, a wait failure or a parser regression stays red.
            assert!(
                why.contains("did not answer within"),
                "{LSOF} runs here, so the census must produce a holder list — \
                 a timeout is tolerated because the probe is best-effort under \
                 load, but not this: {why}"
            );
            return;
        }
        Err(why) => {
            // `lsof` ships with macOS but is not installed on every Linux
            // box; a tool that cannot be run is the one reading allowed to
            // be missing, and nothing else is.
            if cfg!(target_os = "macos") {
                panic!("macOS ships lsof at {LSOF}, so it must run here: {why}");
            }
            assert!(
                why.contains("lsof"),
                "{LSOF} cannot be run here, which is the one reason holders may \
                 be missing — but not with this failure: {why}"
            );
            return;
        }
    };
    assert!(
        holders.contains(&format!("({})", std::process::id())),
        "this process held a pty master for a live tab while the census ran, \
         yet it is not among the holders:\n{holders}"
    );
    for line in holders.lines() {
        assert!(
            holder_line_is_shaped(line),
            "not a holder line: {line:?} in {holders:?}"
        );
    }
}

/// `render_holders`' own output shape: `command(pid): N fds`, or the
/// over-cap tail. Anything else means the parser produced something the
/// reader of a failed spawn cannot act on.
fn holder_line_is_shaped(line: &str) -> bool {
    let line = line.trim();
    if line.starts_with("(+") && line.ends_with(" more)") {
        return true;
    }
    let Some((holder, fds)) = line.rsplit_once(": ") else {
        return false;
    };
    holder.contains('(')
        && holder.ends_with(')')
        && fds
            .strip_suffix(" fds")
            .and_then(|count| count.parse::<usize>().ok())
            .is_some_and(|count| count > 0)
}

/// A `-F` value may carry a newline, so the parser is fed lines that are
/// not records at all — and one starting with a multi-byte character puts
/// byte 1 mid-character. It has to skip such a line, not slice it.
#[test]
fn a_continuation_line_is_skipped_rather_than_sliced() {
    // The `n` record's value is "/tmp/w\nédir", so "édir" arrives as a
    // line of its own whose first character is two bytes wide.
    let feed = "p11\ncsh\nf3\nn/dev/pts/4\nf7\nn/tmp/w\n\u{00e9}dir\np12\ncsh\nf5\nn/dev/pts/6\n";

    let rendered = render_holders(feed.as_bytes());

    assert_eq!(rendered, "sh(11): 1 fds\n    sh(12): 1 fds", "{rendered}");
}

#[test]
fn the_rendered_census_labels_every_field() {
    let census = PtyCensus {
        tabs_live: Some(12),
        fd_limit: Ok("soft 1024 hard 1048576".to_string()),
        open_fds: Err("/dev/fd: nope".to_string()),
        pool: vec![
            ("/dev/ttys* device nodes", Ok("21".to_string())),
            ("kern.tty.ptmx_max", Ok("511".to_string())),
        ],
        holders: Ok("sh(4242): 2 fds".to_string()),
    };

    let rendered = census.render();

    for expected in [
        "tabs live in this test: 12",
        "RLIMIT_NOFILE: soft 1024 hard 1048576",
        "open fds (/dev/fd entries): unavailable: /dev/fd: nope",
        "/dev/ttys* device nodes: 21",
        "kern.tty.ptmx_max: 511",
        "candidate holders (lsof): sh(4242): 2 fds",
    ] {
        assert!(
            rendered.contains(expected),
            "census render is missing {expected:?}:\n{rendered}"
        );
    }
}
