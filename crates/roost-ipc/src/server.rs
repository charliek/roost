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

use crate::framing::{write_frame, FrameReader};
use crate::messages::{RawRequest, Response};
use crate::socket_state::{self, SocketState};
use crate::Error;

/// A handler dispatches a single request to a typed implementation.
///
/// Returning `Ok(value)` produces a `{"ok": true, "result": value}`
/// envelope; returning `Err(HandlerError)` produces a
/// `{"ok": false, "error": {code, message}}` envelope.
///
/// `Send + Sync + 'static` because tokio's accept loop and per-conn
/// tasks move the handler across threads.
pub trait Handler: Send + Sync + 'static {
    /// Handle one decoded request. `op` is the dotted-lowercase op
    /// name; `params` is the raw JSON object (handler decodes per-op
    /// into the typed struct).
    fn handle<'a>(
        &'a self,
        op: &'a str,
        params: serde_json::Value,
    ) -> Pin<Box<dyn Future<Output = Result<serde_json::Value, HandlerError>> + Send + 'a>>;
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
        loop {
            let (conn, _) = self.listener.accept().await?;
            // `None` means enforcement is off, so the connection is
            // served. Dropping `conn` here closes it; the peer sees EOF.
            if self
                .required_uid
                .is_some_and(|expected_uid| !peer_is_allowed(&conn, expected_uid))
            {
                continue;
            }
            let handler = self.handler.clone();
            tokio::spawn(async move {
                if let Err(e) = serve_connection(conn, handler).await {
                    debug!(error = %e, "ipc connection ended");
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

async fn serve_connection<H: Handler>(stream: UnixStream, handler: Arc<H>) -> Result<(), Error> {
    let (r, mut w) = stream.into_split();
    let mut reader = FrameReader::new(r);

    while let Some(line) = reader.read_line().await? {
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
        let result = handler.handle(&op, request.params).await;
        let response = match result {
            Ok(value) => Response::ok(id, value),
            Err(err) => Response::err(id, err.code, err.message),
        };
        let body = match serde_json::to_vec(&response) {
            Ok(b) => b,
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
                    Ok(b) => b,
                    Err(e2) => {
                        warn!(error = %e2, id, "fallback response also failed to serialize; closing connection");
                        return Ok(());
                    }
                }
            }
        };
        write_frame(&mut w, &body).await?;
    }
    Ok(())
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
