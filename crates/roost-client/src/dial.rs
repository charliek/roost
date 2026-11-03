//! How a client reaches a session: one new byte stream per connection.
//!
//! The session wire is the same whatever carries it — a Unix socket on
//! this machine, or an SSH exec of `roost-session client-bridge` — so the
//! connection task, the event stream and every data-plane attach take a
//! [`Dialer`] and never a path.

use std::path::PathBuf;

use futures::future::BoxFuture;
use futures::FutureExt;

pub type BoxRead = Box<dyn tokio::io::AsyncRead + Send + Unpin>;
pub type BoxWrite = Box<dyn tokio::io::AsyncWrite + Send + Unpin>;

/// One dialed stream, split.
pub struct Duplex {
    pub read: BoxRead,
    pub write: BoxWrite,
}

pub trait Dialer: Send + Sync + 'static {
    /// A new stream to the session socket: control, events, or one tab's
    /// data plane.
    fn dial(&self) -> BoxFuture<'_, Result<Duplex, DialError>>;

    /// What a message names this target as.
    fn describe(&self) -> String;
}

#[derive(Debug, thiserror::Error)]
pub enum DialError {
    #[error("{0}")]
    Io(#[from] std::io::Error),
    /// Trust on first use: the UI shows the fingerprint, pins it, and
    /// reconnects.
    #[error("host key for {host} is not trusted yet ({fingerprint})")]
    UntrustedHostKey { host: String, fingerprint: String },
    /// The pinned key changed. Never retried.
    #[error("host key for {host} changed: expected {expected}, got {got}")]
    HostKeyChanged {
        host: String,
        expected: String,
        got: String,
    },
    /// The far side refused: no session, auth, a missing binary.
    #[error("{message}")]
    Remote {
        failure: Option<roost_ipc::ssh::SshFailure>,
        message: String,
    },
}

impl DialError {
    /// Whether retrying without the user doing something could help.
    pub fn retryable(&self) -> bool {
        match self {
            DialError::Io(_) => true,
            DialError::UntrustedHostKey { .. } | DialError::HostKeyChanged { .. } => false,
            DialError::Remote { failure, .. } => !matches!(
                failure,
                Some(
                    roost_ipc::ssh::SshFailure::ChangedHostKey
                        | roost_ipc::ssh::SshFailure::HostKeyUnknown
                        | roost_ipc::ssh::SshFailure::Auth
                        | roost_ipc::ssh::SshFailure::NotFound
                )
            ),
        }
    }
}

/// A session socket on this machine: the desktop dev loop and the tests.
pub struct UnixDialer(pub PathBuf);

impl Dialer for UnixDialer {
    fn dial(&self) -> BoxFuture<'_, Result<Duplex, DialError>> {
        async move {
            let stream = tokio::net::UnixStream::connect(&self.0).await?;
            let (read, write) = stream.into_split();
            Ok(Duplex {
                read: Box::new(read),
                write: Box::new(write),
            })
        }
        .boxed()
    }

    fn describe(&self) -> String {
        self.0.display().to_string()
    }
}
