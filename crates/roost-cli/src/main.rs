//! roostctl — shell-integration CLI for the Roost UIs (Mac + Linux).
//!
//! Talks JSON over a Unix-domain socket directly to the running UI
//! process; there is no daemon. The wire format is documented in
//! `docs/reference/ipc.md`. Subcommands mirror the gRPC-era surface
//! so existing scripts, Claude hooks, and shell aliases keep working:
//!
//!   roostctl notify --title TITLE [--body BODY] [--tab ID]
//!   roostctl set-title --title TITLE [--tab ID]
//!   roostctl identify
//!   roostctl tab focus [--tab ID]
//!   roostctl tab list
//!   roostctl tab set-state --state STATE [--tab ID]
//!   roostctl tab open --project-id N [--cwd …] [--cols 80] [--rows 24]
//!   roostctl tab close [--tab ID]
//!   roostctl tab send [--tab ID] --bytes 'echo hi\n' [--raw]
//!   roostctl tab send [--tab ID] --bytes-base64 BASE64
//!   roostctl tab resize [--tab ID] --cols N --rows N
//!   roostctl tab reorder --project-id N --order id1,id2,id3
//!   roostctl tab clear-notification [--tab ID]
//!   roostctl project {list,create,rename,delete,reorder}
//!   roostctl claude-hook EVENT
//!   roostctl claude install [--force]
//!
//! Target selection (which UI socket to dial):
//!   --socket PATH           (highest precedence)
//!   ROOST_SOCKET env var
//!   --target {mac,gtk}      (resolves to that profile's canonical socket)
//!   ROOST_BUNDLE_PROFILE    (same effect as --target)
//!   auto-detect             (probes both; fails on ambiguity)
//!
//! See `crates/roost-ipc/src/target.rs` for resolution logic.

use std::io::Read;
use std::path::PathBuf;

use anyhow::{anyhow, Result};
use base64::prelude::*;
use clap::{Parser, Subcommand, ValueEnum};

use roost_ipc::messages::ops;
use roost_ipc::messages::{
    IdentifyParams, IdentifyResult, NotificationCreateParams, ProjectCreateParams,
    ProjectCreateResult, ProjectDeleteParams, ProjectRenameParams, ProjectReorderParams,
    TabClearNotificationParams, TabCloseParams, TabFocusParams, TabListResult, TabOpenParams,
    TabOpenResult, TabReorderParams, TabResizeParams, TabSetHookActiveParams, TabSetStateParams,
    TabSetTitleParams, TabState, TabWriteParams,
};
use roost_ipc::paths::BundleProfileKind;
use roost_ipc::target::{ResolvedTarget, TargetError, TargetSelector};
use roost_ipc::IpcClient;

const CLIENT_NAME: &str = "roostctl";

#[derive(Parser, Debug)]
#[command(name = "roostctl", version, about = "Roost shell-integration CLI")]
struct Args {
    /// Explicit socket path. Highest precedence; overrides
    /// `--target`, `ROOST_SOCKET`, and auto-detect.
    #[arg(long)]
    socket: Option<PathBuf>,

    /// Which Roost UI to talk to when auto-detect would otherwise
    /// be ambiguous. `--socket` and `ROOST_SOCKET` both win over
    /// this; passing `--target` short-circuits the auto-detect
    /// probe so the call is also faster when you know.
    #[arg(long, value_enum)]
    target: Option<TargetArg>,

    #[command(subcommand)]
    command: Cmd,
}

#[derive(ValueEnum, Debug, Clone, Copy)]
enum TargetArg {
    Mac,
    Gtk,
}

impl From<TargetArg> for BundleProfileKind {
    fn from(t: TargetArg) -> Self {
        match t {
            TargetArg::Mac => BundleProfileKind::Mac,
            TargetArg::Gtk => BundleProfileKind::Gtk,
        }
    }
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
    /// Print the running UI's identity (socket, pid, active tab,
    /// version).
    Identify,
    /// Tab subcommands.
    #[command(subcommand)]
    Tab(TabCmd),
    /// Project subcommands.
    #[command(subcommand)]
    Project(ProjectCmd),
    /// Claude Code hook entry point. Reads the JSON event payload
    /// from stdin (Claude's contract), dispatches state +
    /// notification ops to the running UI, and ALWAYS exits 0 with
    /// `{}` on stdout — Claude treats nonzero as a failed hook.
    ClaudeHook {
        /// Hook event name. Matches Claude Code's lifecycle:
        /// `session-start`, `prompt-submit`, `notification`,
        /// `stop`, `session-end`.
        event: String,
    },
    /// Claude Code subcommands (install hook settings file).
    #[command(subcommand)]
    Claude(ClaudeCmd),
}

#[derive(Subcommand, Debug)]
enum ClaudeCmd {
    /// Write `~/.config/roost/claude-settings.json` pointing at
    /// this binary's `claude-hook` subcommand for each Claude
    /// Code lifecycle event, then print an `alias claude=…`
    /// snippet the user pastes into their shell rc.
    Install {
        #[arg(long)]
        force: bool,
    },
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
    /// Persist a new sidebar ordering. `--order` is a
    /// comma-separated list of project ids in the target display
    /// order. Any project not listed keeps its prior position;
    /// duplicates / unknown ids fail with `invalid-param`.
    Reorder {
        #[arg(long, value_delimiter = ',')]
        order: Vec<i64>,
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
    /// Open a new tab in the given project. `--cwd` defaults to
    /// the project's cwd; `--cols / --rows` default to 80x24 (the
    /// UI re-quantizes to its cell grid on first attach). Prints
    /// the new tab id on stdout.
    Open {
        #[arg(long)]
        project_id: i64,
        #[arg(long, default_value = "")]
        cwd: String,
        #[arg(long, default_value_t = 80)]
        cols: u32,
        #[arg(long, default_value_t = 24)]
        rows: u32,
        #[arg(long, default_value = "roostctl")]
        title: String,
    },
    /// Close a tab. The UI closes the PTY (if live) and emits
    /// `tab.closed`.
    Close {
        #[arg(long, env = "ROOST_TAB_ID")]
        tab: Option<i64>,
    },
    /// Write bytes into a tab's PTY without attaching a
    /// streaming consumer. The tab must already have a live PTY
    /// (i.e. a UI must have spawned the shell) — errors with
    /// `not-found` otherwise. `--bytes` is treated as a
    /// Rust-style escaped string (`\n`, `\r`, `\t`, `\x1b`, etc.)
    /// unless `--raw` is set. For binary fidelity (arbitrary
    /// bytes, not UTF-8) use `--bytes-base64` instead.
    Send {
        #[arg(long, env = "ROOST_TAB_ID")]
        tab: Option<i64>,
        #[arg(
            long,
            conflicts_with = "bytes_base64",
            required_unless_present = "bytes_base64"
        )]
        bytes: Option<String>,
        /// Base64-encoded payload. Mutually exclusive with
        /// `--bytes`. Unblocks raw-byte transfers that the
        /// escape-decoding `--bytes` form can't represent
        /// safely.
        #[arg(long, conflicts_with = "bytes")]
        bytes_base64: Option<String>,
        #[arg(long, default_value_t = false)]
        raw: bool,
    },
    /// Resize a tab's PTY. Same constraint as `tab send` —
    /// needs an existing live PTY.
    Resize {
        #[arg(long, env = "ROOST_TAB_ID")]
        tab: Option<i64>,
        #[arg(long)]
        cols: u32,
        #[arg(long)]
        rows: u32,
    },
    /// Persist a new tab ordering within a project. `--order`
    /// is a comma-separated list of tab ids in the target
    /// display order. Tabs not listed keep their prior
    /// position; duplicates / cross-project ids fail
    /// `invalid-param`.
    Reorder {
        #[arg(long)]
        project_id: i64,
        #[arg(long, value_delimiter = ',')]
        order: Vec<i64>,
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

    // claude-hook is fire-and-forget — any failure path must exit
    // 0 with `{}` on stdout. Split it out before resolving the
    // target so an offline UI doesn't make the hook itself fail.
    if let Cmd::ClaudeHook { event } = &args.command {
        let event = event.clone();
        let _ = run_claude_hook(&event, &args).await;
        println!("{{}}");
        return Ok(());
    }

    // claude install doesn't dial the UI either — it just writes a
    // settings file pointing at this binary's claude-hook
    // subcommand.
    if let Cmd::Claude(ClaudeCmd::Install { force }) = args.command {
        return claude_install(force);
    }

    // Everything else needs a live UI socket.
    let target = resolve_target(&args, /*probe_alive=*/ true).await?;
    let mut client = IpcClient::connect(&target.socket_path).await?;

    match args.command {
        Cmd::Notify { title, body, tab } => {
            let tab_id = resolve_tab(&mut client, tab).await?;
            client
                .call::<_, serde_json::Value>(
                    ops::NOTIFICATION_CREATE,
                    NotificationCreateParams {
                        tab_id,
                        title,
                        body,
                    },
                )
                .await?;
        }
        Cmd::SetTitle { title, tab } => {
            let tab_id = resolve_tab(&mut client, tab).await?;
            client
                .call::<_, serde_json::Value>(
                    ops::TAB_SET_TITLE,
                    TabSetTitleParams { tab_id, title },
                )
                .await?;
        }
        Cmd::Identify => {
            let resp = identify(&mut client).await?;
            println!(
                "socket={}\npid={}\nactive_project={}\nactive_tab={}\nui_version={}\nproto_version={}",
                resp.socket_path,
                resp.pid,
                resp.active_project_id,
                resp.active_tab_id,
                resp.ui_version,
                resp.protocol_version
            );
        }
        Cmd::Tab(TabCmd::Focus { tab }) => {
            let tab_id = resolve_tab(&mut client, tab).await?;
            client
                .call::<_, serde_json::Value>(ops::TAB_FOCUS, TabFocusParams { tab_id })
                .await?;
        }
        Cmd::Tab(TabCmd::List) => {
            let resp = list_tabs(&mut client).await?;
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
                .call::<_, serde_json::Value>(
                    ops::TAB_SET_STATE,
                    TabSetStateParams { tab_id, state },
                )
                .await?;
        }
        Cmd::Tab(TabCmd::ClearNotification { tab }) => {
            let tab_id = resolve_tab(&mut client, tab).await?;
            client
                .call::<_, serde_json::Value>(
                    ops::TAB_CLEAR_NOTIFICATION,
                    TabClearNotificationParams { tab_id },
                )
                .await?;
        }
        Cmd::Project(ProjectCmd::List) => {
            let resp = list_tabs(&mut client).await?;
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
            let resp: ProjectCreateResult = client
                .call(ops::PROJECT_CREATE, ProjectCreateParams { name, cwd })
                .await?;
            println!(
                "created project {} — {}",
                resp.project.id, resp.project.name
            );
        }
        Cmd::Project(ProjectCmd::Rename { id, name }) => {
            client
                .call::<_, serde_json::Value>(
                    ops::PROJECT_RENAME,
                    ProjectRenameParams {
                        project_id: id,
                        name,
                    },
                )
                .await?;
        }
        Cmd::Project(ProjectCmd::Delete { id }) => {
            client
                .call::<_, serde_json::Value>(
                    ops::PROJECT_DELETE,
                    ProjectDeleteParams { project_id: id },
                )
                .await?;
        }
        Cmd::Project(ProjectCmd::Reorder { order }) => {
            client
                .call::<_, serde_json::Value>(
                    ops::PROJECT_REORDER,
                    ProjectReorderParams { project_ids: order },
                )
                .await?;
        }
        Cmd::Tab(TabCmd::Open {
            project_id,
            cwd,
            cols,
            rows,
            title,
        }) => {
            let resp: TabOpenResult = client
                .call(
                    ops::TAB_OPEN,
                    TabOpenParams {
                        project_id,
                        cwd,
                        argv: vec![],
                        cols,
                        rows,
                        title,
                    },
                )
                .await?;
            println!(
                "opened tab {} in project {} (cwd={})",
                resp.tab.id, resp.tab.project_id, resp.tab.cwd
            );
        }
        Cmd::Tab(TabCmd::Close { tab }) => {
            let tab_id = resolve_tab(&mut client, tab).await?;
            client
                .call::<_, serde_json::Value>(ops::TAB_CLOSE, TabCloseParams { tab_id })
                .await?;
        }
        Cmd::Tab(TabCmd::Send {
            tab,
            bytes,
            bytes_base64,
            raw,
        }) => {
            let tab_id = resolve_tab(&mut client, tab).await?;
            let data = if let Some(b64) = bytes_base64 {
                BASE64_STANDARD
                    .decode(b64.as_bytes())
                    .map_err(|e| anyhow!("--bytes-base64 decode failed: {e}"))?
            } else {
                let s =
                    bytes.ok_or_else(|| anyhow!("tab send requires --bytes or --bytes-base64"))?;
                if raw {
                    s.into_bytes()
                } else {
                    decode_escapes(&s)
                }
            };
            client
                .call::<_, serde_json::Value>(ops::TAB_WRITE, TabWriteParams { tab_id, data })
                .await?;
        }
        Cmd::Tab(TabCmd::Resize { tab, cols, rows }) => {
            let tab_id = resolve_tab(&mut client, tab).await?;
            client
                .call::<_, serde_json::Value>(
                    ops::TAB_RESIZE,
                    TabResizeParams { tab_id, cols, rows },
                )
                .await?;
        }
        Cmd::Tab(TabCmd::Reorder { project_id, order }) => {
            client
                .call::<_, serde_json::Value>(
                    ops::TAB_REORDER,
                    TabReorderParams {
                        project_id,
                        tab_ids: order,
                    },
                )
                .await?;
        }
        // Already handled above before client connect.
        Cmd::ClaudeHook { .. } | Cmd::Claude(_) => unreachable!(),
    }

    Ok(())
}

/// Build the [`TargetSelector`] from CLI args and resolve to a
/// concrete socket path. `probe_alive` controls whether the auto-
/// detect step actually dials candidate sockets; pass `false` for
/// fire-and-forget commands (claude-hook) that no-op when the UI
/// is offline.
async fn resolve_target(args: &Args, probe_alive: bool) -> Result<ResolvedTarget> {
    let selector = TargetSelector {
        socket_override: args.socket.clone(),
        kind_override: args.target.map(BundleProfileKind::from),
    };
    selector
        .resolve(probe_alive)
        .await
        .map_err(|e: TargetError| anyhow!(e))
}

async fn identify(client: &mut IpcClient) -> Result<IdentifyResult> {
    Ok(client
        .identify(IdentifyParams {
            client_name: CLIENT_NAME.into(),
            client_version: env!("CARGO_PKG_VERSION").into(),
        })
        .await?)
}

async fn list_tabs(client: &mut IpcClient) -> Result<TabListResult> {
    Ok(client.call(ops::TAB_LIST, serde_json::json!({})).await?)
}

/// Claude Code hook dispatch. Reads the JSON payload from stdin
/// (Claude's contract), maps the event name to a sequence of ops
/// against the running UI. Best-effort — failures don't surface to
/// Claude (caller wraps in `let _ = ...` and always exits 0).
async fn run_claude_hook(event: &str, args: &Args) -> Result<()> {
    let tab_id = std::env::var("ROOST_TAB_ID")
        .ok()
        .and_then(|s| s.parse::<i64>().ok())
        .unwrap_or(0);
    if tab_id == 0 {
        return Ok(());
    }

    // Drain stdin to a bounded buffer so Claude doesn't block on
    // a closed reader, even though only `notification` uses the
    // payload.
    let mut stdin_buf = Vec::with_capacity(4096);
    let _ = std::io::stdin().take(1 << 20).read_to_end(&mut stdin_buf);
    let parsed: serde_json::Value =
        serde_json::from_slice(&stdin_buf).unwrap_or(serde_json::Value::Null);

    // `probe_alive=false` so the resolver returns the default Mac
    // path even when no UI is listening — the dial below will fail
    // and we silently swallow. Matches the gRPC-era hook semantics
    // (always exits 0).
    let target = match resolve_target(args, false).await {
        Ok(t) => t,
        Err(_) => return Ok(()),
    };
    let mut client = match IpcClient::connect(&target.socket_path).await {
        Ok(c) => c,
        Err(_) => return Ok(()),
    };

    match event {
        "session-start" => {
            let _ = client
                .call::<_, serde_json::Value>(
                    ops::TAB_SET_HOOK_ACTIVE,
                    TabSetHookActiveParams {
                        tab_id,
                        active: true,
                    },
                )
                .await;
        }
        "prompt-submit" => {
            let _ = client
                .call::<_, serde_json::Value>(
                    ops::TAB_CLEAR_NOTIFICATION,
                    TabClearNotificationParams { tab_id },
                )
                .await;
            let _ = client
                .call::<_, serde_json::Value>(
                    ops::TAB_SET_STATE,
                    TabSetStateParams {
                        tab_id,
                        state: TabState::Running,
                    },
                )
                .await;
        }
        "notification" => {
            let _ = client
                .call::<_, serde_json::Value>(
                    ops::TAB_SET_STATE,
                    TabSetStateParams {
                        tab_id,
                        state: TabState::NeedsInput,
                    },
                )
                .await;
            let body = parsed
                .get("message")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .unwrap_or("Claude needs input")
                .to_string();
            let _ = client
                .call::<_, serde_json::Value>(
                    ops::NOTIFICATION_CREATE,
                    NotificationCreateParams {
                        tab_id,
                        title: "Claude Code".into(),
                        body,
                    },
                )
                .await;
        }
        "stop" => {
            let _ = client
                .call::<_, serde_json::Value>(
                    ops::TAB_SET_STATE,
                    TabSetStateParams {
                        tab_id,
                        state: TabState::Idle,
                    },
                )
                .await;
            let _ = client
                .call::<_, serde_json::Value>(
                    ops::NOTIFICATION_CREATE,
                    NotificationCreateParams {
                        tab_id,
                        title: "Claude Code".into(),
                        body: "Turn complete".into(),
                    },
                )
                .await;
        }
        "session-end" => {
            let _ = client
                .call::<_, serde_json::Value>(
                    ops::TAB_SET_HOOK_ACTIVE,
                    TabSetHookActiveParams {
                        tab_id,
                        active: false,
                    },
                )
                .await;
            let _ = client
                .call::<_, serde_json::Value>(
                    ops::TAB_SET_STATE,
                    TabSetStateParams {
                        tab_id,
                        state: TabState::None,
                    },
                )
                .await;
            let _ = client
                .call::<_, serde_json::Value>(
                    ops::TAB_CLEAR_NOTIFICATION,
                    TabClearNotificationParams { tab_id },
                )
                .await;
        }
        other => {
            if std::env::var("ROOST_DEBUG").is_ok() {
                eprintln!("roostctl claude-hook: unknown event: {other}");
            }
        }
    }
    Ok(())
}

/// Write `~/.config/roost/claude-settings.json` and print the
/// `alias claude=…` snippet. The hook command paths point at this
/// binary's canonical path so they survive PATH changes.
fn claude_install(force: bool) -> Result<()> {
    let home = std::env::var("HOME").map_err(|_| anyhow!("$HOME not set"))?;
    let dir = PathBuf::from(&home).join(".config").join("roost");
    std::fs::create_dir_all(&dir)?;
    let settings_path = dir.join("claude-settings.json");

    if !force && settings_path.exists() {
        eprintln!(
            "roostctl claude install: {} already exists; use --force to overwrite",
            settings_path.display()
        );
        std::process::exit(1);
    }

    // Resolve the absolute path of the current binary so the hook
    // commands survive PATH changes. `std::env::current_exe()`
    // returns the canonical path on macOS/Linux (modulo symlinks);
    // `canonicalize` resolves any symlink layer (e.g. when the
    // .app's `Contents/Resources/bin/roostctl` is the entry).
    let exe = std::env::current_exe()?;
    let exe = std::fs::canonicalize(&exe).unwrap_or(exe);
    let exe_str = exe.to_string_lossy().to_string();
    let exe_quoted = quote_for_shell(&exe_str);

    let hook_for = |event: &str| -> serde_json::Value {
        serde_json::json!([{
            "hooks": [{
                "type": "command",
                "command": format!("{} claude-hook {}", exe_quoted, event),
            }]
        }])
    };
    let doc = serde_json::json!({
        "hooks": {
            "SessionStart":     hook_for("session-start"),
            "UserPromptSubmit": hook_for("prompt-submit"),
            "Notification":     hook_for("notification"),
            "Stop":             hook_for("stop"),
            "SessionEnd":       hook_for("session-end"),
        }
    });
    let body = serde_json::to_string_pretty(&doc)? + "\n";
    std::fs::write(&settings_path, body)?;

    eprintln!("# Wrote {}", settings_path.display());
    eprintln!("# Add the line below to your shell rc (e.g. ~/.bashrc), then `source ~/.bashrc`.");
    eprintln!("# Fish/zsh: adapt the alias syntax for your shell.");
    println!();
    println!("# Roost: route Claude Code hooks to the running UI.");
    // Form is `alias claude='claude --settings '<quoted_path>`.
    // The trailing close-quote before the path looks weird but is
    // correct bash quote-concat: the single-quoted prefix
    // `'claude --settings '` is adjacent-concatenated with
    // `quote_for_shell`'s result (also single-quoted when needed),
    // producing one alias value. A double-quoted outer wrapper
    // (the M4c-polish "fix" that this comment reverts) re-exposes
    // `$`, backticks, and backslashes in the path to shell
    // expansion before the inner single quotes can protect them —
    // sub-agent review of M6-M9 caught a working
    // `alias claude="claude --settings '/has \`whoami\`/y'"`
    // example that expanded `whoami` to `charliek`. The
    // adjacent-quote form is safe; keep it.
    println!(
        "alias claude='claude --settings '{}",
        quote_for_shell(&settings_path.to_string_lossy())
    );
    Ok(())
}

fn quote_for_shell(s: &str) -> String {
    let needs_quote = s
        .chars()
        .any(|c| matches!(c, ' ' | '\t' | '"' | '$' | '\\' | '`' | '\''));
    if !needs_quote {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for c in s.chars() {
        if c == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(c);
        }
    }
    out.push('\'');
    out
}

/// Decode common Rust-style string escapes from `tab send --bytes`
/// so the user can write `--bytes "ls\n"` from a shell and get the
/// expected newline byte. Unknown escapes pass through verbatim —
/// the goal is convenience, not a full escape grammar. For binary
/// fidelity prefer `--bytes-base64`.
fn decode_escapes(s: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\\' {
            let mut buf = [0u8; 4];
            out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
            continue;
        }
        match chars.next() {
            Some('n') => out.push(b'\n'),
            Some('r') => out.push(b'\r'),
            Some('t') => out.push(b'\t'),
            Some('0') => out.push(0),
            Some('\\') => out.push(b'\\'),
            Some('"') => out.push(b'"'),
            Some('\'') => out.push(b'\''),
            Some('x') => {
                let h = chars.next();
                let l = chars.next();
                if let (Some(h), Some(l)) = (h, l) {
                    if let Ok(b) = u8::from_str_radix(&format!("{h}{l}"), 16) {
                        out.push(b);
                        continue;
                    }
                }
                out.push(b'\\');
                out.push(b'x');
                if let Some(h) = h {
                    let mut buf = [0u8; 4];
                    out.extend_from_slice(h.encode_utf8(&mut buf).as_bytes());
                }
                if let Some(l) = l {
                    let mut buf = [0u8; 4];
                    out.extend_from_slice(l.encode_utf8(&mut buf).as_bytes());
                }
            }
            Some(other) => {
                out.push(b'\\');
                let mut buf = [0u8; 4];
                out.extend_from_slice(other.encode_utf8(&mut buf).as_bytes());
            }
            None => out.push(b'\\'),
        }
    }
    out
}

/// Resolve the tab id for a per-tab command. Falls back to the
/// running UI's active tab via `identify` when neither `--tab` nor
/// `ROOST_TAB_ID` is set. Errors with a clear message when the UI
/// has no active tab either — better than sending `tab_id = 0` and
/// getting a confusing `not-found` back.
async fn resolve_tab(client: &mut IpcClient, explicit: Option<i64>) -> Result<i64> {
    if let Some(id) = explicit {
        return Ok(id);
    }
    let resp = identify(client).await?;
    if resp.active_tab_id == 0 {
        anyhow::bail!(
            "no --tab specified and the UI reports no active tab; \
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

fn format_state(state: TabState) -> &'static str {
    match state {
        TabState::None => "none",
        TabState::Running => "running",
        TabState::NeedsInput => "needs_input",
        TabState::Idle => "idle",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_escapes_handles_common_sequences() {
        assert_eq!(decode_escapes(r"ls\n"), b"ls\n");
        assert_eq!(decode_escapes(r"\r\t\0"), b"\r\t\0");
        assert_eq!(decode_escapes(r"\\path"), b"\\path");
        assert_eq!(decode_escapes(r"\x1b[31m"), b"\x1b[31m");
    }

    #[test]
    fn decode_escapes_passes_unknown_through_verbatim() {
        // `\q` is not a recognized escape — both the backslash and
        // the char survive.
        assert_eq!(decode_escapes(r"\q"), b"\\q");
        // Trailing backslash with no follower.
        assert_eq!(decode_escapes(r"trail\"), b"trail\\");
        // Malformed `\x` (only one hex digit) — emit the literal.
        assert_eq!(decode_escapes(r"\xZ"), b"\\xZ");
    }

    #[test]
    fn decode_escapes_preserves_utf8() {
        // Non-escaped multi-byte characters pass through byte-for-byte.
        assert_eq!(decode_escapes("café"), "café".as_bytes());
    }

    #[test]
    fn quote_for_shell_passes_safe_strings() {
        assert_eq!(quote_for_shell("simple"), "simple");
        assert_eq!(
            quote_for_shell("/usr/local/bin/roostctl"),
            "/usr/local/bin/roostctl"
        );
    }

    #[test]
    fn quote_for_shell_wraps_special_chars() {
        assert_eq!(quote_for_shell("has space"), "'has space'");
        assert_eq!(quote_for_shell("a$b"), "'a$b'");
        assert_eq!(quote_for_shell("it's"), "'it'\\''s'");
    }

    #[test]
    fn target_arg_maps_to_profile_kind() {
        assert!(matches!(
            BundleProfileKind::from(TargetArg::Mac),
            BundleProfileKind::Mac
        ));
        assert!(matches!(
            BundleProfileKind::from(TargetArg::Gtk),
            BundleProfileKind::Gtk
        ));
    }
}
