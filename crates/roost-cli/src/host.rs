//! `roostctl host` — manage the client-side saved-host registry
//! (host-sessions HS-2, plan 037 C1).
//!
//! Five verbs — `add`, `list`, `remove`, `connect`, `disconnect` — each
//! driving the same `host.*` op the UI's own palette row does, so the
//! CLI can never diverge from what a click does (plan 037 §3.5's
//! ops-parity rule).
//!
//! `Stop Session` is the one palette verb with no equivalent here: it
//! goes straight onto the host's own queue as `session.stop`, so there
//! is no client-side `host.*` op for a CLI to drive yet.
//!
//! Unlike `session`, a saved host is **client-side UI state**
//! (`Workspace::hosts`), not the session daemon's own workspace — so
//! these verbs address the ordinary UI socket the caller already
//! resolved and connected (`main.rs`'s ordinary target-selector
//! prologue), never the session profile.

use anyhow::Result;
use clap::Subcommand;

use roost_ipc::messages::{
    ops, HostAddParams, HostAddResult, HostConnectParams, HostConnectionResult, HostListResult,
    HostRemoveParams,
};
use roost_ipc::{session_launch, IpcClient};

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
    /// Connect a saved host by id. Unconditional takeover (reconnect IS
    /// takeover on this wire), and on localhost it starts the session if
    /// it is not already running. Returns once the attempt is under way
    /// — watch the sidebar or `host list` for the settled state.
    Connect {
        #[arg(long)]
        id: String,
    },
    /// Drop a saved host's connection. Never stops the session: its
    /// shells keep running and reconnecting picks them back up.
    Disconnect {
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
        HostCmd::Connect { id } | HostCmd::Disconnect { id } => {
            connection(client, op_for(cmd), id).await
        }
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
/// `session.identify` with a protocol this build understands.
///
/// The check is [`roost_ipc::session_launch::verify_target`], which the
/// Add Host dialog's "Add & Connect" also calls — including the
/// `localhost` sentinel resolution, so a target can never mean two
/// sockets depending on which binary read it, and `--verify` can never
/// promise a different bar than the dialog does (plan 037 §3.5). The
/// budget is `session`'s CI-scaled one, so a wedged or unreachable
/// target fails fast instead of hanging `roostctl`.
async fn verify_target(target: &str) -> Result<()> {
    let budget = session_launch::IPC_TIMEOUT.mul_f64(session_launch::timeout_scale());
    session_launch::verify_target(target, budget)
        .await
        .map(drop)
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

/// Which `host.*` op a verb drives. Split out so the arg → op mapping
/// is assertable without a socket: plan 037 §7's ops-parity claim is
/// "every palette verb has a `roostctl host` equivalent driving the
/// **same op**", and a test that only checked the subcommand parses
/// would not have checked that.
///
/// `run` dispatches the two connection verbs through it rather than
/// naming their ops again, so the test below is asserting the mapping
/// the CLI actually uses.
fn op_for(cmd: &HostCmd) -> &'static str {
    match cmd {
        HostCmd::Add { .. } => ops::HOST_ADD,
        HostCmd::List { .. } => ops::HOST_LIST,
        HostCmd::Remove { .. } => ops::HOST_REMOVE,
        HostCmd::Connect { .. } => ops::HOST_CONNECT,
        HostCmd::Disconnect { .. } => ops::HOST_DISCONNECT,
    }
}

/// `connect` and `disconnect` differ only in the op they name and the
/// state they report back, so they share one body — the wire shapes are
/// identical by design (`{id}` in, `{host, state}` out).
async fn connection(client: &mut IpcClient, op: &str, id: &str) -> Result<i32> {
    let resp: HostConnectionResult = client
        .call(op, HostConnectParams { id: id.to_string() })
        .await?;
    println!("{}  {}  {}", resp.host.id, resp.host.label, resp.state);
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    /// A throwaway root so `clap` parses the subcommand exactly as
    /// `roostctl host …` does, without dragging the real `Cli` (and its
    /// global target-selector flags) into this file's tests.
    #[derive(Parser, Debug)]
    struct Root {
        #[command(subcommand)]
        host: HostCmd,
    }

    fn parse(args: &[&str]) -> HostCmd {
        let mut argv = vec!["host"];
        argv.extend_from_slice(args);
        Root::parse_from(argv).host
    }

    #[test]
    fn every_verb_maps_to_its_own_host_op() {
        for (args, op) in [
            ("add --label l --target t", ops::HOST_ADD),
            ("list", ops::HOST_LIST),
            ("remove --id abc", ops::HOST_REMOVE),
            ("connect --id abc", ops::HOST_CONNECT),
            ("disconnect --id abc", ops::HOST_DISCONNECT),
        ] {
            let cmd = parse(&args.split(' ').collect::<Vec<_>>());
            assert_eq!(op_for(&cmd), op, "roostctl host {args}");
        }
    }

    /// The two connection verbs address a host by its saved id — the
    /// same handle `host list` prints and the sidebar's ↻ row carries.
    /// A label would be ambiguous the moment two hosts share one.
    #[test]
    fn connection_verbs_carry_the_saved_id() {
        match parse(&["connect", "--id", "3f9a2b7c1d4e4f5a"]) {
            HostCmd::Connect { id } => assert_eq!(id, "3f9a2b7c1d4e4f5a"),
            other => panic!("expected connect, got {other:?}"),
        }
        match parse(&["disconnect", "--id", "3f9a2b7c1d4e4f5a"]) {
            HostCmd::Disconnect { id } => assert_eq!(id, "3f9a2b7c1d4e4f5a"),
            other => panic!("expected disconnect, got {other:?}"),
        }
    }

    /// `--verify` is opt-in: the default save is registry-only, so a
    /// typo'd socket path still saves and the sidebar's dot is what
    /// reports it (documented in `reference/cli.md`).
    #[test]
    fn add_verifies_only_when_asked() {
        assert!(matches!(
            parse(&["add", "--label", "l", "--target", "t"]),
            HostCmd::Add { verify: false, .. },
        ));
        assert!(matches!(
            parse(&["add", "--label", "l", "--target", "t", "--verify"]),
            HostCmd::Add { verify: true, .. },
        ));
    }
}
