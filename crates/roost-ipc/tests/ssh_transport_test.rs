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
//! configuration reaches it through a per-test wrapper script that
//! `exec`s it with the right variables, for the same reason.
//!
//! There are no sleeps: everything that has to settle is polled for.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use roost_ipc::messages::SESSION_PROTOCOL_VERSION;
use roost_ipc::ssh::{
    classify, remote_command, verify_ssh_target, ResolvedTransport, SshConfigPaths, SshFailure,
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

        let ssh_bin = root.path().join("ssh");
        std::fs::write(
            &ssh_bin,
            format!(
                "#!/bin/sh\nFAKE_SSH_LOG={log}\nFAKE_SSH_MODE={mode}\nFAKE_SSH_EXEC={exec}\n\
                 export FAKE_SSH_LOG FAKE_SSH_MODE FAKE_SSH_EXEC\nexec {fixture} \"$@\"\n",
                log = sh_quote(&log.display().to_string()),
                mode = sh_quote(mode),
                exec = sh_quote(exec),
                fixture = sh_quote(&fixture_path().display().to_string()),
            ),
        )
        .expect("write the ssh wrapper");
        std::fs::set_permissions(&ssh_bin, std::fs::Permissions::from_mode(0o755))
            .expect("chmod the ssh wrapper");

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
        }
    }

    fn scratch_dir(&self, host_id: &str) -> PathBuf {
        self.parent.join(format!("roost-ssh-{host_id}"))
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

fn sh_quote(raw: &str) -> String {
    format!("'{}'", raw.replace('\'', r"'\''"))
}

/// The mux warm-up: its remote command is the literal `true`.
fn is_establish(argv: &[String]) -> bool {
    argv.last().is_some_and(|last| last == "true")
}

/// A per-connection exec: its remote command is the bridge one-liner.
fn is_exec(argv: &[String]) -> bool {
    argv.last().is_some_and(|last| *last == remote_command())
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

async fn wait_for(what: &str, mut ready: impl FnMut() -> bool) {
    let scale = std::env::var("ROOST_TEST_TIMEOUT_SCALE")
        .ok()
        .and_then(|value| value.parse::<f64>().ok())
        .filter(|scale| *scale >= 1.0)
        .unwrap_or(1.0);
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

/// A crashed client leaves a directory behind but its `ssh` master
/// outlives it on `ControlPersist`. Reclaiming means exiting that master
/// first — removing its control socket underneath it would strand a
/// process nothing can address any more.
#[tokio::test]
async fn a_stale_directory_is_reclaimed_after_exiting_the_old_master() {
    let harness = Harness::new("ok", "cat");
    let host_id = "00000005";
    let stale = harness.scratch_dir(host_id);
    std::fs::create_dir(&stale).expect("pre-create the stale directory");
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
    assert!(
        !stale.join("ctl").exists(),
        "the reclaimed directory must be recreated fresh"
    );

    tunnel.establish().await.expect("establish after a reclaim");
    let bridge = tunnel.bridge_socket().to_path_buf();
    drop(echo_through(&bridge, b"fresh\n").await);
}

#[tokio::test]
async fn a_live_bridge_socket_refuses_a_second_tunnel() {
    let harness = Harness::new("ok", "cat");
    let host_id = "00000006";
    let occupied = harness.scratch_dir(host_id);
    std::fs::create_dir(&occupied).expect("pre-create the occupied directory");
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

    drop(live);
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

    assert!(!harness.scratch_dir(host_id).exists());
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
    let scratch = harness.scratch_dir(host_id);
    {
        let tunnel = SshTunnel::open(host_id, &ssh_target("workbox"), harness.options())
            .await
            .expect("open");
        tunnel.establish().await.expect("establish");
        assert!(scratch.exists());
    }

    assert!(!scratch.exists(), "Drop must remove the scratch directory");
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
    let (generation, failure) = tunnel.last_error().expect("a recorded failure");
    assert_eq!(generation, 1, "the first exec of this tunnel");
    assert!(
        matches!(failure, SshFailure::Transport(_)),
        "an unclassifiable non-zero exit is a transport failure, got {failure:?}"
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
        &format!("read -r _request; printf '%s\\n' {}", sh_quote(&response)),
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
