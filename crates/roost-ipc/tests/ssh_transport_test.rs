//! The SSH tunnel runtime, driven against a fake `ssh`.
//!
//! Everything here is process choreography — which invocation ran, in
//! what order, with what still on disk, and what the tunnel concluded
//! from an exit code. None of it is observable from a pure unit test and
//! none of it needs a real host, so the whole suite runs against
//! `tools/roosttest/fixtures/fake-ssh.sh` (its header documents the
//! contract) and reads the invocation log that fixture appends to.
//!
//! Two rules keep the suite parallel-safe. Each test gets its own
//! scratch parent directory, handed to the tunnel through
//! [`SshTunnelOptions`] rather than through `$TMPDIR` — process-global
//! env is shared by every test in this binary. And the fixture's own
//! configuration reaches it through a per-test conf file sourced beside
//! the symlink that named it, for the same reason — a *written* wrapper
//! would race `execve` against a sibling thread's fork (see
//! [`Harness::new`]).
//!
//! There are no sleeps: everything that has to settle is polled for.

use std::path::{Path, PathBuf};
use std::time::Duration;

use roost_ipc::bootstrap::shell_quote;
use roost_ipc::messages::SESSION_PROTOCOL_VERSION;
use roost_ipc::ssh::{
    classify, remote_command_for, verify_ssh_target, ResolvedTransport, SshConfigPaths, SshFailure,
    SshTarget, SshTunnel, SshTunnelOptions,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

// ============================================================================
// Harness
// ============================================================================

/// A fake `ssh` plus a scratch parent all its own.
struct Harness {
    _root: tempfile::TempDir,
    parent: PathBuf,
    log: PathBuf,
    ssh_bin: PathBuf,
}

impl Harness {
    /// `mode` and `exec` are the fixture's `FAKE_SSH_MODE` and
    /// `FAKE_SSH_EXEC`.
    fn new(mode: &str, exec: &str) -> Self {
        // Rooted in `/tmp` rather than `$TMPDIR`: macOS's per-user
        // `$TMPDIR` is deep enough that a scratch directory under it can
        // overrun `sun_path`, which is exactly the case
        // `pick_socket_dir` falls back to `/tmp` for. The tests assert
        // on a *known* directory, so they take the short root directly.
        let root = tempfile::Builder::new()
            .prefix("roost-ssh-t")
            .tempdir_in("/tmp")
            .expect("scratch root");
        let parent = root.path().join("scratch");
        std::fs::create_dir(&parent).expect("scratch parent");
        let log = root.path().join("invocations.log");
        std::fs::write(&log, b"").expect("invocation log");

        // A symlink to the committed fixture plus a plain conf file
        // beside it, rather than a per-test wrapper script. The tests in
        // this binary run in parallel threads, and a script written here
        // while a sibling is forking races `execve`: the fork inherits
        // our still-open write descriptor, and the exec that follows
        // answers ETXTBSY ("Text file busy") on Linux. Nothing exec'd is
        // ever written, so there is no such window — see the fixture's
        // own note on `$0.conf`.
        let ssh_bin = root.path().join("ssh");
        std::fs::write(
            root.path().join("ssh.conf"),
            format!(
                "FAKE_SSH_LOG={log}\nFAKE_SSH_MODE={mode}\nFAKE_SSH_EXEC={exec}\n\
                 export FAKE_SSH_LOG FAKE_SSH_MODE FAKE_SSH_EXEC\n",
                log = shell_quote(&log.display().to_string()),
                mode = shell_quote(mode),
                exec = shell_quote(exec),
            ),
        )
        .expect("write the fake ssh config");
        std::os::unix::fs::symlink(fixture_path(), &ssh_bin).expect("link the ssh wrapper");

        Self {
            _root: root,
            parent,
            log,
            ssh_bin,
        }
    }

    fn options(&self) -> SshTunnelOptions {
        SshTunnelOptions {
            // Neither file exists, so the generated config carries no
            // `Include` at all — a test must never read the developer's
            // real `~/.ssh/config`.
            config_paths: SshConfigPaths {
                user: None,
                system: None,
            },
            scratch_parents: vec![self.parent.clone()],
            ssh_bin: self.ssh_bin.clone(),
            // This harness fakes `ssh` itself and never runs the remote
            // command, so it wants the *shipped* ladder — the same one
            // `is_exec` below recognizes.
            jail_fs_root: false,
        }
    }

    /// Every scratch directory this host currently has under the
    /// harness's parent. A directory name carries the attempt's pid and
    /// sequence, so a test that wants "did anything survive" globs
    /// `roost-ssh-<host>-*` rather than naming one path.
    fn scratch_dirs(&self, host_id: &str) -> Vec<PathBuf> {
        let prefix = format!("roost-ssh-{host_id}-");
        let mut dirs: Vec<PathBuf> = std::fs::read_dir(&self.parent)
            .expect("read the scratch parent")
            .filter_map(|entry| {
                let entry = entry.ok()?;
                entry
                    .file_name()
                    .to_str()?
                    .starts_with(&prefix)
                    .then(|| entry.path())
            })
            .collect();
        dirs.sort();
        dirs
    }

    /// A scratch directory as some *other* attempt would have named it.
    /// `seq` is `u64::MAX` in every caller: the sequence a live tunnel
    /// claims comes from a counter that starts at zero, so nothing this
    /// process mints can ever collide with one of these.
    fn leftover_dir(&self, host_id: &str, pid: u32, seq: u64) -> PathBuf {
        let dir = self.parent.join(format!("roost-ssh-{host_id}-{pid}-{seq}"));
        std::fs::create_dir(&dir).expect("pre-create a leftover scratch directory");
        dir
    }

    /// Every invocation so far: the argv, with the fixture's leading
    /// `pid=` field stripped off.
    fn invocations(&self) -> Vec<Vec<String>> {
        self.lines()
            .into_iter()
            .map(|fields| fields[1..].to_vec())
            .collect()
    }

    fn pids(&self) -> Vec<i32> {
        self.lines()
            .into_iter()
            .filter_map(|fields| fields[0].strip_prefix("pid=")?.parse().ok())
            .collect()
    }

    fn lines(&self) -> Vec<Vec<String>> {
        std::fs::read_to_string(&self.log)
            .expect("read the invocation log")
            .lines()
            .filter(|line| !line.is_empty())
            .map(|line| line.split('\t').map(str::to_string).collect())
            .collect()
    }

    fn count(&self, kind: fn(&[String]) -> bool) -> usize {
        self.invocations()
            .iter()
            .filter(|argv| kind(argv.as_slice()))
            .count()
    }
}

fn fixture_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tools/roosttest/fixtures/fake-ssh.sh")
        .canonicalize()
        .expect("the fake-ssh fixture must exist")
}

/// The mux warm-up: its remote command is the literal `true`.
fn is_establish(argv: &[String]) -> bool {
    argv.last().is_some_and(|last| last == "true")
}

/// A per-connection exec: its remote command is the bridge one-liner.
fn is_exec(argv: &[String]) -> bool {
    argv.last()
        .is_some_and(|last| *last == remote_command_for(false))
}

fn is_master_exit(argv: &[String]) -> bool {
    argv.windows(2)
        .any(|pair| pair[0] == "-O" && pair[1] == "exit")
}

fn ssh_target(raw: &str) -> SshTarget {
    match classify(raw).expect("classify the target") {
        ResolvedTransport::Ssh(target) => target,
        other => panic!("expected an ssh target, got {other:?}"),
    }
}

fn scaled(budget: Duration) -> Duration {
    budget.mul_f64(roost_ipc::session_launch::timeout_scale())
}

/// How long the grandchild left holding a stderr pipe must sleep, sized
/// off the already-scaled budget it has to outlive: on a 3x runner an
/// unscaled sleeper would be dead before the budget expired and the test
/// would pass while proving nothing.
fn sleeper_secs(budget: Duration) -> u64 {
    (budget.as_secs_f64() + 15.0).ceil() as u64
}

async fn wait_for(what: &str, mut ready: impl FnMut() -> bool) {
    // A poll ceiling only ever grows: a scale below 1 is for shrinking
    // budgets under test, not for shortening how long we wait on one.
    let scale = roost_ipc::session_launch::timeout_scale().max(1.0);
    let deadline = std::time::Instant::now() + Duration::from_secs(20).mul_f64(scale);
    loop {
        if ready() {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting for {what}"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// `kill(pid, 0)`: true while the pid is addressable, which includes an
/// unreaped zombie — so this doubles as a check that the tunnel reaped
/// what it killed.
fn alive(pid: i32) -> bool {
    // SAFETY: signal 0 sends nothing; it only asks whether the pid can
    // be signalled.
    unsafe { libc::kill(pid, 0) == 0 }
}

/// Dial the bridge and read that one connection out to EOF. For a
/// failing exec the EOF is the moment `serve` has finished recording its
/// verdict, so `last_error` is readable straight after with no polling.
async fn read_bridge_to_eof(tunnel: &SshTunnel) -> Vec<u8> {
    let mut stream = UnixStream::connect(tunnel.bridge_socket())
        .await
        .expect("dial the bridge");
    let mut back = Vec::new();
    stream.read_to_end(&mut back).await.expect("read to EOF");
    back
}

async fn echo_through(bridge: &Path, payload: &[u8]) -> UnixStream {
    let mut stream = UnixStream::connect(bridge).await.expect("dial the bridge");
    stream.write_all(payload).await.expect("write the payload");
    let mut back = vec![0u8; payload.len()];
    stream
        .read_exact(&mut back)
        .await
        .expect("read the echo back");
    assert_eq!(back, payload);
    stream
}

// ============================================================================
// establish
// ============================================================================

/// The pinned divergence from herdr's serialized transport: connections
/// are served concurrently, so two clients that connect and then say
/// nothing — which is exactly what a held-open events connection looks
/// like — cannot keep a third from completing.
#[tokio::test]
async fn concurrent_connections_are_served_while_idle_ones_are_held_open() {
    let harness = Harness::new("ok", "cat");
    let tunnel = SshTunnel::open("aabbccdd", &ssh_target("workbox"), harness.options())
        .await
        .expect("open");
    tunnel.establish().await.expect("establish");

    let bridge = tunnel.bridge_socket().to_path_buf();
    assert!(bridge.exists(), "establish must bind the bridge socket");

    let idle_one = UnixStream::connect(&bridge).await.expect("first idle dial");
    let idle_two = UnixStream::connect(&bridge)
        .await
        .expect("second idle dial");
    wait_for("two idle connections to reach their own ssh exec", || {
        harness.count(is_exec) >= 2
    })
    .await;

    let third = echo_through(&bridge, b"hello\n").await;
    let fourth = echo_through(&bridge, b"and again\n").await;

    assert_eq!(harness.count(is_establish), 1);
    assert_eq!(harness.count(is_exec), 4);
    assert!(tunnel.last_error().is_none(), "{:?}", tunnel.last_error());

    drop((idle_one, idle_two, third, fourth));
}

#[tokio::test]
async fn an_auth_failure_is_classified_and_binds_nothing() {
    let harness = Harness::new("auth-fail", "cat");
    let tunnel = SshTunnel::open("00000001", &ssh_target("workbox"), harness.options())
        .await
        .expect("open");

    let error = tunnel.establish().await.expect_err("auth must fail");
    assert_eq!(error.failure(), Some(&SshFailure::Auth), "{error}");
    assert!(
        !tunnel.bridge_socket().exists(),
        "a failed establish must never leave a bound bridge socket"
    );
}

/// The wary case: `ssh`'s changed-key blob *also* contains "Host key
/// verification failed", and reporting it as the ordinary unknown-key
/// case would tell the user to go accept the new key.
#[tokio::test]
async fn a_changed_host_key_outranks_the_unknown_key_substring_it_contains() {
    let harness = Harness::new("hostkey-changed", "cat");
    let tunnel = SshTunnel::open("00000002", &ssh_target("workbox"), harness.options())
        .await
        .expect("open");

    let error = tunnel
        .establish()
        .await
        .expect_err("a changed key must fail");
    assert_eq!(
        error.failure(),
        Some(&SshFailure::ChangedHostKey),
        "{error}"
    );
    assert!(error.to_string().to_lowercase().contains("do not accept"));
}

// ============================================================================
// Bounded stderr drains (#379)
// ============================================================================

// Everything in this section is timed against a **grandchild** holding
// the stderr pipe open — `hold_stderr_open` in the fixture, which is
// what an `ssh` `ProxyCommand` helper does in production. Reaping our
// own child does not close that pipe, so a drain that waits for EOF
// waits for the grandchild instead of for its own budget.

/// `SshTunnel::establish`'s own warm-up budget, mirrored — the module
/// keeps it private and there is no seam to shrink it, so a test of the
/// timeout arm has to spend it.
const ESTABLISH_BUDGET: Duration = Duration::from_secs(30);

/// #379 and §3.3 C's scoping, on one run of the arm they share: the
/// establish that spends its budget kills its child, and the drain
/// behind that kill is bounded by the grace floor rather than by the
/// lifetime of a grandchild still holding the pipe — *and* the failure
/// it returns is never marked truncated, however that drain went. The
/// timeout is its own verdict; the tail would only have sharpened the
/// copy. That negative is the load-bearing half: a black-holed
/// establish behind a `ProxyCommand` is exactly the case auto-reconnect
/// must keep retrying, and a uniform truncation veto would refuse it.
///
/// One test rather than two because reaching this arm costs a full
/// [`ESTABLISH_BUDGET`] of wall clock and leaves a sleeper behind; both
/// facts are read off the same failure.
#[tokio::test]
async fn a_timed_out_establish_returns_on_its_budget_and_is_never_marked_truncated() {
    let budget = scaled(ESTABLISH_BUDGET);
    let sleeper = sleeper_secs(budget);
    let harness = Harness::new(&format!("slow-stderr-hang:{sleeper}"), "cat");
    let tunnel = SshTunnel::open("0000000d", &ssh_target("workbox"), harness.options())
        .await
        .expect("open");

    let started = std::time::Instant::now();
    let error = tunnel
        .establish()
        .await
        .expect_err("a hung warm-up must fail");
    let elapsed = started.elapsed();

    // Tight against the budget, not merely under the sleeper: the grace
    // floor is 200ms, so anything past a couple of seconds means the
    // drain is waiting on the grandchild again.
    assert!(
        elapsed < budget + Duration::from_secs(3),
        "establish took {elapsed:?} against a {budget:?} budget and a {sleeper}s sleeper: {error}"
    );
    assert!(
        matches!(error.failure(), Some(SshFailure::Transport(Some(line))) if line.contains("timed out")),
        "{error:?}"
    );
    assert!(
        !error.truncated(),
        "the timeout is its own evidence: {error}"
    );
}

/// The other half of the same rule: the non-zero-exit arm gets the
/// *real* deadline, which still has slack because the child exited
/// early — so a far side whose stderr arrives late still classifies as
/// itself instead of degrading to the `Transport` fallthrough. This is
/// the arm a changed host key is decided on.
#[tokio::test]
async fn a_slow_stderr_non_zero_exit_still_classifies_as_a_changed_host_key() {
    let harness = Harness::new("slow-stderr-changed-key:2", "cat");
    let tunnel = SshTunnel::open("0000000f", &ssh_target("workbox"), harness.options())
        .await
        .expect("open");

    let error = tunnel
        .establish()
        .await
        .expect_err("a changed key must fail");
    assert_eq!(
        error.failure(),
        Some(&SshFailure::ChangedHostKey),
        "{error}"
    );
    assert!(!error.truncated(), "the evidence did arrive: {error}");
}

#[tokio::test]
async fn exit_127_reads_as_a_missing_remote_binary() {
    let harness = Harness::new("exit-127", "cat");
    let tunnel = SshTunnel::open("00000003", &ssh_target("workbox"), harness.options())
        .await
        .expect("open");

    let error = tunnel.establish().await.expect_err("127 must fail");
    assert_eq!(error.failure(), Some(&SshFailure::NotFound), "{error}");
}

// ============================================================================
// Ordering, teardown, and reclaim
// ============================================================================

/// The order is the contract: warm up, exec per connection, and exit the
/// master *last* — while its control socket is still on disk, because
/// that path is the only address the master has.
#[tokio::test]
async fn the_master_exit_runs_last_and_while_the_control_socket_still_exists() {
    let harness = Harness::new("ok", "cat");
    let tunnel = SshTunnel::open("00000004", &ssh_target("workbox"), harness.options())
        .await
        .expect("open");
    tunnel.establish().await.expect("establish");

    let bridge = tunnel.bridge_socket().to_path_buf();
    drop(echo_through(&bridge, b"ping\n").await);
    wait_for("the connection's exec to be logged", || {
        harness.count(is_exec) == 1
    })
    .await;

    tunnel.shutdown().await;

    let invocations = harness.invocations();
    assert!(is_establish(&invocations[0]), "{:?}", invocations[0]);
    assert!(is_exec(&invocations[1]), "{:?}", invocations[1]);
    let last = invocations.last().expect("at least one invocation");
    assert!(is_master_exit(last), "{last:?}");
    assert!(
        last.contains(&"ctl-exists=1".to_string()),
        "the master exit must run before the control socket goes: {last:?}"
    );
    assert_eq!(harness.count(is_master_exit), 1);
}

/// A pid no attempt of this process ever wrote. The sweep never asks
/// whether a pid is alive — its liveness question is put to
/// `bridge.sock` and nothing else — so this only has to differ from ours,
/// which is what makes the cross-process rule the one under test.
const OTHER_PID: u32 = u32::MAX - 1;

/// A crashed client leaves a directory behind but its `ssh` master
/// outlives it on `ControlPersist`. Reclaiming means exiting that master
/// first — removing its control socket underneath it would strand a
/// process nothing can address any more.
#[tokio::test]
async fn a_stale_directory_is_reclaimed_after_exiting_the_old_master() {
    let harness = Harness::new("ok", "cat");
    let host_id = "00000005";
    let stale = harness.leftover_dir(host_id, OTHER_PID, u64::MAX);
    // A socket file whose listener is gone — what a SIGKILL leaves.
    let corpse_path = stale.join("bridge.sock");
    let corpse = std::os::unix::net::UnixListener::bind(&corpse_path).expect("bind");
    drop(corpse);
    // `close(2)` on a listener is not instantly visible to `connect(2)`:
    // under load a connect can still land on the dying socket's accept
    // backlog and succeed. The probe is fail-safe and reports that as
    // live, so the corpse has to actually go cold before the reclaim
    // path is the one under test.
    wait_for("the corpse socket to start refusing connections", || {
        std::os::unix::net::UnixStream::connect(&corpse_path).is_err()
    })
    .await;
    std::fs::write(stale.join("ctl"), b"").expect("pre-create the control socket");

    let tunnel = SshTunnel::open(host_id, &ssh_target("workbox"), harness.options())
        .await
        .expect("a dead bridge socket must be reclaimable");

    let invocations = harness.invocations();
    assert_eq!(invocations.len(), 1, "{invocations:?}");
    assert!(is_master_exit(&invocations[0]), "{:?}", invocations[0]);
    assert!(
        invocations[0].contains(&"ctl-exists=1".to_string()),
        "the old master must be exited before its socket is removed: {:?}",
        invocations[0]
    );
    assert!(!stale.exists(), "the stale directory must be swept away");
    assert_eq!(
        harness.scratch_dirs(host_id),
        vec![tunnel
            .bridge_socket()
            .parent()
            .expect("a scratch dir")
            .to_path_buf()],
        "and the only one left is this attempt's own"
    );

    tunnel.establish().await.expect("establish after a reclaim");
    let bridge = tunnel.bridge_socket().to_path_buf();
    drop(echo_through(&bridge, b"fresh\n").await);
}

#[tokio::test]
async fn a_live_bridge_socket_refuses_a_second_tunnel() {
    let harness = Harness::new("ok", "cat");
    let host_id = "00000006";
    let occupied = harness.leftover_dir(host_id, OTHER_PID, u64::MAX);
    let live = tokio::net::UnixListener::bind(occupied.join("bridge.sock")).expect("bind");

    let error = SshTunnel::open(host_id, &ssh_target("workbox"), harness.options())
        .await
        .err()
        .expect("a live bridge socket must be refused");
    assert!(error.to_string().contains("another Roost"), "{error}");
    assert!(error.failure().is_none(), "this is not an ssh failure");
    assert!(
        harness.invocations().is_empty(),
        "refusing must not run any ssh"
    );
    assert!(
        occupied.exists(),
        "and the other Roost's directory is left exactly as it was"
    );

    drop(live);
}

/// The same-process rule, and the guard against a rapid double Connect:
/// this app opens one tunnel per saved host at a time, so a directory
/// *this pid* left behind is superseded by construction. It is reclaimed
/// with no probe at all — probing it would find the previous attempt's
/// own live bridge socket and refuse the replacement, which is exactly
/// the deadlock a second Connect must not hit.
#[tokio::test]
async fn a_leftover_directory_from_this_process_is_superseded_not_refused() {
    let harness = Harness::new("ok", "cat");
    let host_id = "0000000a";
    let superseded = harness.leftover_dir(host_id, std::process::id(), u64::MAX);
    let live = tokio::net::UnixListener::bind(superseded.join("bridge.sock")).expect("bind");
    std::fs::write(superseded.join("ctl"), b"").expect("pre-create the control socket");

    let tunnel = SshTunnel::open(host_id, &ssh_target("workbox"), harness.options())
        .await
        .expect("this process's own leftovers must never refuse it");

    let invocations = harness.invocations();
    assert_eq!(invocations.len(), 1, "{invocations:?}");
    assert!(is_master_exit(&invocations[0]), "{:?}", invocations[0]);
    assert!(
        invocations[0].contains(&"ctl-exists=1".to_string()),
        "a superseded master is exited before its socket goes: {:?}",
        invocations[0]
    );
    assert!(
        !superseded.exists(),
        "the superseded directory is reclaimed"
    );
    assert_eq!(
        harness.scratch_dirs(host_id),
        vec![tunnel
            .bridge_socket()
            .parent()
            .expect("a scratch dir")
            .to_path_buf()],
        "the replacement owns a directory of its own"
    );

    tunnel
        .establish()
        .await
        .expect("establish after superseding");
    drop(echo_through(tunnel.bridge_socket(), b"fresh\n").await);
    drop(live);
}

/// Two overlapping opens for one host — a rapid double Connect, whose
/// establishes are in flight at the same time — never share a directory,
/// so neither one's teardown can delete the other's files.
#[tokio::test]
async fn two_overlapping_tunnels_for_one_host_never_share_a_directory() {
    let harness = Harness::new("ok", "cat");
    let host_id = "0000000b";

    let first = SshTunnel::open(host_id, &ssh_target("workbox"), harness.options())
        .await
        .expect("first open");
    let first_dir = first.bridge_socket().parent().expect("a dir").to_path_buf();
    first.establish().await.expect("first establish");

    // The second open sweeps the first away (same pid, superseded) and
    // takes a directory of its own.
    let second = SshTunnel::open(host_id, &ssh_target("workbox"), harness.options())
        .await
        .expect("second open");
    let second_dir = second
        .bridge_socket()
        .parent()
        .expect("a dir")
        .to_path_buf();
    assert_ne!(first_dir, second_dir);
    second.establish().await.expect("second establish");
    drop(echo_through(second.bridge_socket(), b"live\n").await);

    // The loser's teardown lands late, and must take nothing of the
    // winner's with it.
    first.shutdown().await;
    assert!(second.bridge_socket().exists(), "the live bridge survives");
    drop(echo_through(second.bridge_socket(), b"still here\n").await);
    assert_eq!(harness.scratch_dirs(host_id), vec![second_dir]);
}

#[tokio::test]
async fn shutdown_leaves_no_scratch_directory_and_no_ssh_children() {
    let harness = Harness::new("ok", "cat");
    let host_id = "00000007";
    let tunnel = SshTunnel::open(host_id, &ssh_target("workbox"), harness.options())
        .await
        .expect("open");
    tunnel.establish().await.expect("establish");

    let bridge = tunnel.bridge_socket().to_path_buf();
    drop(echo_through(&bridge, b"bye\n").await);
    wait_for("the connection's exec to be logged", || {
        harness.count(is_exec) == 1
    })
    .await;

    let pids = harness.pids();
    assert!(pids.len() >= 2, "{pids:?}");
    tunnel.shutdown().await;

    assert!(
        harness.scratch_dirs(host_id).is_empty(),
        "{:?}",
        harness.scratch_dirs(host_id)
    );
    wait_for("every fake ssh to be gone", || {
        pids.iter().all(|pid| !alive(*pid))
    })
    .await;

    // Idempotent: a second shutdown has nothing left to do and must not
    // run a second `-O exit`.
    tunnel.shutdown().await;
    assert_eq!(harness.count(is_master_exit), 1);
}

/// Not shutting down is not an excuse to leak: `Drop` still exits the
/// master and removes the directory, blocking because it has no runtime
/// to await on.
#[tokio::test]
async fn dropping_a_tunnel_still_exits_the_master_and_removes_the_directory() {
    let harness = Harness::new("ok", "cat");
    let host_id = "00000008";
    {
        let tunnel = SshTunnel::open(host_id, &ssh_target("workbox"), harness.options())
            .await
            .expect("open");
        tunnel.establish().await.expect("establish");
        assert_eq!(harness.scratch_dirs(host_id).len(), 1);
    }

    assert!(
        harness.scratch_dirs(host_id).is_empty(),
        "Drop must remove the scratch directory"
    );
    let last = harness
        .invocations()
        .last()
        .cloned()
        .expect("at least one invocation");
    assert!(is_master_exit(&last), "{last:?}");
    assert!(last.contains(&"ctl-exists=1".to_string()), "{last:?}");
}

// ============================================================================
// Connection failures
// ============================================================================

/// A stream cut mid-connection: the client sees an EOF, and the verdict
/// is recorded where a caller can read it rather than only logged.
#[tokio::test]
async fn a_stream_cut_mid_connection_records_a_transport_failure() {
    let harness = Harness::new("drop-after:4", "cat");
    let tunnel = SshTunnel::open("00000009", &ssh_target("workbox"), harness.options())
        .await
        .expect("open");
    tunnel.establish().await.expect("establish");

    let mut stream = UnixStream::connect(tunnel.bridge_socket())
        .await
        .expect("dial the bridge");
    stream.write_all(b"12345678").await.expect("write");
    // Half-close so the remote `cat` sees EOF and the truncated pipeline
    // can finish; without it the fixture would sit waiting for input it
    // is never going to get.
    stream.shutdown().await.expect("half-close");

    let mut back = Vec::new();
    stream.read_to_end(&mut back).await.expect("read to EOF");
    assert_eq!(back, b"1234", "the stream is cut after four bytes");

    wait_for("the cut connection to be recorded", || {
        tunnel.last_error().is_some()
    })
    .await;
    let recorded = tunnel.last_error().expect("a recorded failure");
    assert_eq!(recorded.generation, 1, "the first exec of this tunnel");
    assert!(
        matches!(recorded.failure, SshFailure::Transport(_)),
        "an unclassifiable non-zero exit is a transport failure, got {:?}",
        recorded.failure
    );
}

/// A connection that opens and closes without traffic — a probe, like
/// the reclaim path's own liveness check — must not read as a failure:
/// the exec exits cleanly, and only the exec's verdict counts.
#[tokio::test]
async fn a_silent_probe_connection_records_no_failure() {
    let harness = Harness::new("ok", "cat");
    let tunnel = SshTunnel::open("0000000c", &ssh_target("workbox"), harness.options())
        .await
        .expect("open");
    tunnel.establish().await.expect("establish");

    let probe = UnixStream::connect(tunnel.bridge_socket())
        .await
        .expect("dial the bridge");
    drop(probe);

    // A second, real connection proves the exec for the probe has come
    // and gone by the time we assert.
    let mut stream = UnixStream::connect(tunnel.bridge_socket())
        .await
        .expect("dial again");
    stream.write_all(b"ping").await.expect("write");
    stream.shutdown().await.expect("half-close");
    let mut back = Vec::new();
    stream.read_to_end(&mut back).await.expect("echo");
    assert_eq!(back, b"ping");

    assert_eq!(
        tunnel.last_error(),
        None,
        "a zero-byte clean connection is not a failure"
    );
    tunnel.shutdown().await;
}

/// `serve`'s own reap budget, mirrored — private to the module, and the
/// bound both tests below are written against.
const REAP_BUDGET: Duration = Duration::from_secs(2);

/// A remote command that fails with something classifiable on stderr and
/// then leaves a grandchild holding that pipe open for `sleeper`
/// seconds. Only stderr is held: holding stdout would stall the exec's
/// own pump and prove nothing about the drain.
fn slow_stderr_exec(sleeper: u64) -> String {
    format!(
        "sh -c 'sleep {sleeper}' </dev/null >/dev/null & printf 'Permission denied\\n' >&2; exit 1"
    )
}

/// #379 at the per-connection exec: the drain behind `serve`'s reap is
/// bounded by that reap's budget, so a connection whose stderr a
/// grandchild still holds open ends on the budget rather than on the
/// grandchild's lifetime.
#[tokio::test]
async fn a_connection_whose_stderr_is_held_open_ends_on_the_reap_budget() {
    let budget = scaled(REAP_BUDGET);
    let sleeper = sleeper_secs(budget);
    let harness = Harness::new("ok", &slow_stderr_exec(sleeper));
    let tunnel = SshTunnel::open("00000010", &ssh_target("workbox"), harness.options())
        .await
        .expect("open");
    tunnel.establish().await.expect("establish");

    let started = std::time::Instant::now();
    read_bridge_to_eof(&tunnel).await;
    let elapsed = started.elapsed();

    assert!(
        elapsed < budget + Duration::from_secs(4),
        "the connection took {elapsed:?} against a {budget:?} budget and a {sleeper}s sleeper"
    );
}

/// §3.3 C's first truncation path, at the site where the tail **is** the
/// evidence: the far side said `Permission denied`, the drain expired
/// before it could be read, and the family fell through to `Transport`.
/// The flag is what stops a retry policy from trusting that fallthrough.
#[tokio::test]
async fn a_drain_that_expires_marks_the_recorded_failure_truncated() {
    let sleeper = sleeper_secs(scaled(REAP_BUDGET));
    let harness = Harness::new("ok", &slow_stderr_exec(sleeper));
    let tunnel = SshTunnel::open("00000011", &ssh_target("workbox"), harness.options())
        .await
        .expect("open");
    tunnel.establish().await.expect("establish");

    read_bridge_to_eof(&tunnel).await;

    let recorded = tunnel.last_error().expect("a recorded failure");
    assert!(
        recorded.truncated,
        "an expired drain must say so: {recorded:?}"
    );
    assert!(
        matches!(recorded.failure, SshFailure::Transport(_)),
        "and it degraded to the fallthrough, which is the hazard: {recorded:?}"
    );
}

/// §3.3 C's second truncation path, Kimi's: `read_tail` keeps only the
/// last 4 KiB. Here the family marker is in the surviving window — a
/// chatty `ProxyCommand` that scrolled *earlier* output out — so the
/// classification is right and only the flag records that anything was
/// dropped. The same eviction one line later would have taken the
/// marker with it, which is why the flag is set on eviction rather than
/// on a failed match.
#[tokio::test]
async fn a_tail_whose_byte_cap_evicted_leading_output_is_marked_truncated() {
    let harness = Harness::new(
        "ok",
        "yes chatter | head -c 6000 >&2; printf 'Permission denied\\n' >&2; exit 1",
    );
    let tunnel = SshTunnel::open("00000012", &ssh_target("workbox"), harness.options())
        .await
        .expect("open");
    tunnel.establish().await.expect("establish");

    read_bridge_to_eof(&tunnel).await;

    let recorded = tunnel.last_error().expect("a recorded failure");
    assert_eq!(
        recorded.failure,
        SshFailure::Auth,
        "the marker survived the cap: {recorded:?}"
    );
    assert!(
        recorded.truncated,
        "but the cap discarded what came before it: {recorded:?}"
    );
}

/// The `serve` reorder (§3.9, Kimi): the client-visible EOF is what
/// wakes the app thread's drop handling, so the verdict has to be
/// recorded *before* it. Asserted with no polling at all — the read
/// returning is the moment under test, and a `last_error` that is still
/// `None` here is a changed host key seen as a bare EOF.
///
/// The exec closes its stdout and only *then* takes a second to die, so
/// the window between the wire ending and the verdict landing is real
/// rather than a scheduling accident: under the old ordering the client
/// saw the EOF a whole second before the family was recorded.
#[tokio::test]
async fn a_failed_exec_records_its_family_before_the_client_sees_eof() {
    let harness = Harness::new(
        "ok",
        "printf 'bye'; printf 'Permission denied\\n' >&2; exec 1>&-; sleep 1; exit 1",
    );
    let tunnel = SshTunnel::open("00000013", &ssh_target("workbox"), harness.options())
        .await
        .expect("open");
    tunnel.establish().await.expect("establish");

    assert_eq!(read_bridge_to_eof(&tunnel).await, b"bye");

    let recorded = tunnel
        .last_error()
        .expect("the family must be recorded by the time the client can see the EOF");
    assert_eq!(recorded.failure, SshFailure::Auth, "{recorded:?}");
    assert!(!recorded.truncated, "{recorded:?}");
}

// ============================================================================
// One-shot verify
// ============================================================================

/// Verification runs outside any mux and leaves nothing behind — no
/// control socket, no persisting master, no directory. That is what
/// makes it safe to offer for a host the user has not committed to yet.
#[tokio::test]
async fn verify_speaks_identify_over_a_one_shot_exec_outside_the_mux() {
    let result = format!(
        r#"{{"app_version":"0.0.18","session_protocol":{SESSION_PROTOCOL_VERSION},"payload_kinds":["vt"],"libghostty_build":"test-build","session_id":"sess-verify","started_at":"2026-08-31T00:00:00Z"}}"#
    );
    let response = format!(r#"{{"id":"1","ok":true,"result":{result}}}"#);
    let harness = Harness::new(
        "ok",
        &format!(
            "read -r _request; printf '%s\\n' {}",
            shell_quote(&response)
        ),
    );

    let identity = verify_ssh_target(
        &ssh_target("workbox"),
        &harness.options(),
        Duration::from_secs(30),
    )
    .await
    .expect("verify");
    assert_eq!(identity.session_id, "sess-verify");
    assert_eq!(identity.session_protocol, SESSION_PROTOCOL_VERSION);

    let invocations = harness.invocations();
    assert_eq!(invocations.len(), 1, "{invocations:?}");
    let argv = &invocations[0];
    assert!(
        argv.contains(&"ControlMaster=no".to_string()),
        "verify must stay outside the mux: {argv:?}"
    );
    assert!(
        !argv.contains(&"-S".to_string()),
        "verify must not address a control socket: {argv:?}"
    );
    assert!(is_exec(argv), "{argv:?}");

    let leftovers: Vec<_> = std::fs::read_dir(&harness.parent)
        .expect("read the scratch parent")
        .filter_map(|entry| entry.ok().map(|entry| entry.file_name()))
        .collect();
    assert!(
        leftovers.is_empty(),
        "verify must leave no scratch directory: {leftovers:?}"
    );

    let pids = harness.pids();
    wait_for("the verify child to be gone", || {
        pids.iter().all(|pid| !alive(*pid))
    })
    .await;
}

/// #379 at the verify: a far side that never answers spends the whole
/// budget, and the drain behind the kill that ends it must be the grace
/// floor — not the lifetime of a grandchild still holding the stderr
/// pipe. This is the shape plan 039 measured before the fix: a 1s budget
/// against a 10s sleeper took 10s.
#[tokio::test]
async fn a_verify_that_times_out_returns_on_its_budget_not_the_grandchilds_lifetime() {
    let budget = Duration::from_secs(1);
    let sleeper = sleeper_secs(scaled(budget));
    let harness = Harness::new(
        "ok",
        &format!("sh -c 'sleep {sleeper}' </dev/null >/dev/null & exec sleep 3600"),
    );

    let started = std::time::Instant::now();
    let error = verify_ssh_target(&ssh_target("workbox"), &harness.options(), budget)
        .await
        .expect_err("a far side that never answers must fail");
    let elapsed = started.elapsed();

    assert!(
        elapsed < scaled(budget) + Duration::from_secs(4),
        "verify took {elapsed:?} against a {budget:?} budget and a {sleeper}s sleeper: {error}"
    );
}
