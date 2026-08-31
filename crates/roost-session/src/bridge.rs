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

use std::path::Path;

use anyhow::{anyhow, Context};
use roost_ipc::paths::BundleProfile;
use roost_ipc::socket_state::{classify_connect_error, SocketState};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::UnixStream;

/// One read's worth of wire bytes. Large enough that a snapshot stream
/// is not chopped into a syscall per frame, and a match for the default
/// pipe buffer on the stdio side.
const CHUNK: usize = 64 * 1024;

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
        .enable_io()
        .build()
        .context("build the bridge runtime")?;
    let outcome = runtime.block_on(async {
        let socket = connect(&profile.socket_path).await?;
        pump(socket).await
    });
    // Never dropped: `tokio::io::stdin` reads on a blocking thread that
    // can still be parked in `read(2)` when the socket side finishes,
    // and dropping the runtime would wait for it forever.
    runtime.shutdown_background();
    outcome
}

async fn connect(socket_path: &Path) -> anyhow::Result<UnixStream> {
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
            SocketState::Missing | SocketState::Stale => anyhow!("{hint}"),
            _ => anyhow!("{hint} ({error})"),
        }
    })
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
