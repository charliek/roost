//! Reference external consumer of `roost-ipc`.
//!
//! Dials a Roost UI socket, `identify`s, and polls `tab.list` once a
//! second until Ctrl-C. Deliberately confined to leaseless UI-socket
//! ops (`identify`, `tab.list`) — no lease, no `events.subscribe`, no
//! session-socket ops — so this program keeps working across the lease
//! and observer changes elsewhere in the protocol. See
//! `docs/reference/ipc-compatibility.md`.

use std::env;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use roost_ipc::messages::{ops, IdentifyParams, TabListResult};
use roost_ipc::{ClientError, IpcClient};

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    // `args_os`, not `args`: a Unix socket path is bytes and need not
    // be UTF-8.
    let mut args = env::args_os();
    let program = args
        .next()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|| "ipc-consumer".to_string());
    let Some(socket_path) = args.next().map(PathBuf::from) else {
        eprintln!("usage: {program} <ui-socket-path>");
        return ExitCode::FAILURE;
    };

    match run(&socket_path).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("{e}");
            ExitCode::FAILURE
        }
    }
}

async fn run(socket_path: &PathBuf) -> Result<(), ClientError> {
    let mut client = IpcClient::connect(socket_path).await?;

    let identity = client.identify(IdentifyParams::default()).await?;
    println!(
        "connected: app={} ({}) protocol={}",
        identity.app_label, identity.ui_version, identity.protocol_version
    );

    let ctrl_c = tokio::signal::ctrl_c();
    tokio::pin!(ctrl_c);
    let mut ticker = tokio::time::interval(Duration::from_secs(1));
    // Delay, not the default Burst: a slow response must not be
    // followed by a run of back-to-back polls.
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            res = &mut ctrl_c => {
                res?;
                return Ok(());
            }
            // Tick + call together in one cancellable branch, so
            // Ctrl-C still lands while a `tab.list` is in flight.
            result = async {
                ticker.tick().await;
                // No params struct: `tab.list` ignores its params
                // entirely, so `()` (serializes to JSON `null`) costs
                // this crate no `serde`/`serde_json` dependency of its
                // own.
                client.call::<_, TabListResult>(ops::TAB_LIST, ()).await
            } => print_summary(&result?),
        }
    }
}

fn print_summary(list: &TabListResult) {
    if list.projects.is_empty() {
        println!("(no projects)");
        return;
    }
    for project in &list.projects {
        if project.tabs.is_empty() {
            println!("project {} ({}) — no tabs", project.name, project.id);
            continue;
        }
        for tab in &project.tabs {
            println!(
                "project {} ({}) tab {} [{:?}] {:?}",
                project.name, project.id, tab.id, tab.state, tab.title
            );
        }
    }
}
