//! A stand-in for tailscaled's SSH server on a development machine, so
//! the app in an emulator (or on a phone over `adb reverse`) reaches a
//! dev `roost-session` with no tailnet in between.
//!
//! It takes the `none` login for any user, as Tailscale SSH does, and
//! plays `client-bridge` too: every exec, whatever its command, is piped
//! to one session socket. Without the bridge's preamble, so the session
//! sees an ordinary local client. The host key persists in a file, so a
//! pin the app saved stays good across restarts.
//!
//! `examples/dev_sshd.rs` is its command line; the crate's test fixture
//! shares [`config`] and [`pipe`].

use std::io::{self, Read, Write};
use std::net::SocketAddr;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use russh::keys::ssh_key::private::Ed25519Keypair;
use russh::keys::ssh_key::LineEnding;
use russh::keys::{HashAlg, PrivateKey};
use russh::server::{self, Auth, Msg, Session};
use russh::{Channel, ChannelId, ChannelMsg, ChannelReadHalf, ChannelWriteHalf};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, UnixStream};

/// The standard stderr stream number (RFC 4254 §5.2).
const STDERR: u32 = 1;

/// The ed25519 host key at `path`, made on first use (mode 0600, parent
/// directories created) and read back on every use after.
pub fn load_or_create_key(path: &Path) -> io::Result<PrivateKey> {
    match std::fs::read(path) {
        Ok(pem) => return PrivateKey::from_openssh(pem).map_err(invalid_data),
        Err(error) if error.kind() != io::ErrorKind::NotFound => return Err(error),
        Err(_) => {}
    }
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)?;
    }
    let mut seed = [0u8; 32];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut seed)?;
    let key = PrivateKey::from(Ed25519Keypair::from_seed(&seed));
    let pem = key.to_openssh(LineEnding::LF).map_err(invalid_data)?;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(pem.as_bytes())?;
    file.sync_all()?;
    Ok(key)
}

/// The OpenSSH spelling of the key's fingerprint, `SHA256:<base64>`:
/// what the app shows on first connect, and pins.
pub fn fingerprint(key: &PrivateKey) -> String {
    key.public_key().fingerprint(HashAlg::Sha256).to_string()
}

/// The server's settings around `key`. A dead client is dropped after
/// about a minute of unanswered keepalives, the dialer's own budget.
pub fn config(key: PrivateKey) -> Arc<server::Config> {
    Arc::new(server::Config {
        keys: vec![key],
        auth_rejection_time: Duration::from_millis(1),
        auth_rejection_time_initial: Some(Duration::ZERO),
        inactivity_timeout: None,
        keepalive_interval: Some(Duration::from_secs(15)),
        keepalive_max: 3,
        nodelay: true,
        ..server::Config::default()
    })
}

/// Accept on `listener` until it fails, piping every exec on every
/// connection to `socket`.
pub async fn serve(listener: TcpListener, key: PrivateKey, socket: PathBuf) -> io::Result<()> {
    let config = config(key);
    let socket = Arc::new(socket);
    loop {
        let (stream, peer) = listener.accept().await?;
        let handler = Connection {
            socket: socket.clone(),
            peer,
        };
        let config = config.clone();
        tokio::spawn(async move {
            match server::run_stream(config, stream, handler).await {
                Ok(session) => {
                    if let Err(error) = session.await {
                        tracing::debug!(%peer, %error, "connection ended");
                    }
                }
                Err(error) => tracing::warn!(%peer, %error, "SSH handshake failed"),
            }
        });
    }
}

struct Connection {
    socket: Arc<PathBuf>,
    peer: SocketAddr,
}

impl server::Handler for Connection {
    type Error = russh::Error;

    async fn auth_none(&mut self, user: &str) -> Result<Auth, Self::Error> {
        tracing::info!(peer = %self.peer, user, "logged in");
        Ok(Auth::Accept)
    }

    async fn channel_open_session(
        &mut self,
        channel: Channel<Msg>,
        reply: server::ChannelOpenHandle,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        reply.accept().await;
        tokio::spawn(serve_channel(channel, self.socket.clone()));
        Ok(())
    }

    async fn exec_request(
        &mut self,
        channel: ChannelId,
        _command: &[u8],
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        session.channel_success(channel)
    }
}

async fn serve_channel(mut channel: Channel<Msg>, socket: Arc<PathBuf>) {
    loop {
        match channel.wait().await {
            Some(ChannelMsg::Exec { .. }) => break,
            Some(_) => continue,
            None => return,
        }
    }
    let (read, write) = channel.split();
    pipe(read, write, &socket).await;
}

/// Pump an exec'd channel to and from a new connection to `socket`, and
/// end it the way `client-bridge` ends: exit 0 once the session closes
/// the connection, or, with nothing listening, the bridge's one-line
/// `client-bridge: no session …` on stderr and exit 1, which the dialer
/// classifies as [`roost_ipc::ssh::SshFailure::NoSession`].
pub async fn pipe(mut read: ChannelReadHalf, write: ChannelWriteHalf<Msg>, socket: &Path) {
    let session = match UnixStream::connect(socket).await {
        Ok(session) => session,
        Err(error) => {
            let line = format!(
                "client-bridge: no session is listening at {} ({error})\n",
                socket.display()
            );
            let _ = write.extended_data_bytes(STDERR, Bytes::from(line)).await;
            let _ = write.exit_status(1).await;
            let _ = write.eof().await;
            let _ = write.close().await;
            return;
        }
    };
    let (mut from_session, mut to_session) = session.into_split();
    tokio::spawn(async move {
        while let Some(msg) = read.wait().await {
            match msg {
                ChannelMsg::Data { data } => {
                    if to_session.write_all(&data).await.is_err() {
                        return;
                    }
                }
                ChannelMsg::Eof => {
                    let _ = to_session.shutdown().await;
                    return;
                }
                _ => {}
            }
        }
    });
    let mut buf = vec![0; 64 * 1024];
    while let Ok(n @ 1..) = from_session.read(&mut buf).await {
        if write
            .data_bytes(Bytes::copy_from_slice(&buf[..n]))
            .await
            .is_err()
        {
            return;
        }
    }
    let _ = write.exit_status(0).await;
    let _ = write.eof().await;
    let _ = write.close().await;
}

fn invalid_data(error: impl std::error::Error + Send + Sync + 'static) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error)
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    #[test]
    fn the_host_key_is_made_once_and_kept() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("keys").join("host_ed25519");
        let made = load_or_create_key(&path).expect("make the key");
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "the key file is owner-only");
        let read = load_or_create_key(&path).expect("read the key back");
        assert_eq!(fingerprint(&made), fingerprint(&read));
        assert!(fingerprint(&made).starts_with("SHA256:"));
    }

    #[test]
    fn a_key_file_that_is_not_a_key_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("host_ed25519");
        std::fs::write(&path, "not a key").unwrap();
        let error = load_or_create_key(&path).expect_err("garbage is not a key");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "not a key");
    }
}
