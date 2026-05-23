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

        // Remove a stale socket if present. The full TOCTOU-safe
        // single-instance protocol lives in M6 (with a flock); M2 is
        // a thin baseline.
        remove_socket_if_present(&socket_path).await?;

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
        })
    }

    /// Run the accept loop until the listener returns an error.
    /// Typical use: spawn this on a tokio task and let the
    /// application's lifecycle drive shutdown by dropping the server
    /// handle.
    pub async fn run(self) -> anyhow::Result<()> {
        loop {
            let (conn, _) = self.listener.accept().await?;
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

async fn serve_connection<H: Handler>(stream: UnixStream, handler: Arc<H>) -> Result<(), Error> {
    let (r, mut w) = stream.into_split();
    let mut reader = FrameReader::new(r);

    while let Some(line) = reader.read_line().await? {
        let request: RawRequest = match serde_json::from_slice(&line) {
            Ok(r) => r,
            Err(e) => {
                // Best-effort error envelope. If we can't parse the
                // envelope itself we don't know the id, so we send id=0.
                let body = serde_json::to_vec(&Response::err(
                    0,
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

#[cfg(unix)]
fn is_socket(file_type: std::fs::FileType) -> bool {
    use std::os::unix::fs::FileTypeExt;
    file_type.is_socket()
}

#[cfg(not(unix))]
fn is_socket(_file_type: std::fs::FileType) -> bool {
    false
}

async fn remove_socket_if_present(path: &Path) -> anyhow::Result<()> {
    match tokio::fs::symlink_metadata(path).await {
        Ok(meta) => {
            if is_socket(meta.file_type()) {
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
