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

use anyhow::Result;
use clap::{Parser, Subcommand};

use roost_common::{connect_uds, default_socket_path};

use roost_proto::v1::roost_client::RoostClient;
use roost_proto::v1::{
    ClearTabNotificationRequest, CreateNotificationRequest, CreateProjectRequest,
    DeleteProjectRequest, FocusTabRequest, IdentifyRequest, ListTabsRequest, RenameProjectRequest,
    SetTabStateRequest, SetTabTitleRequest, TabState,
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
    /// Project subcommands.
    #[command(subcommand)]
    Project(ProjectCmd),
}

#[derive(Subcommand, Debug)]
enum ProjectCmd {
    /// List all projects (without their tabs — `tab list` for that).
    List,
    /// Create a project. Empty `--name` defaults to "Untitled <n>".
    Create {
        #[arg(long, default_value = "")]
        name: String,
        #[arg(long, default_value = "")]
        cwd: String,
    },
    /// Rename a project.
    Rename {
        #[arg(long)]
        id: i64,
        #[arg(long)]
        name: String,
    },
    /// Delete a project (cascade-deletes its tabs).
    Delete {
        #[arg(long)]
        id: i64,
    },
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
    let socket = match args.socket.clone() {
        Some(p) => p,
        None => default_socket_path()?,
    };
    let channel = connect_uds(socket).await?;
    let mut client = RoostClient::new(channel);

    match args.command {
        Cmd::Notify { title, body, tab } => {
            let tab_id = resolve_tab(&mut client, tab).await?;
            client
                .create_notification(CreateNotificationRequest {
                    tab_id,
                    title,
                    body,
                })
                .await?;
        }
        Cmd::SetTitle { title, tab } => {
            let tab_id = resolve_tab(&mut client, tab).await?;
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
            let tab_id = resolve_tab(&mut client, tab).await?;
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
            let tab_id = resolve_tab(&mut client, tab).await?;
            let state = parse_state(&state)?;
            client
                .set_tab_state(SetTabStateRequest {
                    tab_id,
                    state: state as i32,
                })
                .await?;
        }
        Cmd::Tab(TabCmd::ClearNotification { tab }) => {
            let tab_id = resolve_tab(&mut client, tab).await?;
            client
                .clear_tab_notification(ClearTabNotificationRequest { tab_id })
                .await?;
        }
        Cmd::Project(ProjectCmd::List) => {
            let resp = client.list_tabs(ListTabsRequest {}).await?.into_inner();
            for p in resp.projects {
                println!(
                    "project {} — {}  cwd={}  tabs={}",
                    p.id,
                    p.name,
                    p.cwd,
                    p.tabs.len()
                );
            }
        }
        Cmd::Project(ProjectCmd::Create { name, cwd }) => {
            let resp = client
                .create_project(CreateProjectRequest { name, cwd })
                .await?
                .into_inner();
            let p = resp.project.unwrap_or_default();
            println!("created project {} — {}", p.id, p.name);
        }
        Cmd::Project(ProjectCmd::Rename { id, name }) => {
            client
                .rename_project(RenameProjectRequest {
                    project_id: id,
                    name,
                })
                .await?;
        }
        Cmd::Project(ProjectCmd::Delete { id }) => {
            client
                .delete_project(DeleteProjectRequest { project_id: id })
                .await?;
        }
    }

    Ok(())
}

/// Resolve the tab id for a per-tab command. If the user passed
/// `--tab` (or set `ROOST_TAB_ID`), use that verbatim. Otherwise ask
/// the daemon via `Identify()` for the active tab and use that.
///
/// Without this helper the CLI would silently send `tab_id = 0` to the
/// server when no tab was specified, and the server treats `0` as a
/// real tab id to look up — yielding `TabNotFound(0)` for every
/// per-tab command run without `--tab`. Calling `Identify()` is the
/// same one round-trip pattern the legacy Go CLI used.
async fn resolve_tab(
    client: &mut RoostClient<tonic::transport::Channel>,
    explicit: Option<i64>,
) -> Result<i64> {
    if let Some(id) = explicit {
        return Ok(id);
    }
    let resp = client
        .identify(IdentifyRequest {
            client_name: "roost-cli-rs".into(),
            client_version: env!("CARGO_PKG_VERSION").into(),
        })
        .await?
        .into_inner();
    if resp.active_tab_id == 0 {
        anyhow::bail!(
            "no --tab specified and the daemon reports no active tab; \
             pass --tab or set ROOST_TAB_ID"
        );
    }
    Ok(resp.active_tab_id)
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

// default_socket_path / connect_uds are now imported from roost-common
// — single source of truth shared with the daemon and roost-smoke. See
// crates/roost-common/src/lib.rs.
