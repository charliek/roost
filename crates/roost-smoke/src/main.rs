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
//!     roost-smoke --arg bash --arg --login  # spawn a specific argv

use anyhow::Context;
use clap::Parser;
use std::path::PathBuf;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::Request;

use roost_common::{connect_uds, default_socket_path};
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

    /// Argv to spawn. Repeat the flag for each token, e.g.
    /// `--arg bash --arg --login`. Empty = the daemon picks $SHELL.
    #[arg(long = "arg")]
    argv: Vec<String>,
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
    let socket = match args.socket.clone() {
        Some(p) => p,
        None => default_socket_path()?,
    };

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
            argv: args.argv.clone(),
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

// default_socket_path / connect_uds are now imported from roost-common
// — single source of truth shared with the daemon and roost-cli-rs. See
// crates/roost-common/src/lib.rs.
