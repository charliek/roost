//! An in-process SSH [`Dialer`]: the mobile app's way to a host's
//! `roost-session`.
//!
//! One SSH connection per host, opened on the first dial and shared by
//! every dial after it — the control connection, the events stream and
//! each attached tab are separate exec channels on it, each running
//! [`roost_ipc::ssh::remote_command`], the same `client-bridge` ladder
//! the desktop's `ssh` transport execs. A connection found dead is
//! replaced on the next dial.
//!
//! The login is Tailscale SSH: tailscaled authorizes the tailnet
//! identity against the ACL and takes the `none` method, so the phone
//! holds no key. Host keys are trusted on first use: an unpinned host
//! fails the dial with its fingerprint for the app to show and pin.

mod channel;
#[cfg(feature = "dev-server")]
#[doc(hidden)]
pub mod devserver;
mod target;

use std::io;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::future::BoxFuture;
use futures::FutureExt;
use roost_client::dial::{DialError, Dialer, Duplex};
use roost_ipc::ssh::SshFailure;
use russh::client;
use russh::keys::{HashAlg, PublicKeyOrCertificate};

pub use target::{HostTarget, TargetError, DEFAULT_PORT};

use channel::{LastRemote, RemoteError};

/// Nothing heard from the host for this long sends a keepalive; [`KEEPALIVE_MAX`]
/// unanswered keepalives close the connection, so a dead network is
/// noticed in about a minute even with nothing to send.
const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(15);
const KEEPALIVE_MAX: usize = 3;

/// TCP connect, key exchange and login, together. A host that is off
/// the tailnet otherwise sits in SYN retries for minutes.
const ESTABLISH_BUDGET: Duration = Duration::from_secs(20);

/// How the dialer logs in.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum SshAuth {
    /// Tailscale SSH: the `none` method, authorized by tailnet identity.
    None,
}

/// Trust on first use. `pinned` is an OpenSSH-style `SHA256:<base64>`
/// fingerprint; with none pinned every dial fails
/// [`DialError::UntrustedHostKey`] carrying the key's fingerprint.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HostKeyPolicy {
    pub pinned: Option<String>,
}

/// A [`Dialer`] over Tailscale SSH to one host.
pub struct TailscaleSshDialer {
    target: HostTarget,
    auth: SshAuth,
    policy: Mutex<HostKeyPolicy>,
    /// Held across establishment, so concurrent first dials share one
    /// connection instead of racing to open several.
    connection: tokio::sync::Mutex<Option<Connection>>,
    last_remote: LastRemote,
}

struct Connection {
    handle: Arc<client::Handle<Verifier>>,
    /// The pin it was verified against: a different pin since means
    /// re-verifying on a new connection.
    fingerprint: String,
}

impl TailscaleSshDialer {
    pub fn new(target: HostTarget, policy: HostKeyPolicy) -> Self {
        Self {
            target,
            auth: SshAuth::None,
            policy: Mutex::new(policy),
            connection: tokio::sync::Mutex::new(None),
            last_remote: LastRemote::default(),
        }
    }

    pub fn with_auth(mut self, auth: SshAuth) -> Self {
        self.auth = auth;
        self
    }

    pub fn target(&self) -> &HostTarget {
        &self.target
    }

    /// Replace the pin — after the user trusts a fingerprint an
    /// [`DialError::UntrustedHostKey`] showed them. The next dial
    /// verifies against it on a fresh connection.
    pub fn set_host_key_policy(&self, policy: HostKeyPolicy) {
        *self.policy.lock().expect("host key policy") = policy;
    }

    pub fn host_key_policy(&self) -> HostKeyPolicy {
        self.policy.lock().expect("host key policy").clone()
    }

    /// The classified failure of the last channel whose remote command
    /// exited non-zero (`client-bridge` finding no session, a missing
    /// `roost-session`), until a channel next hears from the far side.
    ///
    /// That failure ends the channel's reader with an `io::Error` whose
    /// inner error is this same [`DialError::Remote`]; this is the way
    /// to it for a layer that only saw the read fail.
    pub fn last_remote_error(&self) -> Option<DialError> {
        self.last_remote
            .lock()
            .expect("last remote error")
            .as_ref()
            .map(RemoteError::to_dial_error)
    }

    async fn open(&self) -> Result<Duplex, DialError> {
        let (handle, reused) = self.connection(None).await?;
        let channel = match handle.channel_open_session().await {
            // The server turned this one channel down: the connection is
            // fine, and a new one would be turned down the same way.
            Err(error @ russh::Error::ChannelOpenFailure(_)) => return Err(self.ssh_error(error)),
            // A connection that died since the last dial, whether or not
            // russh has noticed yet: replace it, once.
            Err(error) if reused => {
                tracing::debug!(host = %self.target, %error, "shared SSH connection is dead; reconnecting");
                let (handle, _) = self.connection(Some(&handle)).await?;
                handle
                    .channel_open_session()
                    .await
                    .map_err(|error| self.ssh_error(error))?
            }
            result => result.map_err(|error| self.ssh_error(error))?,
        };
        // Not awaiting the exec's reply saves a round trip per dial; a
        // refusal ends the reader the way a failed bridge does.
        channel
            .exec(true, roost_ipc::ssh::remote_command())
            .await
            .map_err(|error| self.ssh_error(error))?;
        Ok(channel::duplex(
            channel,
            self.target.host_port(),
            self.last_remote.clone(),
        ))
    }

    /// The shared connection, established when there is none, when the
    /// pin changed since it was verified, or when it is `dead` — the
    /// handle a channel just failed to open on.
    async fn connection(
        &self,
        dead: Option<&Arc<client::Handle<Verifier>>>,
    ) -> Result<(Arc<client::Handle<Verifier>>, bool), DialError> {
        let mut slot = self.connection.lock().await;
        let pinned = self.host_key_policy().pinned;
        if let Some(connection) = slot.as_ref() {
            let usable = pinned.as_deref() == Some(connection.fingerprint.as_str())
                && !dead.is_some_and(|dead| Arc::ptr_eq(dead, &connection.handle));
            if usable {
                return Ok((connection.handle.clone(), true));
            }
        }
        *slot = None;
        let connection = self.establish(pinned).await?;
        let handle = connection.handle.clone();
        *slot = Some(connection);
        Ok((handle, false))
    }

    async fn establish(&self, pinned: Option<String>) -> Result<Connection, DialError> {
        let config = Arc::new(client::Config {
            keepalive_interval: Some(KEEPALIVE_INTERVAL),
            keepalive_max: KEEPALIVE_MAX,
            nodelay: true,
            ..client::Config::default()
        });
        let verifier = Verifier {
            pinned: pinned.clone(),
        };
        let login = async {
            let address = (self.target.host.as_str(), self.target.port);
            let mut handle = client::connect(config, address, verifier)
                .await
                .map_err(|error| self.establish_error(error))?;
            self.authenticate(&mut handle).await?;
            Ok::<_, DialError>(handle)
        };
        let handle = tokio::time::timeout(ESTABLISH_BUDGET, login)
            .await
            .map_err(|_| {
                DialError::Io(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!("connecting to {} timed out", self.target),
                ))
            })??;
        Ok(Connection {
            handle: Arc::new(handle),
            fingerprint: pinned.expect("a verified connection had a pin"),
        })
    }

    async fn authenticate(&self, handle: &mut client::Handle<Verifier>) -> Result<(), DialError> {
        match &self.auth {
            SshAuth::None => {
                let result = handle
                    .authenticate_none(self.target.user.as_str())
                    .await
                    .map_err(|error| self.ssh_error(error))?;
                if !result.success() {
                    return Err(DialError::Remote {
                        failure: Some(SshFailure::Auth),
                        message: format!(
                            "{} did not accept a Tailscale SSH login; is Tailscale SSH enabled \
                             on it and does the tailnet ACL allow this device?",
                            self.target.host_port()
                        ),
                    });
                }
            }
        }
        Ok(())
    }

    fn establish_error(&self, error: EstablishError) -> DialError {
        let host = self.target.host_port();
        match error {
            EstablishError::Untrusted { fingerprint } => {
                DialError::UntrustedHostKey { host, fingerprint }
            }
            EstablishError::Changed { expected, got } => DialError::HostKeyChanged {
                host,
                expected,
                got,
            },
            EstablishError::Ssh(error) => self.ssh_error(error),
        }
    }

    fn ssh_error(&self, error: russh::Error) -> DialError {
        match error {
            russh::Error::IO(error) => DialError::Io(error),
            error => {
                let failure = SshFailure::Transport(Some(error.to_string()));
                let message = failure.message(&self.target.host_port());
                DialError::Remote {
                    failure: Some(failure),
                    message,
                }
            }
        }
    }
}

impl Dialer for TailscaleSshDialer {
    fn dial(&self) -> BoxFuture<'_, Result<Duplex, DialError>> {
        self.open().boxed()
    }

    fn describe(&self) -> String {
        self.target.to_string()
    }
}

/// The OpenSSH spelling of a key's fingerprint: `SHA256:<base64>`.
fn fingerprint(key: &PublicKeyOrCertificate) -> String {
    key.public_key().fingerprint(HashAlg::Sha256).to_string()
}

struct Verifier {
    pinned: Option<String>,
}

#[derive(Debug)]
enum EstablishError {
    Ssh(russh::Error),
    Untrusted { fingerprint: String },
    Changed { expected: String, got: String },
}

impl From<russh::Error> for EstablishError {
    fn from(error: russh::Error) -> Self {
        Self::Ssh(error)
    }
}

impl client::Handler for Verifier {
    type Error = EstablishError;

    async fn check_server_key(
        &mut self,
        server_public_key: &PublicKeyOrCertificate,
    ) -> Result<bool, Self::Error> {
        let got = fingerprint(server_public_key);
        match &self.pinned {
            None => Err(EstablishError::Untrusted { fingerprint: got }),
            Some(expected) if *expected == got => Ok(true),
            Some(expected) => Err(EstablishError::Changed {
                expected: expected.clone(),
                got,
            }),
        }
    }
}
