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

mod support;

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use roost_ipc::messages::{BridgePreamble, BRIDGE_ACCEPTED};
use roost_ipc::paths::{SESSION_SOCKET_DIR_ENV, TEST_OLD_SESSION_DIR_ENV, TEST_PASSWD_HOME_ENV};
use roost_ipc::{ConnCtx, Handler, HandlerError, HandlerOutcome, IpcServer};
use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

/// The socket `BundleProfile::session()` resolves to when the child is
/// handed `root` as its `ROOST_SESSION_SOCKET_DIR` — on every platform,
/// since the override names the socket's directory itself.
fn session_socket_path(root: &Path) -> PathBuf {
    root.join("roost.sock")
}

/// Point a child's session socket at `root`, under `ROOST_TEST_MODE=1`:
/// a fixture that lost its override then fails closed on the product's
/// own guard instead of dialing this user's real session socket.
fn root_env(command: &mut Command, root: &Path) {
    command
        .env(SESSION_SOCKET_DIR_ENV, root)
        .env("ROOST_TEST_MODE", "1")
        .env_remove(TEST_PASSWD_HOME_ENV)
        .env_remove(TEST_OLD_SESSION_DIR_ENV);
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

    /// The bridge's connection, past its preamble: what is left on it is
    /// exactly what the client and the session say.
    async fn accept(&self) -> UnixStream {
        let (mut stream, _) = self.listener.accept().await.expect("accept the bridge");
        let preamble = read_line(&mut stream).await;
        assert!(
            serde_json::from_slice::<BridgePreamble>(&preamble).is_ok(),
            "the bridge opens with its preamble: {:?}",
            String::from_utf8_lossy(&preamble)
        );
        stream
            .write_all(format!("{BRIDGE_ACCEPTED}\n").as_bytes())
            .await
            .expect("ack the preamble");
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

/// One line, read a byte at a time so nothing behind it is consumed.
async fn read_line(stream: &mut UnixStream) -> Vec<u8> {
    let mut line = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        let read = stream.read(&mut byte).await.expect("read a line");
        assert_ne!(read, 0, "EOF before a full line: {line:?}");
        if byte[0] == b'\n' {
            return line;
        }
        line.push(byte[0]);
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

/// A bridge whose own socket is empty, and whose one old-path candidate
/// (`ROOST_TEST_OLD_SESSION_DIR`, a debug build's) is `old`.
fn spawn_fallback_bridge(root: &Path, old: &Path) -> Child {
    let mut command = Command::new(env!("CARGO_BIN_EXE_roost-session"));
    command.arg("client-bridge");
    root_env(&mut command, root);
    command
        .env(TEST_OLD_SESSION_DIR_ENV, old)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    command.spawn().expect("spawn the client bridge")
}

async fn read_reply(stdout: &mut tokio::io::BufReader<ChildStdout>) -> serde_json::Value {
    use tokio::io::AsyncBufReadExt;
    let mut line = String::new();
    stdout.read_line(&mut line).await.expect("read a reply");
    serde_json::from_str(&line).unwrap_or_else(|_| panic!("a JSON reply: {line:?}"))
}

/// Plan 083 A-3: with nothing at this build's socket and a session on an
/// old path, the bridge reaches it as a restricted proxy — identify and
/// stop pass, an attach (or anything else) is answered by the bridge and
/// never reaches that session.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn with_nothing_here_the_bridge_proxies_only_identify_and_stop_to_an_old_path() {
    if !cfg!(debug_assertions) {
        eprintln!("skip: the old-session test candidate is honoured by debug builds only");
        return;
    }
    let root = root_dir();
    let old = root.path().join("old");
    std::fs::create_dir(&old).unwrap();
    std::fs::set_permissions(&old, std::fs::Permissions::from_mode(0o700)).unwrap();
    let seen = support::old_path_sentinel(&old.join("roost.sock"));
    let mut child = spawn_fallback_bridge(&root.path().join("empty"), &old);
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = tokio::io::BufReader::new(child.stdout.take().unwrap());

    stdin
        .write_all(b"{\"id\":\"1\",\"op\":\"session.identify\",\"params\":{}}\n")
        .await
        .unwrap();
    let identified = read_reply(&mut stdout).await;
    assert_eq!(identified["result"]["session_id"], "old-1", "{identified}");

    for op in ["tab.attach", "events.subscribe", "tab.list"] {
        stdin
            .write_all(format!("{{\"id\":\"2\",\"op\":\"{op}\",\"params\":{{}}}}\n").as_bytes())
            .await
            .unwrap();
        let refused = read_reply(&mut stdout).await;
        assert_eq!(refused["ok"], false, "{op}: {refused}");
        let message = refused["error"]["message"].as_str().unwrap();
        assert!(
            message.contains(&old.join("roost.sock").display().to_string()),
            "{message}"
        );
    }
    stdin.write_all(b"not json\n").await.unwrap();
    assert_eq!(read_reply(&mut stdout).await["ok"], false);

    stdin
        .write_all(b"{\"id\":\"3\",\"op\":\"session.stop\",\"params\":{}}\n")
        .await
        .unwrap();
    assert_eq!(read_reply(&mut stdout).await["ok"], true);

    // Plan 084 D2: the restricted proxy never introduces itself.
    assert!(
        seen.lock()
            .unwrap()
            .iter()
            .all(|line| serde_json::from_str::<BridgePreamble>(line).is_err()),
        "no preamble reached the old session: {:?}",
        seen.lock().unwrap()
    );
    let ops: Vec<String> = seen
        .lock()
        .unwrap()
        .iter()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap()["op"].to_string())
        .collect();
    // The discovery probe's own identify, on a connection of its own,
    // is the other line here.
    assert!(
        ops.iter()
            .all(|op| op == "\"session.identify\"" || op == "\"session.stop\""),
        "only the two forwarded ops reached the old session: {ops:?}"
    );
    assert_eq!(
        ops.last().map(String::as_str),
        Some("\"session.stop\""),
        "{ops:?}"
    );
    drop(stdin);
    let _ = child.kill().await;
}

/// The fallback changes nothing when no old path answers either: the
/// bridge still fails with the exact prefix the client matches on.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn with_no_session_anywhere_the_bridge_still_says_no_session() {
    let root = root_dir();
    let old = root.path().join("old");
    std::fs::create_dir(&old).unwrap();
    let output = spawn_fallback_bridge(root.path(), &old)
        .wait_with_output()
        .await
        .expect("reap the failed bridge");
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
        stderr.starts_with("client-bridge: no session is listening at "),
        "{stderr:?}"
    );
}

/// A session that answers every request with the origin its connection
/// arrived with, and counts its connections.
struct OriginEcho {
    conns: Arc<AtomicUsize>,
}

impl Handler for OriginEcho {
    fn handle<'a>(
        &'a self,
        ctx: &'a ConnCtx,
        op: &'a str,
        _params: serde_json::Value,
    ) -> Pin<Box<dyn Future<Output = Result<HandlerOutcome, HandlerError>> + Send + 'a>> {
        let reply = serde_json::json!({
            "op": op,
            "bridge": serde_json::to_value(&ctx.bridge).unwrap(),
        });
        Box::pin(async move { Ok(HandlerOutcome::Reply(reply)) })
    }

    fn connection_ended(&self, _conn_id: u64) {
        self.conns.fetch_add(1, Ordering::AcqRel);
    }
}

/// An [`OriginEcho`] session at the bridge's socket under `root`, with or
/// without the preamble — without it is a session from before the
/// preamble existed. Returns the count of connections it has finished.
async fn origin_echo(root: &Path, preamble: bool) -> Arc<AtomicUsize> {
    let conns = Arc::new(AtomicUsize::new(0));
    let server = IpcServer::bind(
        session_socket_path(root),
        OriginEcho {
            conns: Arc::clone(&conns),
        },
    )
    .await
    .expect("bind the echo session");
    let server = if preamble {
        server.accept_bridge_preamble()
    } else {
        server
    };
    tokio::spawn(server.run());
    conns
}

fn spawn_bridge_from(root: &Path, ssh_connection: &str) -> Child {
    spawn_bridge_with(root, ssh_connection, None)
}

fn spawn_bridge_with(root: &Path, ssh_connection: &str, ssh_client: Option<&str>) -> Child {
    let mut command = Command::new(env!("CARGO_BIN_EXE_roost-session"));
    command.arg("client-bridge");
    root_env(&mut command, root);
    command.env("SSH_CONNECTION", ssh_connection);
    match ssh_client {
        Some(value) => command.env("SSH_CLIENT", value),
        None => command.env_remove("SSH_CLIENT"),
    };
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    command.spawn().expect("spawn the client bridge")
}

/// Long enough for the bridge's own 2 s preamble budget and a fresh
/// dial; what it bounds is a bridge that never answers at all.
fn bridge_budget() -> Duration {
    support::scaled(Duration::from_secs(20))
}

/// One request through the bridge, and every line stdout carries until
/// the bridge exits.
async fn one_request(mut child: Child) -> (Vec<serde_json::Value>, ExitStatus) {
    let mut stdin = child.stdin.take().unwrap();
    stdin
        .write_all(b"{\"id\":\"7\",\"op\":\"session.identify\",\"params\":{}}\n")
        .await
        .unwrap();
    drop(stdin);
    let output = tokio::time::timeout(bridge_budget(), child.wait_with_output())
        .await
        .expect("the bridge answers and exits")
        .expect("reap the bridge");
    let lines = output
        .stdout
        .split(|b| *b == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| {
            serde_json::from_slice(line)
                .unwrap_or_else(|_| panic!("a JSON line: {:?}", String::from_utf8_lossy(line)))
        })
        .collect();
    (lines, output.status)
}

/// The session's count of finished connections, once it reaches `want`
/// or the budget runs out. A connection is counted after its socket has
/// closed, so a bridge can exit a moment before its own is.
async fn finished(conns: &AtomicUsize, want: usize) -> usize {
    let deadline = tokio::time::Instant::now() + bridge_budget();
    while conns.load(Ordering::Acquire) < want && tokio::time::Instant::now() < deadline {
        support::tick().await;
    }
    conns.load(Ordering::Acquire)
}

/// Plan 084 D2: a session that takes the preamble learns the bridge's
/// origin, and the client sees only its own reply — the ack is the
/// bridge's to read.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_session_that_accepts_the_preamble_sees_the_bridged_origin() {
    let root = root_dir();
    let conns = origin_echo(root.path(), true).await;

    let (lines, status) =
        one_request(spawn_bridge_from(root.path(), "10.0.0.1 5000 10.0.0.2 22")).await;

    assert_eq!(status.code(), Some(0));
    assert_eq!(lines.len(), 1, "only the reply reaches stdout: {lines:?}");
    assert_eq!(lines[0]["id"], "7", "{lines:?}");
    assert_eq!(lines[0]["ok"], true, "{lines:?}");
    assert_eq!(
        lines[0]["result"]["bridge"],
        serde_json::json!({"ssh_connection": "10.0.0.1 5000 10.0.0.2 22", "ssh_client": null}),
        "{lines:?}"
    );
    assert_eq!(finished(&conns, 1).await, 1, "one connection, relayed");
}

/// Plan 085 D2: an `SSH_CONNECTION` over 256 bytes reaches the session
/// as unset, the way the starter and roostctl collect it, while a valid
/// `SSH_CLIENT` beside it still does.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_oversized_origin_value_reaches_the_session_unset() {
    let root = root_dir();
    let _conns = origin_echo(root.path(), true).await;

    let (lines, status) = one_request(spawn_bridge_with(
        root.path(),
        &"1".repeat(257),
        Some("10.0.0.1 5000 22"),
    ))
    .await;

    assert_eq!(status.code(), Some(0));
    assert_eq!(
        lines[0]["result"]["bridge"],
        serde_json::json!({"ssh_connection": null, "ssh_client": "10.0.0.1 5000 22"}),
        "{lines:?}"
    );
}

/// Plan 084 D2: a session from before the preamble answers it as a
/// malformed request. The bridge dials again and relays with none, so
/// the client's first request still gets its own reply.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_session_from_before_the_preamble_gets_a_fresh_connection_without_one() {
    let root = root_dir();
    let conns = origin_echo(root.path(), false).await;

    let (lines, status) = one_request(spawn_bridge_from(root.path(), "10.0.0.1 5000")).await;

    assert_eq!(status.code(), Some(0));
    assert_eq!(
        lines.len(),
        1,
        "neither the refusal nor an ack reaches stdout: {lines:?}"
    );
    assert_eq!(lines[0]["id"], "7", "{lines:?}");
    assert_eq!(lines[0]["ok"], true, "{lines:?}");
    assert_eq!(
        lines[0]["result"]["bridge"],
        serde_json::Value::Null,
        "the relayed connection carried no preamble: {lines:?}"
    );
    assert_eq!(
        finished(&conns, 2).await,
        2,
        "the refused connection, then the relayed one"
    );
}

/// Plan 084 D2: a session that never answers the preamble is given up
/// on after the budget, and the client is relayed on a fresh connection.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_session_silent_after_the_preamble_gets_a_fresh_connection() {
    let root = root_dir();
    let listener =
        UnixListener::bind(session_socket_path(root.path())).expect("bind the silent session");
    let child = spawn_bridge_from(root.path(), "10.0.0.1 5000");

    let (mut silent, _) = listener.accept().await.unwrap();
    let preamble = tokio::time::timeout(bridge_budget(), read_line(&mut silent))
        .await
        .expect("the bridge writes its preamble");
    assert!(serde_json::from_slice::<BridgePreamble>(&preamble).is_ok());

    let serve = tokio::spawn(async move {
        let (mut relayed, _) = listener.accept().await.unwrap();
        let request = read_line(&mut relayed).await;
        relayed
            .write_all(b"{\"id\":\"7\",\"ok\":true,\"result\":{}}\n")
            .await
            .unwrap();
        request
    });
    let (lines, status) = one_request(child).await;
    let request = tokio::time::timeout(bridge_budget(), serve)
        .await
        .expect("the bridge dialed again")
        .unwrap();

    assert_eq!(status.code(), Some(0));
    let request: serde_json::Value = serde_json::from_slice(&request).unwrap();
    assert_eq!(
        request["op"], "session.identify",
        "no preamble on the retry"
    );
    assert_eq!(
        lines,
        vec![serde_json::json!({"id": "7", "ok": true, "result": {}})]
    );
    drop(silent);
}
