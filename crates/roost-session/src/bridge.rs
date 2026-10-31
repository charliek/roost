//! The far side of the SSH transport: stdio ↔ the session socket.
//!
//! `roost-session client-bridge` is what a client execs on a host —
//! `ssh -T host 'exec roost-session client-bridge'`, one per accepted
//! connection. It dials this machine's session socket and pumps bytes
//! between it and its own stdio, so the client's local UDS connection
//! reaches the far-side session with no remote socket path to manage.
//!
//! Two properties make it a *transport* rather than a protocol
//! participant:
//!
//! * **Pure byte pump.** No framing, no line buffering. A JSON
//!   handshake line and the binary residue behind it can land in one
//!   read, and both sides must see the bytes exactly as they were
//!   written.
//! * **stdout is the wire.** Nothing else may ever be written there —
//!   not a readiness line, not a log. Failures go to stderr, one line,
//!   prefixed `client-bridge: `, and the process exits non-zero.
//!
//! Half-close is explicit in both directions: stdin EOF shuts the
//! socket's write side down so the session reads a clean EOF, and a
//! socket EOF ends the process without waiting on stdin — the client
//! holds its write half open for the whole life of an events
//! connection, so waiting for it would strand the bridge.
//!
//! # A session on an old path
//!
//! When nothing listens at this build's socket, a session still on a
//! path an older build used (`roost_ipc::old_session`) is the likely
//! reason — this binary was just installed over a running daemon. The
//! bridge then reaches it as a **restricted proxy**: it forwards a
//! request line only when its op is `session.identify` or
//! `session.stop`, and answers any other op itself, naming the old path
//! and the fix. That is all a client needs to see what is there and
//! restart it (an older generation's needs-restart flow), and it can
//! never attach to it — a same-generation daemon on an old path is
//! refused, not adopted. Whether to fall back is decided by `connect()`
//! alone, before a byte reaches stdout; nothing answering anywhere keeps
//! the `client-bridge: no session` stderr prefix the client matches.
//!
//! # The preamble
//!
//! On the full pump (never the restricted proxy) the bridge first
//! writes one line of its own toward the session — a
//! [`BridgePreamble`] carrying its `SSH_CONNECTION` and `SSH_CLIENT` —
//! and reads one line back itself, so the session can tell a bridged
//! connection from a local one. Only [`BRIDGE_ACCEPTED`] lets that
//! connection be relayed; anything else is a session that predates the
//! preamble, and the bridge dials again and relays with none. Neither
//! line ever reaches stdout.

use std::ffi::OsString;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context};
use roost_ipc::codes;
use roost_ipc::framing::FrameReader;
use roost_ipc::messages::{
    ops, BridgeOrigin, BridgePreamble, RawRequest, BRIDGE_ACCEPTED, MAX_BRIDGE_PREAMBLE,
};
use roost_ipc::old_session::{self, OldSession};
use roost_ipc::opener::remote_value_from;
use roost_ipc::paths::BundleProfile;
use roost_ipc::socket_state::{classify_connect_error, SocketState};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::UnixStream;
use tokio::sync::Mutex;

/// One read's worth of wire bytes. Large enough that a snapshot stream
/// is not chopped into a syscall per frame, and a match for the default
/// pipe buffer on the stdio side.
const CHUNK: usize = 64 * 1024;

/// How long a session has to answer the preamble before the bridge
/// takes it for one that predates it.
const PREAMBLE_ACK_BUDGET: Duration = Duration::from_secs(2);

/// Owns the reporting rather than returning an error, because the
/// caller has no other channel: stdout belongs to the wire.
pub fn run() -> i32 {
    match connect_and_pump() {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("client-bridge: {error:#}");
            // Not 255: that is `ssh`'s own "the transport failed" code,
            // and the client has to tell the two apart.
            1
        }
    }
}

fn connect_and_pump() -> anyhow::Result<()> {
    let profile = BundleProfile::session().context("resolve the session bundle profile")?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("build the bridge runtime")?;
    let outcome = runtime.block_on(async {
        match connect(&profile.socket_path).await {
            Ok(socket) => pump(announce(socket, &profile.socket_path).await?).await,
            Err(Unreachable::Nothing(error)) => {
                match old_path_session(&profile.socket_path).await {
                    Some((socket, old)) => restricted_pump(socket, &old).await,
                    None => Err(error),
                }
            }
            Err(Unreachable::Other(error)) => Err(error),
        }
    });
    // Never dropped: `tokio::io::stdin` reads on a blocking thread that
    // can still be parked in `read(2)` when the socket side finishes,
    // and dropping the runtime would wait for it forever.
    runtime.shutdown_background();
    outcome
}

/// Why this build's socket did not connect: nothing listening there,
/// which is when an old path is worth trying, or anything else.
enum Unreachable {
    Nothing(anyhow::Error),
    Other(anyhow::Error),
}

async fn connect(socket_path: &Path) -> Result<UnixStream, Unreachable> {
    UnixStream::connect(socket_path).await.map_err(|error| {
        // The hint is a contract, not just prose: the client classifies
        // a failed bridge by matching `client-bridge: no session` on
        // stderr.
        let hint = format!(
            "no session is listening at {}; run 'roostctl session start' on this machine",
            socket_path.display()
        );
        // The same errno rule the rest of the workspace decides
        // "nothing is listening" by. Anything it cannot call — a
        // permission problem, a full accept backlog — reads the same to
        // the classifier but not to the human, so it keeps its detail.
        match classify_connect_error(&error) {
            SocketState::Missing | SocketState::Stale => Unreachable::Nothing(anyhow!("{hint}")),
            _ => Unreachable::Other(anyhow!("{hint} ({error})")),
        }
    })
}

/// Introduce `socket` with the preamble, and hand back the connection to
/// relay: `socket` itself once the session accepts, otherwise a fresh
/// one that carries no preamble — a session from before the preamble
/// existed answers it as a malformed request, and the client must never
/// see that answer.
async fn announce(mut socket: UnixStream, socket_path: &Path) -> anyhow::Result<UnixStream> {
    let line = preamble_line(&std::env::vars_os().collect::<Vec<_>>());
    if accepted(&mut socket, &line).await {
        return Ok(socket);
    }
    drop(socket);
    connect(socket_path)
        .await
        .map_err(|(Unreachable::Nothing(error) | Unreachable::Other(error))| error)
}

/// The preamble line for `env`, newline included. Each value is collected
/// as every other opener's is, so a value too long or not UTF-8 is left
/// out rather than cut into an identity no other client would send.
fn preamble_line(env: &[(OsString, OsString)]) -> Vec<u8> {
    let preamble = BridgePreamble {
        bridge: BridgeOrigin {
            ssh_connection: remote_value_from(env, "SSH_CONNECTION"),
            ssh_client: remote_value_from(env, "SSH_CLIENT"),
        },
    };
    let mut line = serde_json::to_vec(&preamble).expect("a preamble serializes");
    line.push(b'\n');
    line
}

/// Write the preamble and read exactly one line back, within
/// [`PREAMBLE_ACK_BUDGET`]. Byte by byte, because whatever follows that
/// line belongs to the client.
async fn accepted(socket: &mut UnixStream, line: &[u8]) -> bool {
    let exchange = async {
        socket.write_all(line).await.ok()?;
        let mut reply = Vec::new();
        let mut byte = [0u8; 1];
        loop {
            if socket.read(&mut byte).await.ok()? == 0 {
                return None;
            }
            if byte[0] == b'\n' {
                return Some(reply == BRIDGE_ACCEPTED.as_bytes());
            }
            if reply.len() >= MAX_BRIDGE_PREAMBLE {
                return None;
            }
            reply.push(byte[0]);
        }
    };
    matches!(
        tokio::time::timeout(PREAMBLE_ACK_BUDGET, exchange).await,
        Ok(Some(true))
    )
}

/// The first session found on an old path, connected.
async fn old_path_session(new_socket: &Path) -> Option<(UnixStream, std::path::PathBuf)> {
    for old in old_session::discover(new_socket).await {
        if let OldSession::Found { socket, .. } = old {
            if let Ok(stream) = UnixStream::connect(&socket).await {
                return Some((stream, socket));
            }
        }
    }
    None
}

/// The ops a bridge to an old path forwards: enough to see what is
/// there and to stop it, nothing that attaches.
const OLD_PATH_OPS: [&str; 2] = [ops::SESSION_IDENTIFY, ops::SESSION_STOP];

/// What the restricted proxy does with one request line.
#[derive(Debug, PartialEq, Eq)]
enum Admit {
    Forward,
    /// Answered here, never forwarded: the reply line, newline excluded.
    Refuse(Vec<u8>),
}

/// The restricted proxy's one decision, on the envelope decoded the way
/// a session decodes it ([`RawRequest`]: an `id`, a string `op`, no
/// duplicate or unknown keys). A line that does not decode is refused
/// like an op this does not forward; an admitted one is forwarded as the
/// bytes that came in.
fn admit(line: &[u8], old_socket: &Path) -> Admit {
    let request: Option<RawRequest> = serde_json::from_slice(line).ok();
    let op = request.as_ref().map(|request| request.op.as_str());
    if op.is_some_and(|op| OLD_PATH_OPS.contains(&op)) {
        return Admit::Forward;
    }
    let id = request
        .as_ref()
        .map_or_else(|| "0".to_string(), |request| request.id.to_string());
    let (code, message) = match op {
        Some(op) => (
            codes::NOT_SUPPORTED,
            format!(
                "{op} refused: the only session on this machine is at an old socket path, {}, \
                 which this roost-session no longer uses; restart it there with 'roostctl \
                 session stop --old-path {}' and 'roostctl session start'",
                old_socket.display(),
                old_socket.display()
            ),
        ),
        None => (
            codes::PARSE_ERROR,
            "not a request this bridge can read; nothing was forwarded".to_string(),
        ),
    };
    let reply = serde_json::json!({
        "id": id,
        "ok": false,
        "error": { "code": code, "message": message },
    });
    Admit::Refuse(serde_json::to_vec(&reply).expect("a JSON value serializes"))
}

/// [`pump`] for a session on an old path, line by line: stdin lines go
/// through [`admit`], socket lines come back whole. Whole lines, because
/// a refusal written here must never land in the middle of one the
/// session is sending.
async fn restricted_pump(socket: UnixStream, old_socket: &Path) -> anyhow::Result<()> {
    let (socket_read, mut socket_write) = socket.into_split();
    let stdout = Arc::new(Mutex::new(tokio::io::stdout()));
    let old_socket = old_socket.to_path_buf();
    let replies = Arc::clone(&stdout);
    let mut upstream = tokio::spawn(async move {
        let mut stdin = FrameReader::new(tokio::io::stdin());
        while let Some(line) = stdin.read_line().await.context("read stdin")? {
            match admit(&line, &old_socket) {
                Admit::Forward => {
                    socket_write
                        .write_all(&[line.as_slice(), b"\n"].concat())
                        .await
                        .context("write to the session socket")?;
                }
                Admit::Refuse(reply) => write_line(&replies, &reply).await?,
            }
        }
        socket_write
            .shutdown()
            .await
            .context("half-close the session socket")
    });
    let downstream = async {
        let mut lines = FrameReader::new(socket_read);
        while let Some(line) = lines.read_line().await.context("read the session socket")? {
            write_line(&stdout, &line).await?;
        }
        anyhow::Ok(())
    };
    tokio::pin!(downstream);
    tokio::select! {
        result = &mut downstream => {
            upstream.abort();
            result
        }
        joined = &mut upstream => {
            joined.context("the stdin pump panicked")??;
            downstream.await
        }
    }
}

async fn write_line(stdout: &Mutex<tokio::io::Stdout>, line: &[u8]) -> anyhow::Result<()> {
    let mut stdout = stdout.lock().await;
    stdout
        .write_all(&[line, b"\n"].concat())
        .await
        .context("write stdout")?;
    stdout.flush().await.context("flush stdout")
}

/// The socket→stdout direction owns the exit; stdin's clean end is only
/// a half-close.
async fn pump(socket: UnixStream) -> anyhow::Result<()> {
    let (socket_read, socket_write) = socket.into_split();
    let upstream = tokio::spawn(stdin_to_socket(socket_write));
    let downstream = socket_to_stdout(socket_read);
    tokio::pin!(downstream);

    tokio::select! {
        result = &mut downstream => result,
        joined = upstream => {
            joined.context("the stdin pump panicked")??;
            // stdin EOF is a half-close, not an exit: the session may
            // still have plenty to send.
            downstream.await
        }
    }
}

async fn stdin_to_socket(mut socket: OwnedWriteHalf) -> anyhow::Result<()> {
    let mut stdin = tokio::io::stdin();
    let mut buf = vec![0u8; CHUNK];
    loop {
        let read = stdin.read(&mut buf).await.context("read stdin")?;
        if read == 0 {
            break;
        }
        socket
            .write_all(&buf[..read])
            .await
            .context("write to the session socket")?;
    }
    // The session must see a real EOF on its read half, not a connection
    // that merely went quiet — that is how a client's own half-close
    // reaches it through the bridge.
    socket
        .shutdown()
        .await
        .context("half-close the session socket")
}

async fn socket_to_stdout(mut socket: OwnedReadHalf) -> anyhow::Result<()> {
    let mut stdout = tokio::io::stdout();
    let mut buf = vec![0u8; CHUNK];
    loop {
        let read = socket
            .read(&mut buf)
            .await
            .context("read the session socket")?;
        if read == 0 {
            break;
        }
        stdout
            .write_all(&buf[..read])
            .await
            .context("write stdout")?;
        // Wire bytes carry no newlines to lean on, and stdout is line
        // buffered: without this a whole frame can sit in the buffer.
        stdout.flush().await.context("flush stdout")?;
    }
    stdout.flush().await.context("flush stdout")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn refused(line: &[u8]) -> serde_json::Value {
        match admit(line, Path::new("/tmp/old/roost.sock")) {
            Admit::Refuse(reply) => serde_json::from_slice(&reply).expect("a JSON reply"),
            Admit::Forward => panic!("{} was forwarded", String::from_utf8_lossy(line)),
        }
    }

    #[test]
    fn identify_and_stop_are_forwarded() {
        for op in ["session.identify", "session.stop"] {
            let line = format!(r#"{{"id":"1","op":"{op}","params":{{}}}}"#);
            assert_eq!(
                admit(line.as_bytes(), Path::new("/tmp/old/roost.sock")),
                Admit::Forward,
                "{op}"
            );
        }
    }

    /// Plan 083 R6: anything else — an attach, a subscribe, a line that
    /// is not JSON — is answered here and never reaches the session.
    #[test]
    fn every_other_line_is_refused_naming_the_old_path() {
        for op in [
            "tab.list",
            "events.subscribe",
            "session.set_theme",
            "tab.attach",
        ] {
            let reply = refused(format!(r#"{{"id":"7","op":"{op}","params":{{}}}}"#).as_bytes());
            assert_eq!(reply["id"], "7", "{op}");
            assert_eq!(reply["ok"], false, "{op}");
            assert_eq!(reply["error"]["code"], codes::NOT_SUPPORTED, "{op}");
            let message = reply["error"]["message"].as_str().unwrap();
            assert!(message.contains("/tmp/old/roost.sock"), "{message}");
            assert!(message.contains("--old-path"), "{message}");
        }
        for line in [&b"not json"[..], br#"{"id":"3"}"#, br#"["session.stop"]"#] {
            let reply = refused(line);
            assert_eq!(reply["ok"], false);
        }
        assert_eq!(refused(b"garbage")["error"]["code"], codes::PARSE_ERROR);
    }

    fn env(pairs: &[(&str, &str)]) -> Vec<(OsString, OsString)> {
        pairs
            .iter()
            .map(|(name, value)| (OsString::from(name), OsString::from(value)))
            .collect()
    }

    fn origin(line: &[u8]) -> serde_json::Value {
        serde_json::from_slice::<serde_json::Value>(line).unwrap()["bridge"].clone()
    }

    /// Plan 084 D2: the preamble is one line naming both values, null
    /// when unset.
    #[test]
    fn the_preamble_is_one_line_with_both_origin_values() {
        let line = preamble_line(&env(&[
            ("SSH_CONNECTION", "10.0.0.1 5000 10.0.0.2 22"),
            ("SSH_TTY", "/dev/pts/3"),
        ]));
        assert_eq!(line.last(), Some(&b'\n'));
        assert_eq!(line.iter().filter(|b| **b == b'\n').count(), 1);
        let value: serde_json::Value = serde_json::from_slice(&line).unwrap();
        assert_eq!(
            value,
            serde_json::json!({"bridge": {
                "ssh_connection": "10.0.0.1 5000 10.0.0.2 22",
                "ssh_client": null,
            }})
        );
    }

    /// Plan 085 D2: a value over 256 bytes or with a NUL is left out, as
    /// the session's starter and roostctl leave it out — never cut to a
    /// prefix no other opener would send. The other value is kept.
    #[test]
    fn a_value_the_value_rules_refuse_is_left_out_not_cut() {
        let client = "10.0.0.1 5000 22";
        for refused in [
            "a".repeat(257),
            format!("{}€", "a".repeat(254)),
            "1\0 2".into(),
        ] {
            let line = preamble_line(&env(&[
                ("SSH_CONNECTION", &refused),
                ("SSH_CLIENT", client),
            ]));
            assert_eq!(
                origin(&line),
                serde_json::json!({"ssh_connection": null, "ssh_client": client}),
                "{} bytes",
                refused.len()
            );
        }
        let longest = "a".repeat(256);
        let line = preamble_line(&env(&[("SSH_CONNECTION", &longest)]));
        assert_eq!(origin(&line)["ssh_connection"], longest.as_str());
    }

    #[test]
    fn a_value_that_is_not_utf8_is_left_out() {
        use std::os::unix::ffi::OsStringExt;
        let line = preamble_line(&[(
            OsString::from("SSH_CLIENT"),
            OsString::from_vec(b"10.0.0.1 \xff 22".to_vec()),
        )]);
        assert_eq!(
            origin(&line),
            serde_json::json!({"ssh_connection": null, "ssh_client": null})
        );
    }

    /// Two values at the limit fit the session's line limit even when
    /// every byte is escaped.
    #[test]
    fn the_longest_preamble_fits_the_line_limit() {
        let worst = "\u{1}".repeat(256);
        let line = preamble_line(&env(&[("SSH_CONNECTION", &worst), ("SSH_CLIENT", &worst)]));
        assert!(!origin(&line)["ssh_client"].is_null(), "both are kept");
        assert!(line.len() - 1 <= MAX_BRIDGE_PREAMBLE, "{}", line.len());
    }

    /// Plan 083 A2 review (R6): an envelope a session would refuse to
    /// decode is refused here too, even one naming a forwarded op — a
    /// duplicate `op`, no `id`, an `op` that is not a string, a key the
    /// envelope does not have.
    #[test]
    fn a_malformed_envelope_naming_a_forwarded_op_is_refused() {
        for line in [
            &br#"{"id":"1","op":"tab.attach","op":"session.identify"}"#[..],
            br#"{"id":"1","op":"session.identify","op":"session.identify"}"#,
            br#"{"op":"session.stop"}"#,
            br#"{"id":1,"op":"session.stop"}"#,
            br#"{"id":"1","op":["session.stop"]}"#,
            br#"{"id":"1","op":"session.stop","attach":true}"#,
        ] {
            let reply = refused(line);
            assert_eq!(reply["ok"], false, "{}", String::from_utf8_lossy(line));
        }
    }
}
