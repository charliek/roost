//! An in-process SSH server standing in for tailscaled's: binds
//! loopback, takes (or refuses) the `none` login, and answers every exec
//! the way the test asked — an echo, a failed `client-bridge`, or a pipe
//! to a real session socket.

#![allow(dead_code)]

use std::net::{Shutdown, SocketAddr};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use roost_client::dial::{DialError, Duplex};
use roost_ipc::framing::{write_frame, FrameReader};
use roost_ipc::messages::{ops, RawRequest, Response, SessionIdentify};
use roost_ssh::{HostKeyPolicy, HostTarget, TailscaleSshDialer};
use russh::keys::ssh_key::private::Ed25519Keypair;
use russh::keys::{HashAlg, PrivateKey};
use russh::server::{self, Auth, Msg, Session};
use russh::{Channel, ChannelId, ChannelMsg};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, UnixStream};
use tokio::task::JoinHandle;

/// What the server does with an exec'd command.
#[derive(Debug, Clone)]
pub enum Behavior {
    /// Echo stdin to stdout; exit 0 at stdin's EOF.
    Echo,
    /// Write `stderr` and exit `code` without reading anything — the
    /// shape of a `client-bridge` that found no session.
    Fail { stderr: String, code: u32 },
    /// Pump the channel to and from the Unix socket at this path, the
    /// way `client-bridge` does; exit 0 when the socket closes.
    Pipe(PathBuf),
}

pub const NO_SESSION_STDERR: &str = "client-bridge: no session is listening at /x/roost.sock; \
                                     run 'roostctl session start' on this machine\n";

pub struct Server {
    pub addr: SocketAddr,
    pub fingerprint: String,
    behavior: Arc<Mutex<Behavior>>,
    connections: Arc<AtomicUsize>,
    commands: Arc<Mutex<Vec<String>>>,
    sockets: Arc<Mutex<Vec<std::net::TcpStream>>>,
    accept: JoinHandle<()>,
}

impl Server {
    pub async fn start(behavior: Behavior) -> Self {
        Self::start_with(behavior, true).await
    }

    pub async fn start_with(behavior: Behavior, accept_none: bool) -> Self {
        let behavior = Arc::new(Mutex::new(behavior));
        let key = PrivateKey::from(Ed25519Keypair::from_seed(&[7; 32]));
        let fingerprint = key.public_key().fingerprint(HashAlg::Sha256).to_string();
        let config = Arc::new(server::Config {
            keys: vec![key],
            auth_rejection_time: Duration::from_millis(1),
            auth_rejection_time_initial: Some(Duration::ZERO),
            inactivity_timeout: None,
            ..server::Config::default()
        });
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("local addr");
        let connections = Arc::new(AtomicUsize::new(0));
        let commands = Arc::new(Mutex::new(Vec::new()));
        let sockets = Arc::new(Mutex::new(Vec::new()));
        let accept = tokio::spawn({
            let behavior = behavior.clone();
            let connections = connections.clone();
            let commands = commands.clone();
            let sockets = sockets.clone();
            async move {
                loop {
                    let Ok((stream, _)) = listener.accept().await else {
                        return;
                    };
                    connections.fetch_add(1, Ordering::SeqCst);
                    // A second handle on the same socket, so a test can
                    // cut the TCP connection out from under both sides.
                    let std = stream.into_std().expect("into std");
                    sockets
                        .lock()
                        .unwrap()
                        .push(std.try_clone().expect("clone the socket"));
                    let stream = tokio::net::TcpStream::from_std(std).expect("from std");
                    let handler = Connection {
                        behavior: behavior.clone(),
                        accept_none,
                        commands: commands.clone(),
                    };
                    let config = config.clone();
                    tokio::spawn(async move {
                        if let Ok(session) = server::run_stream(config, stream, handler).await {
                            let _ = session.await;
                        }
                    });
                }
            }
        });
        Self {
            addr,
            fingerprint,
            behavior,
            connections,
            commands,
            sockets,
            accept,
        }
    }

    /// What every exec from here on does.
    pub fn set_behavior(&self, behavior: Behavior) {
        *self.behavior.lock().unwrap() = behavior;
    }

    /// TCP connections accepted so far.
    pub fn connections(&self) -> usize {
        self.connections.load(Ordering::SeqCst)
    }

    /// Every exec'd command, in arrival order.
    pub fn commands(&self) -> Vec<String> {
        self.commands.lock().unwrap().clone()
    }

    /// Cut every TCP connection, with no SSH goodbye.
    pub fn drop_connections(&self) {
        for socket in self.sockets.lock().unwrap().drain(..) {
            let _ = socket.shutdown(Shutdown::Both);
        }
    }

    pub fn target(&self) -> HostTarget {
        HostTarget::parse(&format!("tester@127.0.0.1:{}", self.addr.port()), "").unwrap()
    }

    pub fn dialer(&self, pinned: Option<&str>) -> TailscaleSshDialer {
        TailscaleSshDialer::new(
            self.target(),
            HostKeyPolicy {
                pinned: pinned.map(str::to_string),
            },
        )
    }

    pub fn pinned_dialer(&self) -> TailscaleSshDialer {
        self.dialer(Some(&self.fingerprint))
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.accept.abort();
        self.drop_connections();
    }
}

struct Connection {
    behavior: Arc<Mutex<Behavior>>,
    accept_none: bool,
    commands: Arc<Mutex<Vec<String>>>,
}

impl server::Handler for Connection {
    type Error = russh::Error;

    async fn auth_none(&mut self, _user: &str) -> Result<Auth, Self::Error> {
        Ok(if self.accept_none {
            Auth::Accept
        } else {
            Auth::reject()
        })
    }

    async fn channel_open_session(
        &mut self,
        channel: Channel<Msg>,
        reply: server::ChannelOpenHandle,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        reply.accept().await;
        let behavior = self.behavior.lock().unwrap().clone();
        tokio::spawn(serve_channel(channel, behavior, self.commands.clone()));
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

async fn serve_channel(
    mut channel: Channel<Msg>,
    behavior: Behavior,
    commands: Arc<Mutex<Vec<String>>>,
) {
    let command = loop {
        match channel.wait().await {
            Some(ChannelMsg::Exec { command, .. }) => {
                break String::from_utf8_lossy(&command).into_owned()
            }
            Some(_) => continue,
            None => return,
        }
    };
    commands.lock().unwrap().push(command);
    let (mut read, write) = channel.split();
    match behavior {
        Behavior::Echo => loop {
            match read.wait().await {
                Some(ChannelMsg::Data { data }) => {
                    let _ = write.data_bytes(data).await;
                }
                Some(ChannelMsg::Eof) | None => break,
                Some(_) => {}
            }
        },
        Behavior::Fail { stderr, code } => {
            let _ = write.extended_data_bytes(1, Bytes::from(stderr)).await;
            let _ = write.exit_status(code).await;
            let _ = write.eof().await;
            let _ = write.close().await;
            return;
        }
        Behavior::Pipe(path) => {
            let socket = UnixStream::connect(&path).await.expect("dial the session");
            let (mut from_session, mut to_session) = socket.into_split();
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
        }
    }
    let _ = write.exit_status(0).await;
    let _ = write.eof().await;
    let _ = write.close().await;
}

/// `session.identify` over one dialed stream, as a bare request line.
pub async fn identify(duplex: Duplex) -> Result<SessionIdentify, String> {
    let Duplex { read, mut write } = duplex;
    let request = RawRequest {
        id: 1,
        op: ops::SESSION_IDENTIFY.into(),
        params: serde_json::json!({}),
    };
    let line = serde_json::to_vec(&request).unwrap();
    write_frame(&mut write, &line)
        .await
        .map_err(|e| e.to_string())?;
    let mut reader = FrameReader::new(read);
    let frame = tokio::time::timeout(Duration::from_secs(20), reader.read_line())
        .await
        .map_err(|_| "no reply within 20s")?
        .map_err(|e| e.to_string())?
        .ok_or("the stream closed before the reply")?;
    let response: Response = serde_json::from_slice(&frame).map_err(|e| e.to_string())?;
    if !response.ok {
        return Err(format!("identify refused: {:?}", response.error));
    }
    serde_json::from_value(response.result.unwrap_or_default()).map_err(|e| e.to_string())
}

/// Unwrap a dial's error, failing the test on success.
pub fn dial_error(result: Result<Duplex, DialError>) -> DialError {
    match result {
        Ok(_) => panic!("the dial succeeded"),
        Err(error) => error,
    }
}
