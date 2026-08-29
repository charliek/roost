//! `roostctl host` — manage the client-side saved-host registry
//! (host-sessions HS-2, plan 037 C1).
//!
//! Registry only: `add`, `list`, `remove`. `connect`/`disconnect` land
//! with `HostConn` in a later commit (plan 037 C7) — this is the
//! plumbing they will drive through the same `host.*` ops.
//!
//! Unlike `session`, a saved host is **client-side UI state**
//! (`Workspace::hosts`), not the session daemon's own workspace — so
//! these verbs address the ordinary UI socket the caller already
//! resolved and connected (`main.rs`'s ordinary target-selector
//! prologue), never the session profile.

use anyhow::Result;
use clap::Subcommand;

use roost_ipc::messages::{
    ops, HostAddParams, HostAddResult, HostListResult, HostRemoveParams, SESSION_PROTOCOL_VERSION,
};
use roost_ipc::IpcClient;

use crate::session;

#[derive(Subcommand, Debug)]
pub enum HostCmd {
    /// Save a new host. Registry-only by default: this does not dial
    /// `--target` at all, so a typo'd socket path saves cleanly (the
    /// Hosts sidebar's dot reflects that at the next connect attempt).
    /// `--verify` dials `session.identify` against `--target` first and
    /// refuses to save on an unreachable or incompatible session —
    /// mirroring the Add Host dialog's "Add & Connect" validation.
    Add {
        #[arg(long)]
        label: String,
        #[arg(long)]
        target: String,
        #[arg(long, default_value_t = false)]
        verify: bool,
    },
    /// List saved hosts.
    List {
        #[arg(long, default_value_t = false)]
        json: bool,
    },
    /// Forget a saved host by id.
    Remove {
        #[arg(long)]
        id: String,
    },
}

/// Run a `host` verb against an already-connected UI socket client.
/// Returns the process exit code rather than exiting, matching
/// `session::run`'s shape.
pub async fn run(cmd: &HostCmd, client: &mut IpcClient) -> i32 {
    let result = match cmd {
        HostCmd::Add {
            label,
            target,
            verify,
        } => add(client, label, target, *verify).await,
        HostCmd::List { json } => list(client, *json).await,
        HostCmd::Remove { id } => remove(client, id).await,
    };
    match result {
        Ok(code) => code,
        Err(error) => {
            eprintln!("roostctl host: {error:#}");
            1
        }
    }
}

async fn add(client: &mut IpcClient, label: &str, target: &str, verify: bool) -> Result<i32> {
    if verify {
        if let Err(err) = verify_target(target).await {
            eprintln!("roostctl host add: {target} did not verify: {err:#}");
            return Ok(1);
        }
    }
    let resp: HostAddResult = client
        .call(
            ops::HOST_ADD,
            HostAddParams {
                label: label.to_string(),
                target: target.to_string(),
            },
        )
        .await?;
    println!(
        "added host {} — {} -> {}",
        resp.host.id, resp.host.label, resp.host.target
    );
    Ok(0)
}

/// Dial `target` directly (it is a session socket path, not the UI
/// socket the caller is already connected to) and check it answers
/// `session.identify` with a protocol this build understands. The full
/// compatibility gate (payload kind + exact `libghostty_build` match)
/// belongs to the attach path `HostConn` builds later — this is the
/// coarse "is there even a compatible session here" check `--verify`
/// promises.
///
/// Reuses `session::connect`/`identify_on` rather than dialing
/// `IpcClient` directly: both already wrap the connect and the call in
/// `session`'s CI-scaled timeout, so a wedged or unreachable target
/// fails fast instead of hanging `roostctl`.
async fn verify_target(target: &str) -> Result<()> {
    // "localhost" is the sentinel for this machine's own session (plan
    // 037 §3.5), not a file literally named that. Same mapping the UI's
    // host_conn::resolve_target applies.
    let socket = if target == "localhost" {
        roost_ipc::paths::BundleProfile::session()?.socket_path
    } else {
        std::path::PathBuf::from(target)
    };
    let mut client = session::connect(&socket).await?;
    let identity = session::identify_on(&mut client)
        .await
        .map_err(|e| anyhow::anyhow!("session.identify failed: {e}"))?;
    if identity.session_protocol != SESSION_PROTOCOL_VERSION {
        anyhow::bail!(
            "session protocol mismatch: session speaks {}, this roostctl speaks {}",
            identity.session_protocol,
            SESSION_PROTOCOL_VERSION,
        );
    }
    Ok(())
}

async fn list(client: &mut IpcClient, json: bool) -> Result<i32> {
    let resp: HostListResult = client.call(ops::HOST_LIST, serde_json::json!({})).await?;
    if json {
        println!("{}", serde_json::to_string_pretty(&resp)?);
    } else if resp.hosts.is_empty() {
        println!("no saved hosts");
    } else {
        for h in &resp.hosts {
            let last = h.last_connected.as_deref().unwrap_or("never");
            println!(
                "{}  {}  target={}  last_connected={}",
                h.id, h.label, h.target, last
            );
        }
    }
    Ok(0)
}

async fn remove(client: &mut IpcClient, id: &str) -> Result<i32> {
    client
        .call::<_, serde_json::Value>(ops::HOST_REMOVE, HostRemoveParams { id: id.to_string() })
        .await?;
    println!("removed host {id}");
    Ok(0)
}
