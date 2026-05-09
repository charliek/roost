//! roost-smoke — protocol smoke-test client.
//!
//! Connects to a running roost-core, opens a tab, attaches via `StreamPty`,
//! and pipes the local terminal's stdin/stdout through. The "telnet for
//! our daemon": the cheapest possible way to verify the wire contract
//! end-to-end without involving any UI code.
//!
//! Usage:
//!     roost-smoke                           # uses the default socket
//!     roost-smoke --socket /path/to/sock
//!     roost-smoke --command bash            # spawn a specific shell

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context;
use clap::Parser;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::transport::{Channel, Endpoint};
use tonic::Request;
use tower::service_fn;

use roost_proto::v1::pty_client_message::Kind as PtyClientKind;
use roost_proto::v1::pty_server_message::Kind as PtyServerKind;
use roost_proto::v1::roost_client::RoostClient;
use roost_proto::v1::{
    IdentifyRequest, OpenTabRequest, PtyAttach, PtyClientMessage, PtyInput, PtyResize,
};

#[derive(Parser, Debug)]
#[command(name = "roost-smoke", version, about = "Roost daemon smoke client")]
struct Args {
    /// Unix domain socket path. Defaults to roost-core's default.
    #[arg(long, env = "ROOST_SOCKET")]
    socket: Option<PathBuf>,

    /// Shell command to spawn. Empty = $SHELL on the daemon side.
    #[arg(long, default_value = "")]
    command: String,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "roost_smoke=info,tonic=warn".into()),
        )
        .init();

    let args = Args::parse();
    let socket = args
        .socket
        .clone()
        .or_else(default_socket_path)
        .context("could not determine default socket path")?;

    let channel = connect_uds(socket.clone()).await?;
    let mut client = RoostClient::new(channel);

    // Handshake.
    let identity = client
        .identify(IdentifyRequest {
            client_name: "roost-smoke".into(),
            client_version: env!("CARGO_PKG_VERSION").into(),
        })
        .await
        .context("Identify RPC failed")?
        .into_inner();
    eprintln!(
        "[roost-smoke] connected: pid={} daemon={} proto={} socket={}",
        identity.pid, identity.daemon_version, identity.protocol_version, identity.socket_path
    );

    // Open a tab.
    let cwd = std::env::current_dir()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_default();
    let open = client
        .open_tab(OpenTabRequest {
            project_id: 0,
            cwd: cwd.clone(),
            command: args.command.clone(),
            cols: 80,
            rows: 24,
            title: "smoke".into(),
        })
        .await
        .context("OpenTab RPC failed")?
        .into_inner();
    let tab = open.tab.context("OpenTab returned no tab")?;
    eprintln!("[roost-smoke] tab opened: id={} cwd={}", tab.id, tab.cwd);

    // Attach via StreamPty.
    let (tx, rx) = mpsc::channel::<PtyClientMessage>(64);
    let outbound = ReceiverStream::new(rx);

    tx.send(PtyClientMessage {
        kind: Some(PtyClientKind::Attach(PtyAttach {
            tab_id: tab.id,
            cols: 80,
            rows: 24,
        })),
    })
    .await
    .context("send PtyAttach")?;

    let response = client
        .stream_pty(Request::new(outbound))
        .await
        .context("StreamPty RPC failed")?;
    let mut inbound = response.into_inner();

    // Pump stdin -> input.
    let tx_for_stdin = tx.clone();
    tokio::spawn(async move {
        let mut stdin = tokio::io::stdin();
        let mut buf = [0u8; 1024];
        loop {
            match stdin.read(&mut buf).await {
                Ok(0) => break,
                Ok(n) => {
                    if tx_for_stdin
                        .send(PtyClientMessage {
                            kind: Some(PtyClientKind::Input(PtyInput {
                                data: buf[..n].to_vec(),
                            })),
                        })
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });

    // Optional: forward terminal resizes. Phase 3 keeps it simple — just
    // send the initial size we asserted on attach. Real resize wiring
    // (SIGWINCH) is a Phase 5 concern when the Mac UI is in the picture.
    let _ = tx
        .send(PtyClientMessage {
            kind: Some(PtyClientKind::Resize(PtyResize { cols: 80, rows: 24 })),
        })
        .await;

    // Pump output -> stdout.
    let mut stdout = tokio::io::stdout();
    let mut exit_code = 0i32;
    while let Some(msg) = inbound.message().await? {
        match msg.kind {
            Some(PtyServerKind::Output(o)) => {
                stdout.write_all(&o.data).await?;
                stdout.flush().await?;
            }
            Some(PtyServerKind::Exit(e)) => {
                exit_code = e.status;
                break;
            }
            None => {}
        }
    }

    eprintln!("\n[roost-smoke] tab exited (status={exit_code})");
    std::process::exit(exit_code);
}

fn default_socket_path() -> Option<PathBuf> {
    if cfg!(target_os = "macos") {
        std::env::var_os("HOME").map(|home| {
            PathBuf::from(home)
                .join("Library/Caches/roost")
                .join("roost.sock")
        })
    } else {
        std::env::var_os("XDG_RUNTIME_DIR")
            .map(|dir| PathBuf::from(dir).join("roost").join("roost.sock"))
            .or_else(|| {
                let uid = libc_getuid();
                Some(PathBuf::from(format!("/tmp/roost-{uid}")).join("roost.sock"))
            })
    }
}

#[cfg(unix)]
extern "C" {
    fn getuid() -> u32;
}

#[cfg(unix)]
fn libc_getuid() -> u32 {
    unsafe { getuid() }
}

#[cfg(not(unix))]
fn libc_getuid() -> u32 {
    0
}

/// Build a tonic `Channel` over a Unix domain socket.
///
/// `Endpoint::connect_with_connector` lets us plug in a custom service that
/// returns a Tokio `UnixStream` instead of a TCP one. The URL is irrelevant
/// — tonic only uses it for routing — but it must be a syntactically valid
/// http URI.
async fn connect_uds(path: PathBuf) -> anyhow::Result<Channel> {
    let path = Arc::new(path);
    let endpoint = Endpoint::from_static("http://[::]:0");
    let channel = endpoint
        .connect_with_connector(service_fn(move |_| {
            let path = path.clone();
            async move {
                let stream = tokio::net::UnixStream::connect(&*path).await?;
                let io = hyper_util::rt::TokioIo::new(stream);
                Ok::<_, std::io::Error>(io)
            }
        }))
        .await
        .context("connect_with_connector(uds)")?;
    Ok(channel)
}
