//! The far-side bridge, driven as a real process over real pipes.
//!
//! This is the transport lane the architecture calls CI-able without
//! sshd: `roost-session client-bridge` is exec'd with piped stdio — the
//! same shape `ssh -T` gives it — against a fake session listening on
//! the path its own environment resolves. What is asserted is only what
//! a transport owes: bytes in equals bytes out, and every EOF in the
//! four-quadrant matrix ends the right half of the connection.
//!
//! Nothing here sleeps. Every wait is a read that cannot complete until
//! the bridge has done the thing being tested.

use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};

use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

/// The socket `BundleProfile::session()` resolves to when the child's
/// environment is rooted at `root`. A deliberate mirror of
/// `roost_ipc::paths` for the one profile this lane drives — the
/// resolver itself is pinned by that crate's own golden tests, and the
/// alternative here is mutating process-global env that every other test
/// in this binary also reads.
fn session_socket_path(root: &Path) -> PathBuf {
    #[cfg(target_os = "macos")]
    {
        let label = if cfg!(debug_assertions) {
            "RoostSessionDev"
        } else {
            "RoostSession"
        };
        root.join("Library/Caches").join(label).join("roost.sock")
    }
    #[cfg(not(target_os = "macos"))]
    {
        let namespace = if cfg!(debug_assertions) {
            "roost-session-dev"
        } else {
            "roost-session"
        };
        root.join(namespace).join("roost.sock")
    }
}

/// Point a child's path resolution at `root`: HOME on macOS,
/// `XDG_RUNTIME_DIR` on Linux — the inputs each platform's resolver
/// reads for the socket.
fn root_env(command: &mut Command, root: &Path) {
    #[cfg(target_os = "macos")]
    command.env("HOME", root);
    #[cfg(not(target_os = "macos"))]
    command.env("XDG_RUNTIME_DIR", root);
}

/// A root under `/tmp` rather than `$TMPDIR`: macOS hands out deep
/// per-user temp paths, and the socket path built under one can exceed
/// `sun_path`'s 104 bytes.
fn root_dir() -> TempDir {
    tempfile::Builder::new()
        .prefix("roost-bridge-")
        .tempdir_in("/tmp")
        .expect("tempdir")
}

fn spawn_bridge(root: &Path) -> Child {
    let mut command = Command::new(env!("CARGO_BIN_EXE_roost-session"));
    command.arg("client-bridge");
    root_env(&mut command, root);
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    command.spawn().expect("spawn the client bridge")
}

/// A fake session listening where the bridge will dial, plus the bridge
/// process itself with its stdio in hand.
struct Fixture {
    _root: TempDir,
    listener: UnixListener,
    child: Child,
    /// Held out of `child` on purpose: `Child::wait` closes the child's
    /// own `stdin` before waiting, which would half-close the bridge
    /// underneath the very tests that assert it exits with stdin open.
    stdin: Option<ChildStdin>,
    stdout: Option<ChildStdout>,
}

impl Fixture {
    /// Bind first, then spawn, so the bridge's connect cannot lose a
    /// race with the listener.
    fn start() -> Self {
        let root = root_dir();
        let socket_path = session_socket_path(root.path());
        std::fs::create_dir_all(socket_path.parent().expect("socket dir"))
            .expect("create the socket dir");
        let listener = UnixListener::bind(&socket_path).expect("bind the fake session");
        let mut child = spawn_bridge(root.path());
        let stdin = child.stdin.take().expect("child stdin");
        let stdout = child.stdout.take().expect("child stdout");
        Self {
            _root: root,
            listener,
            child,
            stdin: Some(stdin),
            stdout: Some(stdout),
        }
    }

    async fn accept(&self) -> UnixStream {
        let (stream, _) = self.listener.accept().await.expect("accept the bridge");
        stream
    }

    fn stdin(&mut self) -> &mut ChildStdin {
        self.stdin.as_mut().expect("child stdin is still open")
    }

    fn stdout(&mut self) -> &mut ChildStdout {
        self.stdout.as_mut().expect("child stdout is not drained")
    }

    fn close_stdin(&mut self) {
        self.stdin = None;
    }

    /// Read stdout to EOF from its own task. A payload larger than the
    /// stdout pipe would otherwise deadlock: the bridge parks in a write
    /// nobody is reading, stops draining the socket, and the test's own
    /// `write_all` never returns.
    fn drain(&mut self) -> tokio::task::JoinHandle<Vec<u8>> {
        let mut stdout = self.stdout.take().expect("stdout is not drained twice");
        tokio::spawn(async move {
            let mut seen = Vec::new();
            stdout
                .read_to_end(&mut seen)
                .await
                .expect("drain the bridge's stdout");
            seen
        })
    }

    async fn wait(mut self) -> ExitStatus {
        self.child.wait().await.expect("reap the bridge")
    }

    /// Drain stdout, then reap. Holding stdin open until the process is
    /// gone is deliberate in the tests that use it: an events
    /// connection's write half never closes.
    async fn finish(mut self) -> (Vec<u8>, ExitStatus) {
        let trailing = self.drain().await.expect("the drain task");
        (trailing, self.wait().await)
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_bridge_carries_bytes_through_unchanged_in_both_directions() {
    let mut fixture = Fixture::start();
    let mut server = fixture.accept().await;

    // A handshake line and binary residue in ONE write: a framing-aware
    // pump would split here, and the client would never see the tail.
    let mut down = br#"{"op":"session.identify","id":1}"#.to_vec();
    down.push(b'\n');
    down.extend_from_slice(&[0x00, 0xff, 0x01, 0xfe, b'\n', 0x00, 0x80]);
    server.write_all(&down).await.expect("server writes");

    let mut seen = vec![0u8; down.len()];
    fixture
        .stdout()
        .read_exact(&mut seen)
        .await
        .expect("read the bridge's stdout");
    assert_eq!(seen, down, "socket→stdout must be byte-exact");

    // Upstream in two writes: the bridge may coalesce them, but the
    // bytes must arrive in order and untouched.
    let head = [0x00u8, 0xff, 0x7f, 0x80];
    let tail: Vec<u8> = (0..=255u8).collect();
    fixture.stdin().write_all(&head).await.expect("write head");
    fixture.stdin().write_all(&tail).await.expect("write tail");

    let mut seen = vec![0u8; head.len() + tail.len()];
    server
        .read_exact(&mut seen)
        .await
        .expect("server reads what the client wrote");
    assert_eq!(
        &seen[..head.len()],
        &head,
        "stdin→socket must be byte-exact"
    );
    assert_eq!(&seen[head.len()..], &tail[..], "and must not reorder");

    drop(server);
    let (trailing, status) = fixture.finish().await;
    assert!(trailing.is_empty(), "no bytes after the socket closed");
    assert_eq!(status.code(), Some(0));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stdin_eof_half_closes_the_socket_while_the_session_keeps_talking() {
    let mut fixture = Fixture::start();
    let mut server = fixture.accept().await;

    fixture.close_stdin();

    let mut probe = [0u8; 1];
    let read = server.read(&mut probe).await.expect("server reads");
    assert_eq!(read, 0, "stdin EOF must shut the socket's write side down");

    // The session's own direction is untouched by that half-close.
    server.write_all(b"after-eof").await.expect("server writes");
    let mut seen = [0u8; 9];
    fixture
        .stdout()
        .read_exact(&mut seen)
        .await
        .expect("bytes written after the half-close still arrive");
    assert_eq!(&seen, b"after-eof");

    drop(server);
    let (trailing, status) = fixture.finish().await;
    assert!(trailing.is_empty());
    assert_eq!(status.code(), Some(0));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_closed_socket_ends_the_bridge_even_with_stdin_still_open() {
    let fixture = Fixture::start();
    let server = fixture.accept().await;

    drop(server);

    // `finish` never touches stdin, so the bridge exits with its write
    // half still open — the case that would hang a pump waiting on both.
    let (trailing, status) = fixture.finish().await;
    assert!(trailing.is_empty());
    assert_eq!(status.code(), Some(0));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_held_open_write_half_still_receives_everything_before_the_close() {
    let mut fixture = Fixture::start();
    let mut server = fixture.accept().await;

    // Bigger than any single read, so the answer depends on the pump
    // looping rather than on one lucky syscall.
    let payload: Vec<u8> = (0..128 * 1024usize).map(|i| (i % 251) as u8).collect();
    let drain = fixture.drain();
    server.write_all(&payload).await.expect("server writes");
    drop(server);

    let seen = drain.await.expect("the drain task");
    assert_eq!(seen.len(), payload.len(), "nothing may be dropped");
    assert_eq!(seen, payload, "and nothing may be reordered");
    assert_eq!(fixture.wait().await.code(), Some(0));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_session_fails_on_stderr_with_the_hint_and_an_empty_stdout() {
    let root = root_dir();
    let socket_path = session_socket_path(root.path());
    let child = spawn_bridge(root.path());

    let output = child
        .wait_with_output()
        .await
        .expect("reap the failed bridge");

    // Exit 1 exactly: 255 is ssh's own transport-failure code, and the
    // client classifier tells the two apart.
    assert_eq!(
        output.status.code(),
        Some(1),
        "a bridge with nowhere to dial"
    );
    assert!(
        output.stdout.is_empty(),
        "stdout is the wire and carries nothing else: {:?}",
        String::from_utf8_lossy(&output.stdout)
    );
    let stderr = String::from_utf8(output.stderr).expect("stderr is utf-8");
    assert_eq!(stderr.lines().count(), 1, "exactly one line: {stderr:?}");
    // The prefix the client's classifier matches on.
    assert!(
        stderr.starts_with("client-bridge: no session is listening at "),
        "{stderr:?}"
    );
    assert!(
        stderr.contains(&socket_path.display().to_string()),
        "{stderr:?}"
    );
    assert!(stderr.contains("roostctl session start"), "{stderr:?}");
}
