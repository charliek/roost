//! `IpcServer` — accepts JSON-IPC connections on a Unix-domain socket
//! and dispatches each request to a [`Handler`].
//!
//! Threading model (Rust side, mirrors `docs/reference/ipc.md`):
//!
//! * The accept loop and per-connection read loops run on tokio
//!   worker threads.
//! * JSON parse happens on those tokio threads.
//! * The handler trait is `async` and `Send + Sync`, so a UI process
//!   that needs main-thread (glib / `@MainActor`) work hops itself
//!   via the appropriate primitive (e.g. `glib::MainContext::channel`)
//!   inside its handler impl.
//! * The framed write per connection is owned by the per-connection
//!   task; concurrent writes from different connections do not
//!   interleave.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;

use anyhow::Context;
use tokio::net::{UnixListener, UnixStream};
use tracing::{debug, warn};

use crate::dataframe::DataFrameReader;
use crate::framing::{write_frame, FrameReader};
use crate::messages::{
    AttachHandshake, AttachHandshakeReply, EventEnvelope, RawRequest, Response,
    SessionStoppingEvent, SESSION_STOPPING_EVENT,
};
use crate::socket_state::{self, SocketState};
use crate::Error;

/// A handler dispatches a single request to a typed implementation.
///
/// Returning `Ok(HandlerOutcome::Reply(value))` produces a
/// `{"ok": true, "result": value}` envelope; returning
/// `Err(HandlerError)` produces a `{"ok": false, "error": {code,
/// message}}` envelope. [`HandlerOutcome::ReplyThen`] writes the same
/// reply first and then hands the connection to a [`ConnAction`].
///
/// `Send + Sync + 'static` because tokio's accept loop and per-conn
/// tasks move the handler across threads.
pub trait Handler: Send + Sync + 'static {
    /// Handle one decoded request. `op` is the dotted-lowercase op
    /// name; `params` is the raw JSON object (handler decodes per-op
    /// into the typed struct).
    ///
    /// `ctx` identifies the connection this request arrived on and
    /// carries the handle that closes it. A handler that ignores `ctx`
    /// is wire-identical to one written before it existed.
    fn handle<'a>(
        &'a self,
        ctx: &'a ConnCtx,
        op: &'a str,
        params: serde_json::Value,
    ) -> Pin<Box<dyn Future<Output = Result<HandlerOutcome, HandlerError>> + Send + 'a>>;

    /// Serve a connection whose first line was an attach handshake
    /// rather than a request envelope — the connection has already
    /// stopped being a request/response socket by the time this is
    /// called, so the handler owns both halves and every byte from here
    /// on, including the handshake reply line.
    ///
    /// The default answers the one honest thing a socket with no data
    /// plane can say and closes. Only a host session overrides it; the
    /// UI sockets keep this.
    fn handle_data<'a>(
        &'a self,
        ctx: &'a ConnCtx,
        handshake: AttachHandshake,
        conn: DataConn,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
        let _ = (ctx, handshake);
        Box::pin(async move {
            let (_reader, mut writer, _close) = conn.into_parts();
            let written = write_handshake_rejection(
                &mut writer,
                "not-supported",
                "this socket does not serve attach data connections",
            )
            .await;
            if let Err(e) = written {
                debug!(error = %e, "attach rejection could not be written");
            }
        })
    }
}

/// Why the server is closing a connection out from under its own read
/// loop.
///
/// The reason exists so the peer learns *why*: a client that was taken
/// over should not retry the way one whose session is shutting down
/// should.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloseReason {
    /// Another client took the session's interactive lease.
    TakenOver,
    /// A newer data connection replaced this one for the same tab.
    Superseded,
    /// The session is stopping.
    ShuttingDown,
}

impl CloseReason {
    /// The `reason` a push connection's [`SESSION_STOPPING_EVENT`]
    /// envelope carries. The published vocabulary is exactly
    /// `"stop"` | `"taken-over"`; `Superseded` is a data-plane-only
    /// reason that never reaches a push connection, and if it somehow
    /// does, "stop" is the truthful half of it (the stream is over)
    /// rather than a word no client knows.
    pub fn stopping_reason(self) -> &'static str {
        match self {
            CloseReason::TakenOver => "taken-over",
            CloseReason::Superseded | CloseReason::ShuttingDown => "stop",
        }
    }

    /// The stable code a data connection's `ERROR` frame carries.
    ///
    /// No caller in this crate: the frame is written by whatever
    /// overrides [`Handler::handle_data`] (the engine, at HS-1b), and
    /// the vocabulary lives here so both ends read it from one place.
    pub fn error_code(self) -> &'static str {
        match self {
            CloseReason::TakenOver => "taken-over",
            CloseReason::Superseded => "superseded",
            CloseReason::ShuttingDown => "shutting-down",
        }
    }
}

/// How long a connection being closed by the server may spend telling
/// its peer why.
///
/// Short on purpose: the label is a courtesy, the close is not. A peer
/// that has stopped reading makes the labeled write physically
/// impossible once the socket buffer fills, and EOF is then the signal.
pub const CLOSE_LABEL_DEADLINE: std::time::Duration = std::time::Duration::from_secs(2);

/// Identity of the connection a request arrived on, handed to every
/// [`Handler::handle`] call.
///
/// A session's lease registry keys on `conn_id` and stores `closer`, so
/// a takeover can close every connection the previous lease holder
/// owned — including ones parked in push or data mode, which no longer
/// read requests and so can never be told anything by replying.
pub struct ConnCtx {
    pub conn_id: u64,
    pub closer: ConnCloser,
}

impl ConnCtx {
    /// Mint a context plus the watch its connection selects on. The
    /// accept loop calls this once per connection; a test that drives a
    /// [`Handler`] directly calls it to get a context to hand in.
    pub fn new(conn_id: u64) -> (Self, ConnCloseWatch) {
        let (closer, watch) = ConnCloser::new();
        (Self { conn_id, closer }, watch)
    }
}

/// A one-shot, cloneable "close this connection, and here is why".
///
/// Cloneable because a registry keeps one per connection while the
/// connection task keeps its own; one-shot because the first reason
/// wins — a connection being torn down for a takeover that then gets
/// caught by a shutdown should still report the takeover.
#[derive(Clone)]
pub struct ConnCloser {
    tx: Arc<tokio::sync::watch::Sender<Option<CloseReason>>>,
}

impl ConnCloser {
    fn new() -> (Self, ConnCloseWatch) {
        let (tx, rx) = tokio::sync::watch::channel(None);
        (Self { tx: Arc::new(tx) }, ConnCloseWatch { rx })
    }

    /// Fire the closer. Returns whether this call is the one that set
    /// the reason; a second call is a no-op.
    pub fn close(&self, reason: CloseReason) -> bool {
        self.tx.send_if_modified(|slot| {
            if slot.is_some() {
                false
            } else {
                *slot = Some(reason);
                true
            }
        })
    }

    /// The reason this closer fired, if it has.
    pub fn reason(&self) -> Option<CloseReason> {
        *self.tx.borrow()
    }
}

/// The receiving half of a [`ConnCloser`], held by whatever owns the
/// connection's write side.
#[derive(Clone)]
pub struct ConnCloseWatch {
    rx: tokio::sync::watch::Receiver<Option<CloseReason>>,
}

impl ConnCloseWatch {
    /// Resolve once the closer fires. Cancel-safe, and sticky: after
    /// firing it resolves immediately, every time.
    ///
    /// If every closer handle is dropped without firing, this never
    /// resolves — by then the only thing that can still end the
    /// connection is its own read loop, which is exactly what the
    /// caller is selecting this against.
    pub async fn closed(&mut self) -> CloseReason {
        loop {
            if let Some(reason) = *self.rx.borrow_and_update() {
                return reason;
            }
            if self.rx.changed().await.is_err() {
                std::future::pending::<()>().await;
            }
        }
    }

    /// The reason, without waiting.
    pub fn reason(&self) -> Option<CloseReason> {
        *self.rx.borrow()
    }
}

/// Both halves of a connection that turned out to be a data connection,
/// handed to [`Handler::handle_data`].
///
/// The reader is already the binary framer, seeded with whatever the
/// line reader had buffered past the handshake line: the handshake and
/// the first binary bytes routinely share a `read`, and dropping that
/// residue would lose the head of the stream.
pub struct DataConn {
    reader: DataFrameReader<tokio::net::unix::OwnedReadHalf>,
    writer: tokio::net::unix::OwnedWriteHalf,
    close: ConnCloseWatch,
}

impl DataConn {
    pub fn into_parts(
        self,
    ) -> (
        DataFrameReader<tokio::net::unix::OwnedReadHalf>,
        tokio::net::unix::OwnedWriteHalf,
        ConnCloseWatch,
    ) {
        (self.reader, self.writer, self.close)
    }
}

/// What a [`Handler`] wants done with the connection after this request.
///
/// A dispatcher that only ever returns [`HandlerOutcome::Reply`] is
/// wire-identical to the request/response-only server: the reply frame
/// is written and the read loop continues.
#[derive(Debug)]
pub enum HandlerOutcome {
    /// Write `{"ok": true, "result": value}` and keep serving requests.
    Reply(serde_json::Value),
    /// Write the reply first, *then* run `then`. The ordering is the
    /// contract: a client that sees the reply frame knows the action has
    /// not run yet, and a client that never sees it knows nothing was
    /// started on its behalf.
    ReplyThen {
        reply: serde_json::Value,
        then: ConnAction,
    },
}

// Hand-written: neither a push source nor a finalizer has anything
// meaningful to print, but callers still want an outcome in an
// assertion message.
impl std::fmt::Debug for ConnAction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConnAction::StartPush(_) => f.write_str("StartPush"),
            ConnAction::FinalizeStop(_) => f.write_str("FinalizeStop"),
        }
    }
}

/// A change of mode for the connection, taken after the reply is on the
/// wire.
pub enum ConnAction {
    /// Hand the connection's owned write half to a push loop fed by
    /// `source`. The read half demotes to discard-and-detect-EOF: frames
    /// are still read (and thrown away) so a peer that goes away is
    /// noticed promptly.
    StartPush(PushSource),
    /// Close the connection and run `finalizer` on a detached task.
    /// Used by `session.stop`, whose shutdown tail must not begin until
    /// the stop reply has been flushed to the caller.
    ///
    /// Two guarantees, both load-bearing for a daemon whose stop already
    /// happened by the time the finalizer is handed over:
    ///
    /// * It runs even if the reply could not be written (the peer hung
    ///   up mid-shutdown), so a lost reply can never strand the process.
    /// * It is detached, so [`IpcServer::run_until`]'s cancellation —
    ///   which the finalizer itself typically triggers — cannot abort it
    ///   part-way through.
    FinalizeStop(StopFinalizer),
}

/// Budget one push frame gets to reach a peer before the connection is
/// considered stalled. Generous — a healthy client drains a few hundred
/// bytes in microseconds, so reaching this means the peer has stopped
/// reading, not that it is busy.
pub const DEFAULT_PUSH_WRITE_DEADLINE: std::time::Duration = std::time::Duration::from_secs(30);

/// Ready-to-write push messages for [`ConnAction::StartPush`].
///
/// Deliberately a plain bounded-channel receiver rather than a
/// `Stream`: this crate has no `futures`/`tokio-stream` dependency, and
/// the producer side is always somebody else's task (the event bridge
/// adapts a workspace broadcast receiver into the sender), so a channel
/// is both the simplest shape and the one that keeps the backpressure
/// policy on the producer's side of the seam.
pub struct PushSource {
    rx: tokio::sync::mpsc::Receiver<serde_json::Value>,
    write_deadline: std::time::Duration,
}

impl PushSource {
    pub fn new(rx: tokio::sync::mpsc::Receiver<serde_json::Value>) -> Self {
        Self {
            rx,
            write_deadline: DEFAULT_PUSH_WRITE_DEADLINE,
        }
    }

    /// How long one frame may sit unwritten before the connection is
    /// torn down.
    ///
    /// A peer that has stopped reading blocks the write once the socket
    /// buffer fills, and an unbounded write there parks the connection
    /// task — holding the socket and everything queued behind it —
    /// indefinitely. Producers that already have a stall policy
    /// (`roost-engine`'s event relay) pass theirs so the two bounds
    /// cannot disagree; everyone else gets
    /// [`DEFAULT_PUSH_WRITE_DEADLINE`].
    #[must_use]
    pub fn with_write_deadline(mut self, deadline: std::time::Duration) -> Self {
        self.write_deadline = deadline;
        self
    }

    /// The next message to write, or `None` once every sender is gone
    /// (which ends the push loop and closes the connection).
    ///
    /// Cancel-safe: `mpsc::Receiver::recv` is, so this may be used in a
    /// `select!` without losing a message.
    pub async fn next(&mut self) -> Option<serde_json::Value> {
        self.rx.recv().await
    }
}

/// The tail a [`ConnAction::FinalizeStop`] runs once the reply is out.
///
/// Boxed rather than generic so the outcome type stays object-safe and
/// this crate stays ignorant of what "stop" means to its embedder.
///
/// It runs on a detached task and survives connection cancellation, so
/// it is free to be the thing that resolves [`IpcServer::run_until`]'s
/// shutdown future.
pub struct StopFinalizer(Box<dyn FnOnce() -> BoxFuture + Send>);

type BoxFuture = Pin<Box<dyn Future<Output = ()> + Send>>;

impl StopFinalizer {
    pub fn new<F, Fut>(f: F) -> Self
    where
        F: FnOnce() -> Fut + Send + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        Self(Box::new(move || Box::pin(f())))
    }

    pub async fn run(self) {
        (self.0)().await;
    }
}

/// Error returned by a [`Handler`] implementation.
#[derive(Debug, thiserror::Error)]
#[error("{code}: {message}")]
pub struct HandlerError {
    pub code: String,
    pub message: String,
}

impl HandlerError {
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
        }
    }

    pub fn unknown_op(op: &str) -> Self {
        Self::new("unknown-op", format!("no such op: {op}"))
    }

    pub fn invalid_param(message: impl Into<String>) -> Self {
        Self::new("invalid-param", message)
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new("not-found", message)
    }
}

/// Server bound to a Unix-domain socket.
pub struct IpcServer<H: Handler> {
    listener: UnixListener,
    handler: Arc<H>,
    socket_path: PathBuf,
    /// When set, the accept loop drops any connection whose peer
    /// effective UID differs. Unset by default — the UI sockets keep
    /// the behavior they have always had.
    required_uid: Option<u32>,
}

impl<H: Handler> IpcServer<H> {
    /// Bind a fresh server at `socket_path`. Removes a stale socket
    /// at the same path (only if it actually is a socket — refuses to
    /// silently delete a regular file).
    pub async fn bind(socket_path: impl AsRef<Path>, handler: H) -> anyhow::Result<Self> {
        let socket_path = socket_path.as_ref().to_path_buf();

        // Ensure the parent directory exists. Errors here are fatal
        // — there's no clean way to recover from a missing parent.
        if let Some(parent) = socket_path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .with_context(|| format!("create {}", parent.display()))?;
        }

        // Remove a stale socket if present.
        //
        // What makes this safe is the caller holding the socket/bind
        // flock (`BundleProfile::socket_lock_path`, acquired in each
        // UI's `main` before this path is reached) across the whole
        // probe→unlink→bind sequence — not the probe, which on its own
        // is TOCTOU. The probe is the second line of defence: it stops
        // a process whose lock file was removed underneath it, or that
        // holds the lock for a different runtime dir, from stealing a
        // live socket. Anything other than "refused" or "absent" is
        // treated as live and refused (see `socket_state`).
        //
        // The Mac side does the equivalent dance in
        // `mac/Sources/Roost/IPCServer.swift::bindWithRecovery` (M6);
        // it gates the unlink on the flock state rather than doing it
        // unconditionally because Mac's `IPCServer` is sometimes
        // constructed from contexts that don't own the lock (tests,
        // `ROOST_ALLOW_MULTI=1`).
        let state = socket_state::probe(&socket_path, socket_state::PROBE_TIMEOUT).await;
        match state {
            SocketState::Missing => {}
            SocketState::Stale => remove_socket_if_present(&socket_path).await?,
            SocketState::NotASocket(kind) => anyhow::bail!(
                "refusing to remove non-socket path {} (file type: {kind}). \
                 If this was intentional, remove it manually first.",
                socket_path.display(),
            ),
            SocketState::Live => anyhow::bail!(
                "a live listener already answers on {}; refusing to unlink it",
                socket_path.display(),
            ),
            SocketState::Indeterminate(why) => anyhow::bail!(
                "cannot tell whether {} is live ({why}); refusing to unlink it",
                socket_path.display(),
            ),
        }

        let listener = UnixListener::bind(&socket_path)
            .with_context(|| format!("bind {}", socket_path.display()))?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let perms = std::fs::Permissions::from_mode(0o600);
            std::fs::set_permissions(&socket_path, perms)
                .with_context(|| format!("chmod 0600 {}", socket_path.display()))?;
        }

        Ok(Self {
            listener,
            handler: Arc::new(handler),
            socket_path,
            required_uid: None,
        })
    }

    /// Serve only peers whose effective UID is `expected_uid`; drop
    /// every other connection at accept.
    ///
    /// The uid is injected rather than read here so the reject branch is
    /// reachable from a test over a real socket (`require_uid(euid + 1)`).
    /// Production callers want [`Self::require_same_uid`].
    ///
    /// Socket mode bits alone stop nothing once the socket is forwarded
    /// (sshd opens a remote-forwarded socket as the forwarding user), so
    /// a session serving over SSH needs the kernel's answer to "who is
    /// on the other end", not the filesystem's.
    #[must_use]
    pub fn require_uid(mut self, expected_uid: u32) -> Self {
        self.required_uid = Some(expected_uid);
        self
    }

    /// Serve only peers running as this process's own user.
    #[must_use]
    pub fn require_same_uid(self) -> Self {
        self.require_uid(crate::peer::current_euid())
    }

    /// Run the accept loop until the listener returns an error.
    /// Typical use: spawn this on a tokio task and let the
    /// application's lifecycle drive shutdown by dropping the server
    /// handle.
    pub async fn run(self) -> anyhow::Result<()> {
        self.run_until(std::future::pending::<()>()).await
    }

    /// Like [`Self::run`], but stops accepting and cancels every live
    /// connection task as soon as `shutdown` resolves.
    ///
    /// Cancelling the live tasks is the half that matters: a session
    /// that has already torn its PTYs down must not leave a peer parked
    /// on a half-open socket waiting for a reply that will never come.
    ///
    /// A [`ConnAction::FinalizeStop`] tail is exempt — it runs detached,
    /// so a finalizer that resolves `shutdown` still completes after the
    /// connection it came from is cancelled.
    pub async fn run_until(
        self,
        shutdown: impl Future<Output = ()> + Send + 'static,
    ) -> anyhow::Result<()> {
        // `false` -> serving, `true` -> cancelled. A watch (rather than
        // a token type from another crate) keeps this dependency-free
        // and lets every live task observe the flip.
        //
        // The sender is shared with each connection task so it outlives
        // this loop: a dropped sender also wakes `changed()`, which would
        // turn a fatal accept error into a mass cancellation. That error
        // is deliberately *not* a cancellation — it ends the accept loop
        // only, leaving served connections to finish exactly as they did
        // before `run_until` existed.
        let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
        let cancel_tx = Arc::new(cancel_tx);
        let mut shutdown = Box::pin(shutdown);
        loop {
            let accepted = tokio::select! {
                accepted = self.listener.accept() => accepted,
                () = &mut shutdown => {
                    let _ = cancel_tx.send(true);
                    break Ok(());
                }
            };
            let (conn, _) = match accepted {
                Ok(conn) => conn,
                Err(e) => break Err(anyhow::Error::from(e)),
            };
            // `None` means enforcement is off, so the connection is
            // served. Dropping `conn` here closes it; the peer sees EOF.
            if self
                .required_uid
                .is_some_and(|expected_uid| !peer_is_allowed(&conn, expected_uid))
            {
                continue;
            }
            let handler = self.handler.clone();
            let mut cancel = cancel_rx.clone();
            let keep_sender_alive = cancel_tx.clone();
            let conn_id = next_conn_id();
            tokio::spawn(async move {
                let _keep_sender_alive = keep_sender_alive;
                tokio::select! {
                    served = serve_connection(conn, handler, conn_id) => {
                        if let Err(e) = served {
                            debug!(error = %e, "ipc connection ended");
                        }
                    }
                    // `changed()` errors only if the sender is gone,
                    // which also means the server is finished — either
                    // way the connection is done.
                    _ = cancel.changed() => {
                        debug!("ipc connection cancelled by server shutdown");
                    }
                }
            });
        }
    }

    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }
}

/// The accept loop is the boundary that handles a peer-check failure,
/// so it is where the failure is logged. Fail-closed: a lookup that
/// errors is a reject, because a connection we cannot attribute is
/// exactly the one not to trust.
fn peer_is_allowed(conn: &UnixStream, expected_uid: u32) -> bool {
    match crate::peer::peer_uid(conn) {
        Ok(uid) if uid == expected_uid => true,
        Ok(uid) => {
            warn!(
                peer_uid = uid,
                expected_uid, "dropping ipc connection from a foreign uid"
            );
            false
        }
        Err(e) => {
            warn!(
                error = %e,
                expected_uid,
                "dropping ipc connection: peer credential lookup failed"
            );
            false
        }
    }
}

/// Process-unique connection ids. Starts at 1 so `0` stays available
/// as "no connection" in a registry.
fn next_conn_id() -> u64 {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

/// Does this line open a data connection?
///
/// The test is deliberately narrow: a JSON **object** carrying `attach`
/// and **not** carrying `op`. Anything else — an op-carrying envelope
/// (even one that also has `attach`), a non-object, malformed JSON —
/// stays on the request path and behaves exactly as it did before the
/// data plane existed.
fn is_data_handshake(line: &[u8]) -> bool {
    let Ok(serde_json::Value::Object(map)) = serde_json::from_slice::<serde_json::Value>(line)
    else {
        return false;
    };
    map.contains_key("attach") && !map.contains_key("op")
}

async fn serve_connection<H: Handler>(
    stream: UnixStream,
    handler: Arc<H>,
    conn_id: u64,
) -> Result<(), Error> {
    let (r, mut w) = stream.into_split();
    let mut reader = FrameReader::new(r);
    let (ctx, mut close_watch) = ConnCtx::new(conn_id);
    // The sniff is a property of the connection's *first* line only, so
    // a request stream can never be diverted mid-flight by a payload
    // that happens to look like a handshake.
    let mut first_line = true;

    loop {
        // Checked before the read, not only inside the `select!`: with
        // `biased` a peer that always has another request buffered
        // would otherwise keep the read arm ready forever and never let
        // the closer be polled.
        if let Some(reason) = close_watch.reason() {
            debug!(
                conn_id,
                ?reason,
                "closing ipc connection at the server's request"
            );
            return Ok(());
        }
        let line = tokio::select! {
            // The read loop is the common case; check it first so a
            // closer that fires while a request is already in hand does
            // not race the reply out of existence.
            biased;
            line = reader.read_line() => match line? {
                Some(line) => line,
                None => return Ok(()),
            },
            // A plain request connection has nothing to label: there is
            // no stream the peer is waiting on, only a reply it did not
            // ask for. Close.
            reason = close_watch.closed() => {
                debug!(conn_id, ?reason, "closing ipc connection at the server's request");
                return Ok(());
            }
        };

        if std::mem::take(&mut first_line) && is_data_handshake(&line) {
            return serve_data(&ctx, handler.as_ref(), reader, w, close_watch, &line).await;
        }

        let request: RawRequest = match serde_json::from_slice(&line) {
            Ok(r) => r,
            Err(e) => {
                // `RawRequest` is `deny_unknown_fields`, so a request
                // carrying a valid `id` alongside an otherwise
                // malformed envelope (extra field, wrong-typed param,
                // ...) fails the typed decode and would lose the id.
                // Peel `id` from the raw JSON so the error reply lands
                // at the id the client is matching on, not id=0.
                // Truly un-parseable input falls back to id=0. The
                // extra parse only runs on the error path. (#80)
                // `id` is string-encoded on the wire (string_int64),
                // so peel it as a string and parse; tolerate a bare
                // JSON number too.
                let id = serde_json::from_slice::<serde_json::Value>(&line)
                    .ok()
                    .and_then(|v| v.get("id").cloned())
                    .and_then(|id| {
                        id.as_i64()
                            .or_else(|| id.as_str().and_then(|s| s.parse().ok()))
                    })
                    .unwrap_or(0);
                let body = serde_json::to_vec(&Response::err(
                    id,
                    "parse-error",
                    format!("envelope decode failed: {e}"),
                ))?;
                write_frame(&mut w, &body).await?;
                continue;
            }
        };

        let id = request.id;
        let op = request.op.clone();
        let result = handler.handle(&ctx, &op, request.params).await;
        let (response, action) = match result {
            Ok(HandlerOutcome::Reply(value)) => (Response::ok(id, value), None),
            Ok(HandlerOutcome::ReplyThen { reply, then }) => (Response::ok(id, reply), Some(then)),
            Err(err) => (Response::err(id, err.code, err.message), None),
        };
        // `None` when not even the fallback envelope could be encoded:
        // there is nothing to put on the wire for this id.
        let body = match serde_json::to_vec(&response) {
            Ok(b) => Some(b),
            Err(e) => {
                // Surface the failure to the client rather than
                // dropping the request on the floor — the original
                // handler result was unrepresentable (e.g. a value
                // containing a non-finite float), but the client
                // still deserves a reply at this id so its read
                // loop unblocks.
                warn!(error = %e, id, op = %op, "response serialization failed; sending fallback");
                let fallback = Response::err(
                    id,
                    "internal",
                    format!("response serialization failed: {e}"),
                );
                match serde_json::to_vec(&fallback) {
                    Ok(b) => Some(b),
                    Err(e2) => {
                        warn!(error = %e2, id, "fallback response also failed to serialize; closing connection");
                        None
                    }
                }
            }
        };
        let encoded = body.is_some();
        let wrote_reply = match body {
            Some(body) => write_frame(&mut w, &body).await,
            None => Ok(()),
        };

        // Strictly after the reply attempt — the reply-before-action
        // ordering is what `ReplyThen` promises. The two actions differ
        // in what a *failed* reply means for them.
        match action {
            Some(ConnAction::FinalizeStop(finalizer)) => {
                // Runs whatever became of the reply. By the time a
                // handler hands this back, the stop it describes has
                // already happened — latched, flushed, reaped — so a peer
                // that hung up during it (a Ctrl-C'd `roostctl session
                // stop`, whose write then fails EPIPE) must not be able
                // to strand the process with its latch set and its socket
                // still on disk.
                if let Err(e) = &wrote_reply {
                    warn!(error = %e, id, op = %op, "reply failed to reach the client; finalizing anyway");
                } else if !encoded {
                    warn!(id, op = %op, "no reply could be encoded; finalizing anyway");
                }
                // Detached on purpose: the finalizer is what typically
                // resolves `run_until`'s shutdown, and that cancels this
                // connection task. Running it here would let the
                // cancellation abort it mid-flight, leaving the socket
                // unlinked. The reply is already out, so nothing is
                // ordered behind this task.
                tokio::spawn(finalizer.run());
                return Ok(());
            }
            Some(ConnAction::StartPush(source)) => {
                // The mirror image: only a client that actually received
                // its subscribe ack may be put into push mode. One that
                // did not would sit on a stream it never learned it was
                // on, so close instead.
                wrote_reply?;
                if !encoded {
                    return Ok(());
                }
                return serve_push(reader, w, source, close_watch).await;
            }
            None => {
                wrote_reply?;
                if !encoded {
                    return Ok(());
                }
            }
        }
    }
}

/// A connection whose first line was an attach handshake. From here on
/// the handler owns the wire — including the reply line, because only
/// it knows whether the handshake is admissible.
///
/// A handshake that does not decode never reaches the handler: a
/// malformed first line is answered with the same `{"ok": false,
/// error}` shape a rejected handshake gets and the connection closes.
/// The alternative — routing an undecodable handshake in as an error
/// path — buys the handler nothing it can act on.
/// The one line a refused data connection gets: an
/// [`AttachHandshakeReply::Rejected`], which is the same shape an
/// accepted handshake's reply has with `ok: false`. Both refusal sites
/// — the undecodable line here and the default [`Handler::handle_data`]
/// — go through this so neither hand-rolls the wire shape.
async fn write_handshake_rejection(
    w: &mut tokio::net::unix::OwnedWriteHalf,
    code: &str,
    message: impl Into<String>,
) -> Result<(), Error> {
    let body = serde_json::to_vec(&AttachHandshakeReply::rejected(code, message))?;
    write_frame(w, &body).await
}

async fn serve_data<H: Handler>(
    ctx: &ConnCtx,
    handler: &H,
    reader: FrameReader<tokio::net::unix::OwnedReadHalf>,
    mut w: tokio::net::unix::OwnedWriteHalf,
    mut close_watch: ConnCloseWatch,
    line: &[u8],
) -> Result<(), Error> {
    let handshake: AttachHandshake = match serde_json::from_slice(line) {
        Ok(h) => h,
        Err(e) => {
            write_handshake_rejection(
                &mut w,
                "parse-error",
                format!("attach handshake decode failed: {e}"),
            )
            .await?;
            return Ok(());
        }
    };

    let (read_half, residue) = reader.into_parts();
    let conn = DataConn {
        reader: DataFrameReader::new(read_half, residue),
        writer: w,
        close: close_watch.clone(),
    };

    // The handler holds the same close watch and owns the labeled
    // `ERROR` frame, so it gets the deadline to write one — but not
    // longer than that. The abort is this loop's guarantee, not the
    // handler's.
    let mut served = handler.handle_data(ctx, handshake, conn);
    tokio::select! {
        biased;
        () = &mut served => {}
        reason = close_watch.closed() => {
            if tokio::time::timeout(CLOSE_LABEL_DEADLINE, served).await.is_err() {
                warn!(
                    conn_id = ctx.conn_id,
                    reason = ?reason,
                    "data connection did not finish within the close deadline; dropping it"
                );
            }
        }
    }
    Ok(())
}

/// Push mode: the connection stops answering requests and becomes a
/// one-way stream of `source`'s messages.
///
/// Teardown is symmetric. The reader task only detects EOF (frames are
/// read and discarded so a peer that keeps writing can't wedge the
/// socket buffer), and whichever side finishes first ends the other:
/// reader EOF drops out of the `select!` and the writer is cancelled
/// with it; a write failure, a stalled write, or an exhausted source
/// aborts the reader.
///
/// The third way it ends is the server closing it (a takeover, a stop):
/// the peer gets one final labeled control envelope, best-effort under
/// [`CLOSE_LABEL_DEADLINE`], and then the connection goes away.
async fn serve_push(
    mut reader: FrameReader<tokio::net::unix::OwnedReadHalf>,
    mut w: tokio::net::unix::OwnedWriteHalf,
    mut source: PushSource,
    mut close_watch: ConnCloseWatch,
) -> Result<(), Error> {
    let write_deadline = source.write_deadline;
    let mut eof = tokio::spawn(async move { while let Ok(Some(_)) = reader.read_line().await {} });
    let result = loop {
        // Sticky check before the select: the closer must win over any
        // other arm that happens to be ready in the same pass (a queued
        // event, a simultaneous EOF), or a taken-over client could keep
        // receiving batches — or lose its label — on a coin flip.
        if let Some(reason) = close_watch.reason() {
            write_stopping_envelope(&mut w, reason).await;
            break Ok(());
        }
        tokio::select! {
            _ = &mut eof => break Ok(()),
            reason = close_watch.closed() => {
                write_stopping_envelope(&mut w, reason).await;
                break Ok(());
            }
            item = source.next() => match item {
                None => break Ok(()),
                Some(value) => {
                    let body = match serde_json::to_vec(&value) {
                        Ok(b) => b,
                        Err(e) => {
                            // Dropping one unrepresentable push message
                            // would silently punch a hole in the stream
                            // the client is gap-checking, so end the
                            // connection instead and let it re-subscribe.
                            warn!(error = %e, "push message failed to serialize; closing connection");
                            break Err(Error::from(e));
                        }
                    };
                    // A peer that stopped reading blocks this write once
                    // the socket buffer fills. Waiting on it forever
                    // holds the connection — and everything queued
                    // behind it — for as long as the peer feels like it,
                    // so a stalled write ends the connection the same
                    // way a failed one does. A closer firing MID-write
                    // shrinks the remaining budget to the label deadline:
                    // if the frame completes in that window the label
                    // goes out on a frame boundary; if not, the peer
                    // gets EOF — the accepted fallback — rather than a
                    // label spliced into a half-written frame.
                    let (wrote, close_reason) = {
                        let write = write_frame(&mut w, &body);
                        tokio::pin!(write);
                        let stall = tokio::time::sleep(write_deadline);
                        tokio::pin!(stall);
                        let mut close_reason: Option<CloseReason> = None;
                        loop {
                            tokio::select! {
                                res = &mut write => break (Some(res), close_reason),
                                () = &mut stall => break (None, close_reason),
                                reason = close_watch.closed(), if close_reason.is_none() => {
                                    close_reason = Some(reason);
                                    stall
                                        .as_mut()
                                        .reset(tokio::time::Instant::now() + CLOSE_LABEL_DEADLINE);
                                }
                            }
                        }
                    };
                    match (wrote, close_reason) {
                        (Some(Ok(())), Some(reason)) => {
                            write_stopping_envelope(&mut w, reason).await;
                            break Ok(());
                        }
                        (Some(Ok(())), None) => {}
                        (Some(Err(e)), _) => break Err(e),
                        (None, Some(reason)) => {
                            debug!(
                                ?reason,
                                "push write did not finish within the close deadline; closing unlabeled"
                            );
                            break Ok(());
                        }
                        (None, None) => {
                            warn!(
                                deadline_ms = write_deadline.as_millis(),
                                "push write stalled past its deadline; closing the connection"
                            );
                            break Err(Error::Io(std::io::Error::new(
                                std::io::ErrorKind::TimedOut,
                                "push write stalled past its deadline",
                            )));
                        }
                    }
                }
            },
        }
    };
    eof.abort();
    result
}

/// The one frame a push connection sends that is not an `EventBatch`:
/// the terminal control envelope naming why the stream is ending.
///
/// Best-effort by contract. A peer that stopped reading has already
/// made this write impossible once the socket buffer filled, and EOF is
/// then the only signal it gets — so a failure here is logged, never
/// propagated.
async fn write_stopping_envelope(w: &mut tokio::net::unix::OwnedWriteHalf, reason: CloseReason) {
    let encoded = serde_json::to_value(SessionStoppingEvent {
        reason: reason.stopping_reason().to_string(),
    })
    .and_then(|data| {
        serde_json::to_vec(&EventEnvelope {
            event: SESSION_STOPPING_EVENT.to_string(),
            data,
        })
    });
    let body = match encoded {
        Ok(b) => b,
        Err(e) => {
            warn!(error = %e, "stopping envelope failed to serialize");
            return;
        }
    };
    match tokio::time::timeout(CLOSE_LABEL_DEADLINE, write_frame(w, &body)).await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => debug!(error = %e, "stopping envelope could not be written; closing anyway"),
        Err(_) => debug!("stopping envelope stalled past its deadline; closing anyway"),
    }
}

/// Unlink `path` if it is a socket. Re-checks the file type rather
/// than trusting the probe's stat, so a path that turned into
/// something else in between is still refused.
async fn remove_socket_if_present(path: &Path) -> anyhow::Result<()> {
    match tokio::fs::symlink_metadata(path).await {
        Ok(meta) => {
            if socket_state::is_socket(meta.file_type()) {
                tokio::fs::remove_file(path)
                    .await
                    .with_context(|| format!("remove stale socket {}", path.display()))?;
                Ok(())
            } else {
                anyhow::bail!(
                    "refusing to remove non-socket path {} (file type: {:?}). \
                     If this was intentional, remove it manually first.",
                    path.display(),
                    meta.file_type()
                );
            }
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err).with_context(|| format!("stat {}", path.display())),
    }
}
