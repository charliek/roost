//! roost-cli-rs — shell-integration CLI for roost-core.
//!
//! Mirrors the legacy `cmd/roost-cli` subcommands so the user-facing UX
//! survives the rewrite intact:
//!
//!   roost-cli-rs notify --title TITLE [--body BODY] [--tab ID]
//!   roost-cli-rs set-title --title TITLE [--tab ID]
//!   roost-cli-rs identify
//!   roost-cli-rs tab focus [--tab ID]
//!   roost-cli-rs tab list
//!   roost-cli-rs tab set-state --state STATE [--tab ID]
//!
//! Renamed to `roost-cli` in the Phase 9 cutover (when the legacy Go
//! binary is deleted).

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use tonic::transport::{Channel, Endpoint};
use tower::service_fn;

use roost_proto::v1::roost_client::RoostClient;
use roost_proto::v1::{
    ClearTabNotificationRequest, CreateNotificationRequest, FocusTabRequest, IdentifyRequest,
    ListTabsRequest, SetTabStateRequest, SetTabTitleRequest, TabState,
};

#[derive(Parser, Debug)]
#[command(name = "roost-cli-rs", version, about = "Roost shell-integration CLI")]
struct Args {
    #[arg(long, env = "ROOST_SOCKET")]
    socket: Option<PathBuf>,

    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Fire a notification on a tab.
    Notify {
        #[arg(long)]
        title: String,
        #[arg(long, default_value = "")]
        body: String,
        #[arg(long, env = "ROOST_TAB_ID")]
        tab: Option<i64>,
    },
    /// Rename a tab (locks it from OSC overwrites).
    SetTitle {
        #[arg(long)]
        title: String,
        #[arg(long, env = "ROOST_TAB_ID")]
        tab: Option<i64>,
    },
    /// Print the daemon's identity (socket, pid, active tab, version).
    Identify,
    /// Tab subcommands.
    #[command(subcommand)]
    Tab(TabCmd),
}

#[derive(Subcommand, Debug)]
enum TabCmd {
    Focus {
        #[arg(long, env = "ROOST_TAB_ID")]
        tab: Option<i64>,
    },
    List,
    SetState {
        #[arg(long, value_parser = ["none", "running", "needs_input", "idle"])]
        state: String,
        #[arg(long, env = "ROOST_TAB_ID")]
        tab: Option<i64>,
    },
    ClearNotification {
        #[arg(long, env = "ROOST_TAB_ID")]
        tab: Option<i64>,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "warn".into()),
        )
        .init();

    let args = Args::parse();
    let socket = args
        .socket
        .clone()
        .or_else(default_socket_path)
        .context("could not resolve roost-core socket path")?;
    let channel = connect_uds(socket).await?;
    let mut client = RoostClient::new(channel);

    match args.command {
        Cmd::Notify { title, body, tab } => {
            let tab_id = tab.unwrap_or(0);
            client
                .create_notification(CreateNotificationRequest {
                    tab_id,
                    title,
                    body,
                })
                .await?;
        }
        Cmd::SetTitle { title, tab } => {
            let tab_id = tab.unwrap_or(0);
            client
                .set_tab_title(SetTabTitleRequest { tab_id, title })
                .await?;
        }
        Cmd::Identify => {
            let resp = client
                .identify(IdentifyRequest {
                    client_name: "roost-cli-rs".into(),
                    client_version: env!("CARGO_PKG_VERSION").into(),
                })
                .await?
                .into_inner();
            println!(
                "socket={}\npid={}\nactive_project={}\nactive_tab={}\ndaemon_version={}\nproto_version={}",
                resp.socket_path,
                resp.pid,
                resp.active_project_id,
                resp.active_tab_id,
                resp.daemon_version,
                resp.protocol_version
            );
        }
        Cmd::Tab(TabCmd::Focus { tab }) => {
            let tab_id = tab.unwrap_or(0);
            client.focus_tab(FocusTabRequest { tab_id }).await?;
        }
        Cmd::Tab(TabCmd::List) => {
            let resp = client.list_tabs(ListTabsRequest {}).await?.into_inner();
            for project in resp.projects {
                println!("project {} — {}", project.id, project.name);
                for tab in project.tabs {
                    println!(
                        "  tab {} [{}] {} cwd={}",
                        tab.id,
                        format_state(tab.state),
                        tab.title,
                        tab.cwd
                    );
                }
            }
        }
        Cmd::Tab(TabCmd::SetState { state, tab }) => {
            let tab_id = tab.unwrap_or(0);
            let state = parse_state(&state)?;
            client
                .set_tab_state(SetTabStateRequest {
                    tab_id,
                    state: state as i32,
                })
                .await?;
        }
        Cmd::Tab(TabCmd::ClearNotification { tab }) => {
            let tab_id = tab.unwrap_or(0);
            client
                .clear_tab_notification(ClearTabNotificationRequest { tab_id })
                .await?;
        }
    }

    Ok(())
}

fn parse_state(s: &str) -> Result<TabState> {
    Ok(match s {
        "none" => TabState::None,
        "running" => TabState::Running,
        "needs_input" => TabState::NeedsInput,
        "idle" => TabState::Idle,
        other => anyhow::bail!("unknown state '{other}'"),
    })
}

fn format_state(state: i32) -> &'static str {
    match TabState::try_from(state) {
        Ok(TabState::None) => "none",
        Ok(TabState::Running) => "running",
        Ok(TabState::NeedsInput) => "needs_input",
        Ok(TabState::Idle) => "idle",
        _ => "?",
    }
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

async fn connect_uds(path: PathBuf) -> Result<Channel> {
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
        .context("connect uds")?;
    Ok(channel)
}
