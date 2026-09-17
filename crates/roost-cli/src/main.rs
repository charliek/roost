//! roostctl — shell-integration CLI for the Roost UIs (Mac, Linux, and Iced).
//!
//! Talks JSON over a Unix-domain socket directly to the running UI
//! process; there is no daemon. The wire format is documented in
//! `docs/reference/ipc.md`. Subcommands mirror the gRPC-era surface
//! so existing scripts, Claude hooks, and shell aliases keep working:
//!
//!   roostctl notify --title TITLE [--body BODY] [--tab ID]
//!   roostctl set-title --title TITLE [--tab ID]
//!   roostctl identify
//!   roostctl wait [--tab ID] {--state S | --text T | --gone} [--timeout SECS | --no-timeout]
//!   roostctl events [--tab ID]
//!   roostctl tab focus [--tab ID]
//!   roostctl tab list
//!   roostctl tab set-state --state STATE [--tab ID]
//!   roostctl tab open --project-id N [--cwd …] [--after-tab ID] [--focus] [--hold] [-- <cmd…>]
//!   roostctl tab close [--tab ID]
//!   roostctl tab send [--tab ID] --bytes 'echo hi\n' [--raw]
//!   roostctl tab send [--tab ID] --bytes-base64 BASE64
//!   roostctl tab send-file --tab ID PATH…
//!   roostctl tab resize [--tab ID] --cols N --rows N
//!   roostctl tab reorder --project-id N --order id1,id2,id3
//!   roostctl tab clear-notification [--tab ID]
//!   roostctl project {list,create,ensure,rename,delete,reorder}
//!   roostctl open --project NAME [--cwd …] [--title T] [--focus] [--hold] [-- <cmd…>]
//!   roostctl palette {open,state,query,activate,dismiss}
//!   roostctl screenshot [--out PATH] [--scale 1|2]
//!   roostctl render-stats [--reset]
//!   roostctl agent-hook AGENT
//!   roostctl agent {ensure,set,install,uninstall,status}
//!   roostctl agent set <list|off> --local
//!   roostctl claude-hook EVENT
//!   roostctl claude install        (alias of `agent install claude`)
//!   roostctl session {start,stop,status}
//!   roostctl host {add,list,remove,connect,disconnect}
//!     add: --label, --target, [--verify]; the last three: --id
//!   roostctl rpc <op> [params]
//!   roostctl skill
//!
//! `--json` is global: every verb but the two hooks prints one JSON
//! document with it, and every failure goes through [`CliError`]'s one
//! renderer. `[--tab ID]` falls back to `ROOST_TAB_ID`; a verb in
//! [`MUTATING_TAB_VERBS`] refuses without either.
//!
//! Target selection (which UI socket to dial):
//!   --socket PATH           (highest precedence)
//!   ROOST_SOCKET env var
//!   --target {mac,linux,iced} (resolves to that profile's canonical socket)
//!   ROOST_BUNDLE_PROFILE    (same effect as --target)
//!   auto-detect             (probes all distinct paths; fails on ambiguity)
//!
//! See `crates/roost-ipc/src/target.rs` for resolution logic. The
//! headless session is **not** a target on that ladder: `session …`
//! addresses its socket directly (see [`session`]), and a generic op
//! reaches a session only through an explicit `--socket`.

mod agent_install;
mod doctor;
mod error;
mod events;
mod host;
mod session;
mod wait;

use std::ffi::OsString;
use std::io::{IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use base64::prelude::*;
use clap::{Parser, Subcommand, ValueEnum};
use serde::de::DeserializeOwned;
use serde::Serialize;

use roost_agent::claude::{canonical_hook_event, claude_event_to_reports};
use roost_agent::hook::{self, hook_payload, parse_tab_id, payload_event_name};
use roost_agent::Agent;
use roost_agent_install::Guard;
use roost_ipc::agent::TabAgentReportParams;
use roost_ipc::messages::ops;
use roost_ipc::messages::{
    AppRenderStatsParams, AppRenderStatsResult, IdentifyParams, IdentifyResult,
    NotificationCreateParams, PaletteActivateParams, PaletteItemView, PaletteOpenParams,
    PalettePresentParams, PalettePresentResult, PaletteQueryParams, PaletteStateResult,
    ProjectCreateParams, ProjectCreateResult, ProjectDeleteParams, ProjectEnsureParams,
    ProjectEnsureResult, ProjectRenameParams, ProjectReorderParams, ScreenshotParams,
    ScreenshotResult, TabClearNotificationParams, TabCloseParams, TabDumpParams, TabDumpResult,
    TabFocusParams, TabListResult, TabOpenParams, TabOpenResult, TabReorderParams, TabResizeParams,
    TabSendFileParams, TabSendFileResult, TabSetStateParams, TabSetTitleParams, TabState,
    TabWriteParams, WireProjectRef, WireTabRef,
};
use roost_ipc::paths::BundleProfileKind;
use roost_ipc::session_launch::timeout_scale;
use roost_ipc::target::TargetSelector;
use roost_ipc::IpcClient;

use crate::error::CliError;

const CLIENT_NAME: &str = "roostctl";

/// The verbs that change a tab, spelled the way a user types them.
///
/// Each takes its tab from `--tab` or `ROOST_TAB_ID` and otherwise exits
/// 2 `usage` before dialling ([`require_tab`]): the UI's active tab is
/// whatever a person last clicked, which is no answer for a command that
/// writes to it. The read-only `tab dump` and `wait` keep that fallback
/// ([`resolve_tab_or_active`]).
///
/// `tab send-file` is not listed because clap itself requires its
/// `--tab`. Neither are `agent-hook` and `claude-hook`: they take no
/// `--tab`, and the id their reports carry in the payload comes from
/// `ROOST_TAB_ID` alone — a hook without one sends nothing and still
/// answers `{}`.
const MUTATING_TAB_VERBS: &[&str] = &[
    "notify",
    "set-title",
    "tab set-state",
    "tab clear-notification",
    "tab close",
    "tab send",
    "tab resize",
    "tab focus",
];

/// The agent skill `roostctl skill` prints: the repo's `skills/roost/SKILL.md`,
/// which the Claude Code plugin and `npx skills add` also install, so the
/// binary and the published skill cannot disagree.
const SKILL: &str = include_str!("../../../skills/roost/SKILL.md");

const NO_TAB: &str = "no --tab and ROOST_TAB_ID is unset; refusing to guess the active tab for \
                      a command that changes it — `roostctl tab list` shows ids";

#[derive(Parser, Debug)]
#[command(
    name = "roostctl",
    version,
    about = "Roost shell-integration CLI",
    long_about = "Roost shell-integration CLI.\n\n\
                  Bare tab and project ids mean whichever backend the UI \
                  runs its own tabs on. With `local-backend = session` the \
                  UI hands those ops to the local `roost-session` and \
                  answers with the session's reply, so `--tab 7` names a \
                  session tab, and so does the active tab `tab dump` and \
                  `wait` fall back to; `--tab h<n>.<id>` still names a saved \
                  host's tab and is never re-addressed. `roostctl identify` \
                  prints the backend and the session socket.\n\n\
                  A command that changes a tab (notify, set-title, tab \
                  set-state, tab clear-notification, tab close, tab send, \
                  tab resize, tab focus) needs --tab or ROOST_TAB_ID, which \
                  every Roost tab sets; without either it exits 2.\n\n\
                  Exit codes: 0 ok, 1 failed, 2 usage, 3 `session status` \
                  found no session, 4 `wait` timed out. A failure prints \
                  `roostctl: <code>: <message>` on stderr, or \
                  {\"error\":{\"code\",\"message\"}} under --json.",
    after_help = "Driving Roost from an agent? `roostctl skill` prints the skill; \
                  docs: https://charliek.github.io/roost/guides/automation/"
)]
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

    /// Print the result as one JSON document on stdout, and a failure as
    /// `{"error":{"code","message"}}` on stderr. Accepted before or after
    /// the subcommand. `agent-hook` and `claude-hook` ignore it.
    #[arg(long, global = true)]
    json: bool,

    #[command(subcommand)]
    command: Cmd,
}

#[derive(ValueEnum, Debug, Clone, Copy)]
enum TargetArg {
    Mac,
    Linux,
    Iced,
}

impl From<TargetArg> for BundleProfileKind {
    fn from(t: TargetArg) -> Self {
        match t {
            TargetArg::Mac => BundleProfileKind::Mac,
            TargetArg::Linux => BundleProfileKind::Linux,
            TargetArg::Iced => BundleProfileKind::Iced,
        }
    }
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Fire a notification on a tab.
    ///
    /// Needs `--tab` or `ROOST_TAB_ID`: it never falls back to the UI's
    /// active tab.
    Notify {
        #[arg(long)]
        title: String,
        #[arg(long, default_value = "")]
        body: String,
        /// The tab. Defaults to `$ROOST_TAB_ID`; exits 2 without either.
        #[arg(long)]
        tab: Option<i64>,
    },
    /// Rename a tab (locks it from OSC overwrites).
    ///
    /// Needs `--tab` or `ROOST_TAB_ID`: it never falls back to the UI's
    /// active tab.
    SetTitle {
        #[arg(long)]
        title: String,
        /// The tab. Defaults to `$ROOST_TAB_ID`; exits 2 without either.
        #[arg(long)]
        tab: Option<i64>,
    },
    /// Print the running UI's identity (socket, pid, active tab,
    /// version).
    Identify,
    /// Block until a tab reaches a condition, then exit 0 — the
    /// no-`sleep` synchronization primitive for scripts and tests. At least
    /// one of `--state` / `--text` / `--gone` is required; when several are
    /// given, all must hold. Exits 4 (`timeout`) if `--timeout` elapses
    /// first.
    ///
    /// Event-driven where the tabs' socket streams its events: an
    /// in-process UI, or the local session `identify` names under
    /// `local-backend = session`. `--text` also re-reads the viewport every
    /// `--interval-ms`, since no event carries a tab's output. Against a
    /// server with no stream (the Mac app, an older Roost) it polls at
    /// `--interval-ms` instead. A lost stream is resolved again once, and
    /// the wait carries on only if the same process answers: tab ids do not
    /// carry across a local-backend switch or a restart, so either exits 1
    /// (`connection`), as do a second loss and a session that stops.
    ///
    /// `--json` prints `tab_id`, `satisfied` (`state`, `text` and `gone`,
    /// each `null` unless its flag was given) and `after_ms`.
    Wait(wait::Args),
    /// Print the event stream, one JSON line per event, until it ends.
    ///
    /// Each line is an event envelope, `{"event","data","revision"}` —
    /// the events of one commit share a `revision`, in order, and a commit
    /// with no events prints nothing. The stream's last line is its
    /// terminal envelope, `session.stopping` or `stream.ended` (no
    /// `revision`), and then it exits 0. A stream that drops or skips a
    /// revision exits 1 (`connection`); a server that does not serve the
    /// stream (the Mac app, an older Roost) exits 1 with its own refusal.
    /// Always JSON, with or without `--json`.
    ///
    /// Reads the same source `wait` does: the UI's in-process stream, or
    /// the local session under `local-backend = session`.
    Events {
        /// Print only the events that name this tab (a bare id). Takes no
        /// default from `$ROOST_TAB_ID`: without it every event prints.
        #[arg(long)]
        tab: Option<String>,
    },
    /// Tab subcommands.
    ///
    /// A bare `--tab` (and the active tab `tab dump` falls back to when
    /// the flag and `ROOST_TAB_ID` are both absent) names a tab on
    /// whichever backend the UI runs its own tabs on — see `roostctl
    /// --help`. Every verb here that changes a tab needs `--tab` or
    /// `ROOST_TAB_ID` and exits 2 without either.
    #[command(subcommand)]
    Tab(TabCmd),
    /// Project subcommands.
    ///
    /// Bare project ids follow the same rule as `roostctl tab`: under
    /// `local-backend = session` they are the local session's projects.
    #[command(subcommand)]
    Project(ProjectCmd),
    /// Find-or-create a project by exact name, then open a tab in it —
    /// `project.ensure` followed by `tab.open` (plan 066 §3.2). The
    /// one-shot verb an agent skill reaches for instead of composing
    /// `project list` + `project create` by hand, which races another
    /// caller doing the same thing (#221): the two calls this makes are
    /// **not atomic** with each other, but each is atomic on the server,
    /// so two concurrent `open`s with the same `--project` converge on
    /// one project either way.
    ///
    /// Always prints `{"project","tab","created"}` — the ensured
    /// project, the opened tab, and whether the project was just
    /// created — with or without `--json`: this is an agent verb, not a
    /// human-typed one. `--cwd` defaults to `$PWD`, for both the ensure
    /// and the open. Without `-- cmd…` the tab opens the default shell,
    /// same as `tab open`; `--hold` and `--focus` compose exactly as
    /// they do there — `--focus` runs a follow-up `tab.focus` and
    /// nothing else ever does, so a caller that didn't ask never steals
    /// the window.
    ///
    /// Refuses `unsupported` (exit 1) against a server whose
    /// `identify.ops` doesn't list `project.ensure` — the Mac app today
    /// — naming the manual route (`project list --json` +
    /// `tab open --project-id`) rather than falling back to it itself,
    /// which would reopen the same race. Refuses `usage` (exit 2) while
    /// `identify.local_backend_switch` shows a switch in progress: the
    /// project set is mid-flight, so a name resolved against it may not
    /// hold. A `not-found` from the `tab.open` half (the project vanished
    /// between the two calls — a switch landing in that window, say)
    /// reaches the caller as the server's own refusal, unchanged.
    Open {
        /// The project's exact name. Created at `--cwd` if no project
        /// has it; found otherwise.
        #[arg(long)]
        project: String,
        /// Working directory for both the ensured project (if created)
        /// and the new tab. Defaults to `$PWD`.
        #[arg(long)]
        cwd: Option<String>,
        #[arg(long, default_value = "roostctl")]
        title: String,
        /// Focus (activate) the new tab after opening.
        #[arg(long, default_value_t = false)]
        focus: bool,
        /// Keep the tab open after the command exits, dropping to an
        /// interactive shell (mirrors `tab open --hold`). Only
        /// meaningful with a command.
        #[arg(long, default_value_t = false)]
        hold: bool,
        /// Command + args to run in the tab, after `--`. Empty ⇒ the
        /// default shell.
        #[arg(last = true)]
        argv: Vec<String>,
    },
    /// Command-palette subcommands: drive the overlay (open, inspect,
    /// filter, activate a row, dismiss). Activating a row runs the same
    /// command its keybind would — so this is also a command-dispatch
    /// surface, not just a UI poke.
    #[command(subcommand)]
    Palette(PaletteCmd),
    /// Capture a PNG of the running UI's whole window (sidebar, tabs,
    /// active terminal), rendered in-process. Writes to `--out` if
    /// given, otherwise raw PNG bytes to stdout. `--json` needs `--out`
    /// and prints `{"out","bytes"}`.
    Screenshot {
        /// File to write the PNG to. Omit to stream raw bytes to stdout.
        #[arg(long)]
        out: Option<PathBuf>,
        /// Pixel multiplier: `1` (logical size) or `2` (super-sampled).
        /// Out-of-range values are rejected by clap with exit code 2.
        #[arg(long, default_value_t = 1, value_parser = clap::value_parser!(u32).range(1..=2))]
        scale: u32,
    },
    /// Read the running UI's render-path counters — refresh/draw call
    /// counts, elapsed nanos, rows and cells walked, `fill_text` calls.
    /// The only way to measure the real draw path: it needs a live
    /// renderer no unit test can construct.
    RenderStats {
        /// Zero the counters after reading them, so the next read is a
        /// clean delta over whatever ran in between.
        #[arg(long)]
        reset: bool,
    },
    /// Claude Code hook entry point. Reads the JSON event payload
    /// from stdin (Claude's contract), dispatches state +
    /// notification ops to the running UI, and ALWAYS exits 0 with
    /// `{}` on stdout — Claude treats nonzero as a failed hook.
    ClaudeHook {
        /// Hook event name. Accepts Claude Code's own `hook_event_name`
        /// (`SessionStart`, `UserPromptSubmit`, `PreToolUse`,
        /// `PermissionRequest`, `PermissionDenied`, `PostToolUse`,
        /// `PostToolUseFailure`, `Notification`, `Stop`, `StopFailure`,
        /// `SessionEnd`) as well as the legacy CLI
        /// spellings this binary wrote into `claude-settings.json`
        /// before this event set existed (`session-start`,
        /// `prompt-submit`, `notification`, `stop`, `session-end`) —
        /// every already-installed settings file uses those.
        /// `roost_agent::canonical_hook_event` resolves them all.
        event: String,
    },
    /// The one hook entrypoint every supported agent invokes.
    ///
    /// Reads the agent's JSON event payload from stdin, takes the event
    /// name from the payload's own `hook_event_name` (there is no
    /// `--event` flag: one installed command string serves every event),
    /// dispatches state + notification ops to the running UI, and ALWAYS
    /// exits 0 with `{}` on stdout — Claude's and codex's
    /// `PermissionRequest` are decision hooks whose dialog waits on this
    /// process, and a hook that answers with anything else may be read
    /// as a block.
    ///
    /// An agent Roost has no adapter for drains stdin and answers `{}`
    /// like every other path, so a stale config never breaks a turn.
    AgentHook {
        /// `claude`, `grok`, `codex`, `cursor`, or `opencode`.
        /// gx reports as `grok` and has no name of its own.
        agent: String,
    },
    /// Agent-hook install subcommands: wire Roost's hook entries into
    /// the supported agents' own config files, and take them out again.
    ///
    /// These never dial a UI — they read and write dotfiles — so they
    /// work with nothing running.
    #[command(subcommand)]
    Agent(agent_install::AgentCmd),
    /// Claude Code subcommands — `install` is a bare alias of `agent
    /// install claude` (plan 046 §3.5).
    #[command(subcommand)]
    Claude(ClaudeCmd),
    /// Headless host-session subcommands: start, stop, and inspect the
    /// `roost-session` daemon.
    ///
    /// A session is not a UI: it has its own socket, and it is
    /// deliberately unreachable through `--target` /
    /// `ROOST_BUNDLE_PROFILE` / auto-detect. These verbs address the
    /// session profile's socket directly; other ops reach a session only
    /// via an explicit `--socket`.
    #[command(subcommand)]
    Session(session::SessionCmd),
    /// Client-side saved-host subcommands: add, list, remove, connect,
    /// disconnect (host-sessions HS-2). Unlike `session`, these address the
    /// ordinary UI socket target — a saved host is UI state, not the
    /// session daemon's own workspace.
    #[command(subcommand)]
    Host(host::HostCmd),
    /// Call an op directly, by name: `roostctl rpc tab.list`,
    /// `roostctl rpc tab.write '{"tab_id":"4","data":"bHM="}'`. `params`
    /// is a JSON object literal, or `-` to read one from stdin; omitted
    /// ⇒ `{}`. Always prints the result as one JSON document, with or
    /// without `--json`; a failure goes through the same
    /// `roostctl: <code>: <message>` / `{"error":…}` envelope as every
    /// other verb, and an op this socket does not serve answers the
    /// server's own `unknown-op` verbatim.
    ///
    /// Bypasses the target policy [`MUTATING_TAB_VERBS`] enforces on the
    /// named verbs, and does not read `ROOST_TAB_ID`: the caller writes
    /// ids straight into `params`, so there is no `--tab` for this verb
    /// to resolve — and it is **not** a way around `--tab` for a verb
    /// that needs it; that verb stays the named one.
    Rpc {
        /// The op name, passed through verbatim (e.g. `tab.list`).
        op: String,
        /// The op's params as a JSON object, or `-` to read them from
        /// stdin. Omitted ⇒ `{}`.
        params: Option<String>,
    },
    /// Print the agent skill: the `SKILL.md` that teaches a coding agent
    /// to drive Roost with this CLI, byte for byte as the plugin installs
    /// it. Needs no running Roost. `--json` prints
    /// `{"topic":"roost","format":"markdown","content"}`.
    Skill,
    /// Diagnose the Roost integration: target resolution, socket, UI
    /// identity, shell-integration contract, the selected tab's four
    /// agent axes, and the Claude hook install. Read-only — it reports
    /// and links, it never repairs. Exits 1 (`checks-failed`) if any
    /// check fails; the report is on stdout either way.
    Doctor {
        /// Inspect this tab instead of `$ROOST_TAB_ID` / the UI's active
        /// tab.
        //
        // Doctor reads the env var itself rather than through
        // [`named_tab`] like every other per-tab command: that turns an
        // unparseable value into exit 2 where doctor owes a diagnostic,
        // and erases the difference between "the user passed --tab" and
        // "the env named a tab".
        #[arg(long)]
        tab: Option<i64>,
        /// Print the full per-check report instead of one line per
        /// section. Ignored by `--json`, which always carries everything.
        #[arg(short, long, default_value_t = false)]
        verbose: bool,
        /// When to color the text output. `auto` colors only a TTY, and
        /// honours `NO_COLOR` and `TERM=dumb`. Ignored by `--json`.
        #[arg(long, value_enum, default_value = "auto")]
        color: doctor::ColorMode,
    },
}

#[derive(Subcommand, Debug)]
enum ClaudeCmd {
    /// Alias of `roostctl agent install claude` (plan 046 §3.5). Kept as
    /// its own verb so the command existing scripts already run keeps
    /// working; it no longer writes
    /// `~/.config/roost/claude-settings.json` or prints a shell alias.
    Install,
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
    /// Find the project with this exact name, or create it at `--cwd` —
    /// atomic on the server, so two callers racing the same name
    /// converge on one project (#221) rather than each creating their
    /// own. Never activates it. `--cwd` defaults to `$PWD`: the op needs
    /// a directory on the create path, and this verb always sends one
    /// rather than leaving it to the server's "caller already knows this
    /// project exists" omission.
    ///
    /// Always prints `{"project","created"}` — the wire result — with or
    /// without `--json`: `project create`/`rename`/`delete`/`reorder`
    /// print nothing without the flag, so there is no single human form
    /// this verb could match instead.
    ///
    /// Refuses `unsupported` (exit 1) against a server whose
    /// `identify.ops` doesn't list `project.ensure` — the Mac app today
    /// — naming the manual route (`project list --json` + `project
    /// create`) rather than racing it with a list-then-create fallback.
    Ensure {
        /// The project's exact name.
        #[arg(long)]
        name: String,
        /// Working directory to create the project at, if it doesn't
        /// already exist. Defaults to `$PWD`.
        #[arg(long)]
        cwd: Option<String>,
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
    /// Focus a tab. `--tab` takes a bare id, or the `h<host>.<id>`
    /// spelling to select (and attach) a connected host's tab. Needs
    /// `--tab` or `ROOST_TAB_ID`.
    Focus {
        /// The tab. Defaults to `$ROOST_TAB_ID`; exits 2 without either.
        #[arg(long)]
        tab: Option<String>,
    },
    /// Send local files to a tab: upload them to the tab's host and
    /// paste the host paths, or — for a local tab — paste the escaped
    /// local paths. The same route a file drop onto the tab takes.
    ///
    /// `--tab` is **required**: the no-flag fallback resolves the UI's
    /// local active tab, which is never the host tab a caller means.
    ///
    /// **Blocks** until the paste has been queued in the tab — up to
    /// the upload budget for large files — and prints nothing until
    /// then. Deliberate: there is no progress channel, and the pasted
    /// text is the only thing worth printing. Prints the pasted text;
    /// `--json` prints the whole result (uploads + skips).
    SendFile {
        #[arg(long)]
        tab: String,
        #[arg(required = true)]
        paths: Vec<PathBuf>,
    },
    /// List projects + their tabs. `--json` emits the machine-readable
    /// workspace snapshot (the `tab.list` result) instead of plain text.
    ///
    /// Carries `revision` exactly where the socket also serves the event
    /// stream it fences: a UI running its tabs in-process does, and a UI
    /// under `local-backend = session` does not, though its answer came
    /// from the local session.
    List,
    /// Set the tab's agent-lifecycle axis by claiming ownership as
    /// `manual` (plan 002 §3.7). This **supersedes a live agent's
    /// ownership** — if Claude (or another agent) currently owns the
    /// tab, its subsequent hook events are dropped until its next
    /// session start, because taking the wheel is what a manual
    /// override means.
    ///
    /// `--state none` additionally **releases** ownership rather than
    /// claiming an inactive one, so the tab falls through to
    /// shell-derived state. This is a behavior change from before plan
    /// 002: a tab with a live foreground process now shows `running`
    /// under `none`, not `none` unconditionally.
    SetState {
        /// `none` releases ownership (falls through to shell state —
        /// see above); `running`/`needs_input`/`idle` claim ownership
        /// with that lifecycle.
        #[arg(long, value_parser = ["none", "running", "needs_input", "idle"])]
        state: String,
        /// The tab. Defaults to `$ROOST_TAB_ID`; exits 2 without either.
        #[arg(long)]
        tab: Option<i64>,
    },
    /// Clear a tab's pending notification. Needs `--tab` or
    /// `ROOST_TAB_ID`.
    ClearNotification {
        /// The tab. Defaults to `$ROOST_TAB_ID`; exits 2 without either.
        #[arg(long)]
        tab: Option<i64>,
    },
    /// Open a new tab in the given project. `--cwd` defaults to
    /// the project's cwd; `--cols / --rows` default to 80x24 (the
    /// UI re-quantizes to its cell grid on first attach). Prints
    /// the new tab id on stdout.
    ///
    /// A command to run in the tab can be given after `--`
    /// (e.g. `roostctl tab open --project-id 1 -- htop`). Without a
    /// command the tab opens the default shell. By default the tab
    /// closes when the command exits (hold=false); `--hold` keeps it
    /// open by dropping to an interactive shell afterward.
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
        /// Place the new tab immediately after this tab (same project).
        /// Omitted ⇒ appended at the end.
        #[arg(long)]
        after_tab: Option<i64>,
        /// Focus (activate) the new tab after opening it.
        #[arg(long, default_value_t = false)]
        focus: bool,
        /// Keep the tab open after the command exits, dropping to an
        /// interactive shell (mirrors `command = … hold=true`). Only
        /// meaningful with a command after `--`.
        #[arg(long, default_value_t = false)]
        hold: bool,
        /// Command + args to run in the tab, after `--`. Empty ⇒ the
        /// default shell.
        #[arg(last = true)]
        argv: Vec<String>,
    },
    /// Close a tab. The UI closes the PTY (if live) and emits
    /// `tab.closed`. Needs `--tab` or `ROOST_TAB_ID`.
    Close {
        /// The tab. Defaults to `$ROOST_TAB_ID`; exits 2 without either.
        #[arg(long)]
        tab: Option<i64>,
    },
    /// Write bytes into a tab's PTY without attaching a
    /// streaming consumer. The tab must already have a live PTY
    /// (i.e. a UI must have spawned the shell) — errors with
    /// `not-found` otherwise. `--bytes` is treated as a
    /// Rust-style escaped string (`\n`, `\r`, `\t`, `\x1b`, etc.)
    /// unless `--raw` is set. For binary fidelity (arbitrary
    /// bytes, not UTF-8) use `--bytes-base64` instead. Needs `--tab` or
    /// `ROOST_TAB_ID`.
    Send {
        /// The tab. Defaults to `$ROOST_TAB_ID`; exits 2 without either.
        #[arg(long)]
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
    /// Resize a tab's PTY. Same constraints as `tab send` —
    /// needs an existing live PTY, and `--tab` or `ROOST_TAB_ID`.
    Resize {
        /// The tab. Defaults to `$ROOST_TAB_ID`; exits 2 without either.
        #[arg(long)]
        tab: Option<i64>,
        #[arg(long)]
        cols: u32,
        #[arg(long)]
        rows: u32,
    },
    /// Dump the tab's terminal viewport as text — one line per visible
    /// row, for content assertions in automated tests. Prints the rows
    /// to stdout; `--json` emits the full result (dims + cursor + rows).
    /// `--tab` takes a bare id or, against the UI socket, the
    /// `h<host>.<id>` spelling of an attached host tab's client-side
    /// terminal (host-sessions §3.4).
    Dump {
        /// The tab. Defaults to `$ROOST_TAB_ID`, then the UI's active tab.
        #[arg(long)]
        tab: Option<String>,
        /// History rows above the viewport to include. Printed before
        /// the viewport rows with no separator, so `| grep` keeps
        /// working over the larger window. Server-side clamped, so
        /// this is passed through unvalidated; `0` (the default) omits
        /// the field entirely, keeping the request byte-identical to
        /// before this flag existed.
        #[arg(long, default_value_t = 0)]
        scrollback: u32,
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

/// `roostctl palette …` — drive the command-palette overlay. Each
/// subcommand prints the resulting palette state (a `>` marks the
/// highlighted row); `--json` emits the structured result.
#[derive(Subcommand, Debug)]
enum PaletteCmd {
    /// Open a palette root frame and print its rows.
    Open {
        /// Which frame to open: `commands` (default), `launcher`,
        /// `custom`, or `agents`.
        #[arg(long, default_value = "commands")]
        kind: String,
    },
    /// Print the current palette state (open?, frame, query, rows).
    State,
    /// Set the current frame's filter (as if typed), print the result.
    Query {
        /// The filter text.
        query: String,
    },
    /// Activate the row with this item id — the same dispatch as its
    /// keybind. Errors `not-found` if no palette is open or no row
    /// matches.
    Activate {
        /// The item id (a KeybindAction id like `new_tab`, or a sub-frame
        /// row id like a theme name).
        id: String,
    },
    /// Dismiss any open palette.
    Dismiss,
    /// Present a caller-supplied list and block until the user picks a
    /// row or dismisses, then print the chosen id (nothing on dismiss).
    /// Items come from `--items <json>` or stdin: a JSON array
    /// `[{"id","title","subtitle?"}]` or an object `{"items":[…]}`.
    Present {
        /// Title/placeholder shown in the search field.
        #[arg(long, default_value = "")]
        title: String,
        /// Overrides `--title` for the field placeholder when set.
        #[arg(long, default_value = "")]
        placeholder: String,
        /// The items JSON. When omitted, read from stdin (dmenu-style).
        #[arg(long)]
        items: Option<String>,
    },
}

#[tokio::main]
async fn main() {
    // Diagnostics to stderr, because stdout is this binary's *data*
    // channel: `--json` on any verb, `tab dump`, and the hook payloads
    // are all decoded by something. The config parser warns about a key
    // it cannot read, and since plan 064 the retired `agent-hooks =
    // auto` spelling is one of those — so on stdout that warning would
    // land in front of the JSON the Mac app decodes from `agent ensure`.
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "warn".into()),
        )
        .with_writer(std::io::stderr)
        .init();

    let argv: Vec<OsString> = std::env::args_os().collect();
    let args = match Args::try_parse_from(&argv) {
        Ok(args) => args,
        Err(error) => std::process::exit(refuse_command_line(&error, &argv)),
    };
    let json = args.json;
    let tab_env = std::env::var("ROOST_TAB_ID").ok();
    // `$PWD` as `open` / `project ensure` default `--cwd` to. Falls back to
    // the process's own working directory when the shell variable is
    // unset (a non-interactive launcher, say) — resolved once here, never
    // inside `run`, so a test can inject an arbitrary string without a
    // real directory or a process-wide `chdir` to back it.
    let cwd_env = std::env::var("PWD").ok().or_else(|| {
        std::env::current_dir()
            .ok()
            .map(|p| p.display().to_string())
    });
    let code = match run(args, tab_env.as_deref(), cwd_env.as_deref()).await {
        Ok(code) => code,
        Err(error) => report(&error, json),
    };
    std::process::exit(code);
}

/// Write a failure to stderr and hand back its exit code. Fallible for
/// the reason [`hook_answer`] is: `eprintln!` panics on a closed stderr,
/// which would trade this exit code for 101.
fn report(error: &CliError, json: bool) -> i32 {
    let _ = std::io::stderr()
        .lock()
        .write_all(error.render(json).as_bytes());
    error.exit_code()
}

/// A command line clap would not accept, rendered like every other
/// failure. `--help` and `--version` are not failures and still print to
/// stdout; a missing subcommand prints clap's help unchanged, because
/// that help is the usage text.
fn refuse_command_line(error: &clap::Error, argv: &[OsString]) -> i32 {
    use clap::error::ErrorKind;
    if matches!(
        error.kind(),
        ErrorKind::DisplayHelp | ErrorKind::DisplayVersion
    ) {
        error.exit();
    }
    let usage = CliError::from_clap(error);
    let json = asks_for_json(argv);
    if !json && error.kind() == ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand {
        let _ = error.print();
        return usage.exit_code();
    }
    report(&usage, json)
}

/// Whether argv carries `--json` — read off the raw words because a
/// command line clap refused has no parsed flag to ask. Stops at `--`:
/// every word past it belongs to the command `tab open` runs.
fn asks_for_json(argv: &[OsString]) -> bool {
    argv.iter()
        .skip(1)
        .map(|arg| arg.to_str())
        .take_while(|arg| *arg != Some("--"))
        .any(|arg| arg == Some("--json"))
}

/// Every verb, after parsing: its exit code, or its failure for `main`
/// to render.
///
/// `tab_env` is `ROOST_TAB_ID`, `cwd_env` is `$PWD` (falling back to the
/// process cwd), both as `main` read them — parameters so the target
/// policy and `open`/`project ensure`'s `--cwd` default are tested
/// without writing to the process environment every other test in this
/// binary reads.
async fn run(args: Args, tab_env: Option<&str>, cwd_env: Option<&str>) -> Result<i32, CliError> {
    let json = args.json;
    let selector = selector(&args);
    match args.command {
        // Both hook verbs are fire-and-forget — any failure path must
        // exit 0 with `{}` on stdout, written fallibly: Rust ignores
        // SIGPIPE, so a `println!` into a pipe whose reader has gone
        // would panic with 101 and no JSON at all, which a decision hook
        // may read as a block. They never reach [`UiSocket`], so an
        // offline UI doesn't make the hook itself fail, and they ignore
        // `--json`: `{}` is already the only answer they give.
        //
        // `claude-hook EVENT` is the alias `agent-hook claude` grew out
        // of: it takes its event from argv instead of the payload, which
        // is what every already-installed `claude-settings.json` writes.
        Cmd::ClaudeHook { event } => {
            run_claude_hook(&event, &selector, tab_env).await;
            hook_answer();
            Ok(0)
        }
        Cmd::AgentHook { agent } => {
            run_agent_hook(&agent, &selector, tab_env).await;
            hook_answer();
            Ok(0)
        }
        // The `agent` verbs write dotfiles and never dial a UI — wiring
        // an agent has to work with nothing running, which is exactly
        // when a user reaches for it. The one shape `dials_the_ui` names
        // is served over the UI socket instead.
        Cmd::Agent(cmd) if !agent_install::dials_the_ui(&cmd) => agent_install::run(&cmd, json),
        // `claude install` doesn't dial the UI either — it is a bare
        // alias of `agent install claude`, which only reads and writes
        // dotfiles.
        Cmd::Claude(ClaudeCmd::Install) => claude_install(json),
        // An agent reads the skill to learn how to find a Roost, so it
        // must print with none running.
        Cmd::Skill => write_skill(&mut std::io::stdout().lock(), json),
        // `session` addresses the session profile's own socket, which no
        // target selector resolves (and must not — see
        // `roost_ipc::target`'s HS-0 fences). `start` also has to work
        // with nothing listening at all.
        Cmd::Session(cmd) => session::run(&cmd, json).await,
        // doctor exists to report "no UI is running", so it must not
        // dial through [`UiSocket`], which fails on exactly that
        // condition before the verb's own code runs.
        Cmd::Doctor {
            tab,
            verbose,
            color,
        } => run_doctor(&selector, tab, json, verbose, color).await,
        command => {
            run_on_ui(
                command,
                &mut UiSocket::new(&selector),
                tab_env,
                cwd_env,
                json,
            )
            .await
        }
    }
}

async fn run_doctor(
    selector: &TargetSelector,
    tab: Option<i64>,
    json: bool,
    verbose: bool,
    color: doctor::ColorMode,
) -> Result<i32, CliError> {
    // The three impure probes live here, in the thin I/O layer;
    // `color_enabled` itself stays pure so its precedence is
    // table-testable.
    let no_color = std::env::var("NO_COLOR").ok();
    let term = std::env::var("TERM").ok();
    let style = doctor::Style {
        color: doctor::color_enabled(
            color,
            std::io::stdout().is_terminal(),
            no_color.as_deref(),
            term.as_deref(),
        ),
    };
    let report = doctor::evaluate(&doctor::collect(selector, tab).await);
    let rendered = doctor::render(&report, json, style, verbose).map_err(CliError::failed)?;
    let mut stdout = std::io::stdout().lock();
    stdout
        .write_all(rendered.as_bytes())
        .and_then(|()| stdout.flush())
        .map_err(|e| CliError::Failed(format!("write the report: {e}")))?;
    report.verdict()
}

/// The UI socket a verb talks to, dialled the first time the verb asks
/// for it — so a verb that refuses its own arguments has dialled nothing.
struct UiSocket<'a> {
    selector: &'a TargetSelector,
    client: Option<IpcClient>,
    path: PathBuf,
}

impl<'a> UiSocket<'a> {
    fn new(selector: &'a TargetSelector) -> Self {
        Self {
            selector,
            client: None,
            path: PathBuf::new(),
        }
    }

    async fn client(&mut self) -> Result<&mut IpcClient, CliError> {
        let client = match self.client.take() {
            Some(client) => client,
            None => {
                let target = self.selector.resolve(true).await?;
                let client = dial(&target.socket_path).await?;
                self.path = target.socket_path;
                client
            }
        };
        Ok(self.client.insert(client))
    }

    /// The socket [`Self::client`] dialled; empty before it has.
    fn socket_path(&self) -> &Path {
        &self.path
    }

    /// Hang up, so the next [`Self::client`] resolves and dials afresh.
    fn redial(&mut self) {
        self.client = None;
    }

    async fn call<P: Serialize, R: DeserializeOwned>(
        &mut self,
        op: &str,
        params: P,
    ) -> Result<R, CliError> {
        Ok(self.client().await?.call(op, params).await?)
    }

    /// A verb that prints nothing on success: under `--json`, the op's reply.
    async fn ack<P: Serialize>(&mut self, op: &str, params: P, json: bool) -> Result<(), CliError> {
        let reply: serde_json::Value = self.call(op, params).await?;
        if json {
            print_json(&reply)?;
        }
        Ok(())
    }
}

/// The verbs served over the UI socket.
async fn run_on_ui(
    command: Cmd,
    ui: &mut UiSocket<'_>,
    tab_env: Option<&str>,
    cwd_env: Option<&str>,
    json: bool,
) -> Result<i32, CliError> {
    match command {
        Cmd::Notify { title, body, tab } => {
            let tab_id = require_tab("notify", tab, tab_env)?;
            ui.ack(
                ops::NOTIFICATION_CREATE,
                NotificationCreateParams {
                    tab_id,
                    title,
                    body,
                },
                json,
            )
            .await?;
        }
        Cmd::SetTitle { title, tab } => {
            let tab_id = require_tab("set-title", tab, tab_env)?;
            ui.ack(
                ops::TAB_SET_TITLE,
                TabSetTitleParams { tab_id, title },
                json,
            )
            .await?;
        }
        Cmd::Identify => {
            let resp = identify(ui.client().await?).await?;
            if json {
                print_json(&resp)?;
            } else {
                println!(
                    "socket={}\npid={}\nactive_project={}\nactive_tab={}\nui_version={}\nproto_version={}\napp_id={}",
                    resp.socket_path,
                    resp.pid,
                    resp.active_project_id,
                    resp.active_tab_id,
                    resp.ui_version,
                    resp.protocol_version,
                    resp.app_id
                );
            }
        }
        Cmd::Wait(args) => return wait::run(ui, args, tab_env, json).await,
        Cmd::Events { tab } => return events::run(ui, tab).await,
        Cmd::Tab(TabCmd::Focus { tab }) => {
            let tab_id = require_tab(
                "tab focus",
                tab.as_deref().map(parse_tab_flag).transpose()?,
                tab_env,
            )?;
            ui.ack(ops::TAB_FOCUS, TabFocusParams { tab_id }, json)
                .await?;
        }
        Cmd::Tab(TabCmd::List) => {
            let resp = list_tabs(ui.client().await?).await?;
            if json {
                print_json(&resp)?;
            } else {
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
        }
        Cmd::Tab(TabCmd::SetState { state, tab }) => {
            let tab_id = require_tab("tab set-state", tab, tab_env)?;
            let state = parse_state(&state)?;
            ui.ack(
                ops::TAB_SET_STATE,
                TabSetStateParams { tab_id, state },
                json,
            )
            .await?;
        }
        Cmd::Tab(TabCmd::ClearNotification { tab }) => {
            let tab_id = require_tab("tab clear-notification", tab, tab_env)?;
            ui.ack(
                ops::TAB_CLEAR_NOTIFICATION,
                // No `generation`: a person at a CLI is answering the
                // tab, not one raise on it.
                TabClearNotificationParams {
                    tab_id,
                    generation: None,
                },
                json,
            )
            .await?;
        }
        Cmd::Project(ProjectCmd::List) => {
            let resp = list_tabs(ui.client().await?).await?;
            if json {
                print_json(&resp)?;
            } else {
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
        }
        Cmd::Project(ProjectCmd::Create { name, cwd }) => {
            let resp: ProjectCreateResult = ui
                .call(ops::PROJECT_CREATE, ProjectCreateParams { name, cwd })
                .await?;
            if json {
                print_json(&resp)?;
            } else {
                println!(
                    "created project {} — {}",
                    resp.project.id, resp.project.name
                );
            }
        }
        Cmd::Project(ProjectCmd::Ensure { name, cwd }) => {
            require_project_ensure(ui).await?;
            let cwd = resolve_cwd(cwd, cwd_env)?;
            let resp: ProjectEnsureResult = ui
                .call(
                    ops::PROJECT_ENSURE,
                    ProjectEnsureParams {
                        name,
                        cwd: Some(cwd),
                    },
                )
                .await?;
            // Always the wire result, `--json` or not: `create` is the
            // only other `project` verb with a one-line human form, and
            // `rename`/`delete`/`reorder` print nothing without the
            // flag, so there is no single convention this could join
            // instead of just always answering in JSON.
            print_json(&resp)?;
        }
        Cmd::Project(ProjectCmd::Rename { id, name }) => {
            ui.ack(
                ops::PROJECT_RENAME,
                ProjectRenameParams {
                    project_id: id,
                    name,
                },
                json,
            )
            .await?;
        }
        Cmd::Project(ProjectCmd::Delete { id }) => {
            ui.ack(
                ops::PROJECT_DELETE,
                ProjectDeleteParams { project_id: id },
                json,
            )
            .await?;
        }
        Cmd::Project(ProjectCmd::Reorder { order }) => {
            ui.ack(
                ops::PROJECT_REORDER,
                ProjectReorderParams {
                    // The CLI's ids are numbers; the host-qualified
                    // wire form is reachable through the op, not
                    // through this verb (plan 044 §4).
                    project_ids: order.into_iter().map(WireProjectRef::Local).collect(),
                },
                json,
            )
            .await?;
        }
        Cmd::Tab(TabCmd::Open {
            project_id,
            cwd,
            cols,
            rows,
            title,
            after_tab,
            focus,
            hold,
            argv,
        }) => {
            let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());
            // `--hold` wraps the command so a fresh interactive shell takes
            // over when it exits (tab persists). Without it, the argv runs
            // directly and the tab closes on exit. Empty argv ⇒ default shell.
            let argv = if hold && !argv.is_empty() {
                held_argv(&shell, &argv)
            } else {
                argv
            };
            let client = ui.client().await?;
            let resp: TabOpenResult = client
                .call(
                    ops::TAB_OPEN,
                    TabOpenParams {
                        project_id,
                        cwd,
                        argv,
                        cols,
                        rows,
                        title,
                    },
                )
                .await?;
            let new_id = resp.tab.id;
            // `--after-tab`: place the new tab right after that one via a
            // reorder over the project's current order.
            if let Some(after) = after_tab {
                let snapshot = list_tabs(client).await?;
                if let Some(project) = snapshot.projects.iter().find(|p| p.id == project_id) {
                    let ids: Vec<i64> = project.tabs.iter().map(|t| t.id).collect();
                    client
                        .call::<_, serde_json::Value>(
                            ops::TAB_REORDER,
                            TabReorderParams {
                                project_id: WireProjectRef::Local(project_id),
                                tab_ids: order_with_after(&ids, new_id, after)
                                    .into_iter()
                                    .map(WireTabRef::Local)
                                    .collect(),
                            },
                        )
                        .await?;
                }
            }
            if focus {
                client
                    .call::<_, serde_json::Value>(
                        ops::TAB_FOCUS,
                        TabFocusParams {
                            tab_id: WireTabRef::Local(new_id),
                        },
                    )
                    .await?;
            }
            if json {
                print_json(&resp)?;
            } else {
                // Print just the new tab id (matches the documented
                // contract; script-friendly for `id=$(roostctl tab open …)`).
                println!("{new_id}");
            }
        }
        Cmd::Open {
            project,
            cwd,
            title,
            focus,
            hold,
            argv,
        } => {
            let identity = require_project_ensure(ui).await?;
            if identity.local_backend_switch.is_some() {
                return Err(CliError::Usage(
                    "a backend switch is in progress; retry".into(),
                ));
            }
            let cwd = resolve_cwd(cwd, cwd_env)?;
            let ensured: ProjectEnsureResult = ui
                .call(
                    ops::PROJECT_ENSURE,
                    ProjectEnsureParams {
                        name: project,
                        cwd: Some(cwd.clone()),
                    },
                )
                .await?;
            let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());
            // Same `--hold` composition `tab open --hold` uses — see its
            // handler above; not duplicated as a shared helper because
            // this is the entire use of the local `argv` binding here.
            let argv = if hold && !argv.is_empty() {
                held_argv(&shell, &argv)
            } else {
                argv
            };
            let client = ui.client().await?;
            let opened: TabOpenResult = client
                .call(
                    ops::TAB_OPEN,
                    TabOpenParams {
                        project_id: ensured.project.id,
                        cwd,
                        argv,
                        cols: 80,
                        rows: 24,
                        title,
                    },
                )
                .await?;
            if focus {
                client
                    .call::<_, serde_json::Value>(
                        ops::TAB_FOCUS,
                        TabFocusParams {
                            tab_id: WireTabRef::Local(opened.tab.id),
                        },
                    )
                    .await?;
            }
            // Always JSON, `--json` or not — an agent verb, not a
            // human-typed one.
            print_json(&serde_json::json!({
                "project": ensured.project,
                "tab": opened.tab,
                "created": ensured.created,
            }))?;
        }
        Cmd::Tab(TabCmd::Close { tab }) => {
            let tab_id = require_tab("tab close", tab, tab_env)?;
            ui.ack(ops::TAB_CLOSE, TabCloseParams { tab_id }, json)
                .await?;
        }
        Cmd::Tab(TabCmd::Send {
            tab,
            bytes,
            bytes_base64,
            raw,
        }) => {
            let tab_id = require_tab("tab send", tab, tab_env)?;
            let data = if let Some(b64) = bytes_base64 {
                BASE64_STANDARD
                    .decode(b64.as_bytes())
                    .map_err(|e| CliError::Usage(format!("--bytes-base64 decode failed: {e}")))?
            } else {
                let s = bytes.ok_or_else(|| {
                    CliError::Usage("tab send requires --bytes or --bytes-base64".into())
                })?;
                if raw {
                    s.into_bytes()
                } else {
                    decode_escapes(&s)
                }
            };
            ui.ack(ops::TAB_WRITE, TabWriteParams { tab_id, data }, json)
                .await?;
        }
        Cmd::Tab(TabCmd::SendFile { tab, paths }) => {
            let tab_id = parse_tab_flag(&tab)?;
            let paths = canonicalize_for_send_file(&paths)?;
            let budget = send_file_budget(paths.len(), timeout_scale());
            let call = ui.client().await?.call(
                ops::TAB_SEND_FILE,
                TabSendFileParams {
                    tab: tab_id.to_string(),
                    paths,
                },
            );
            let result: TabSendFileResult =
                tokio::time::timeout(budget, call).await.map_err(|_| {
                    CliError::Connection(format!(
                        "tab send-file gave up after {}s; the paste may still land in the tab",
                        budget.as_secs()
                    ))
                })??;
            if json {
                print_json(&result)?;
            } else {
                println!("{}", result.pasted);
            }
        }
        Cmd::Tab(TabCmd::Resize { tab, cols, rows }) => {
            let tab_id = require_tab("tab resize", tab, tab_env)?;
            ui.ack(
                ops::TAB_RESIZE,
                TabResizeParams { tab_id, cols, rows },
                json,
            )
            .await?;
        }
        Cmd::Tab(TabCmd::Dump { tab, scrollback }) => {
            let flag = tab.as_deref().map(parse_tab_flag).transpose()?;
            let tab_id = resolve_tab_or_active(ui, flag, tab_env).await?;
            let result: TabDumpResult = ui
                .call(ops::TAB_DUMP, TabDumpParams { tab_id, scrollback })
                .await?;
            if json {
                print_json(&result)?;
            } else {
                // Plain text: history rows then viewport rows, no
                // separator between them, reconstructing the screen for
                // `roostctl tab dump | grep …` assertions. `scrollback_text`
                // is empty when `--scrollback` is omitted, so this is
                // byte-identical to the pre-flag output.
                for line in &result.scrollback_text {
                    println!("{line}");
                }
                for line in &result.rows_text {
                    println!("{line}");
                }
            }
        }
        Cmd::Tab(TabCmd::Reorder { project_id, order }) => {
            ui.ack(
                ops::TAB_REORDER,
                TabReorderParams {
                    project_id: WireProjectRef::Local(project_id),
                    tab_ids: order.into_iter().map(WireTabRef::Local).collect(),
                },
                json,
            )
            .await?;
        }
        Cmd::Screenshot { out, scale } => {
            if json && out.is_none() {
                return Err(CliError::Usage(
                    "--json needs --out: the PNG and the JSON would share stdout".into(),
                ));
            }
            // `scale` range is enforced by clap's value_parser (exit 2).
            let resp: ScreenshotResult =
                ui.call(ops::SCREENSHOT, ScreenshotParams { scale }).await?;
            match out {
                Some(path) => {
                    std::fs::write(&path, &resp.png)
                        .map_err(|e| CliError::Failed(format!("write {}: {e}", path.display())))?;
                    if json {
                        print_json(&serde_json::json!({
                            "out": path.display().to_string(),
                            "bytes": resp.png.len(),
                        }))?;
                    } else {
                        eprintln!(
                            "wrote {} ({}x{} @ {}x, {} bytes)",
                            path.display(),
                            resp.width,
                            resp.height,
                            resp.scale,
                            resp.png.len()
                        );
                    }
                }
                None => {
                    // Raw PNG to stdout — never `println!`, which would
                    // append a newline and corrupt the binary stream.
                    let mut stdout = std::io::stdout().lock();
                    stdout
                        .write_all(&resp.png)
                        .and_then(|()| stdout.flush())
                        .map_err(|e| CliError::Failed(format!("write the PNG to stdout: {e}")))?;
                }
            }
        }
        Cmd::RenderStats { reset } => {
            let stats: AppRenderStatsResult = ui
                .call(ops::APP_RENDER_STATS, AppRenderStatsParams { reset })
                .await?;
            if json {
                print_json(&stats)?;
                return Ok(0);
            }
            let per = |total: i64, calls: i64| {
                if calls > 0 {
                    (total / calls).to_string()
                } else {
                    "-".to_string()
                }
            };
            println!("refresh_calls    {}", stats.refresh_calls);
            println!("refresh_nanos    {}", stats.refresh_nanos);
            println!("rows_rebuilt     {}", stats.rows_rebuilt);
            println!("cells_walked     {}", stats.cells_walked);
            println!("draw_calls       {}", stats.draw_calls);
            println!("draw_nanos       {}", stats.draw_nanos);
            println!("fill_text_calls  {}", stats.fill_text_calls);
            println!("view_calls       {}", stats.view_calls);
            println!("view_nanos       {}", stats.view_nanos);
            println!("elide_calls      {}", stats.elide_calls);
            println!("elide_nanos      {}", stats.elide_nanos);
            println!(
                "ns_per_refresh   {}",
                per(stats.refresh_nanos, stats.refresh_calls)
            );
            println!(
                "ns_per_draw      {}",
                per(stats.draw_nanos, stats.draw_calls)
            );
            println!(
                "us_per_view      {}",
                per(stats.view_nanos / 1_000, stats.view_calls)
            );
            println!(
                "ns_per_elide     {}",
                per(stats.elide_nanos, stats.elide_calls)
            );
        }
        Cmd::Palette(PaletteCmd::Open { kind }) => {
            let state: PaletteStateResult = ui
                .call(ops::PALETTE_OPEN, PaletteOpenParams { kind })
                .await?;
            print_palette(&state, json)?;
        }
        Cmd::Palette(PaletteCmd::State) => {
            let state: PaletteStateResult =
                ui.call(ops::PALETTE_STATE, serde_json::json!({})).await?;
            print_palette(&state, json)?;
        }
        Cmd::Palette(PaletteCmd::Query { query }) => {
            let state: PaletteStateResult = ui
                .call(ops::PALETTE_QUERY, PaletteQueryParams { query })
                .await?;
            print_palette(&state, json)?;
        }
        Cmd::Palette(PaletteCmd::Activate { id }) => {
            let state: PaletteStateResult = ui
                .call(ops::PALETTE_ACTIVATE, PaletteActivateParams { id })
                .await?;
            print_palette(&state, json)?;
        }
        Cmd::Palette(PaletteCmd::Dismiss) => {
            let state: PaletteStateResult =
                ui.call(ops::PALETTE_DISMISS, serde_json::json!({})).await?;
            print_palette(&state, json)?;
        }
        Cmd::Palette(PaletteCmd::Present {
            title,
            placeholder,
            items,
        }) => {
            let raw = match items {
                Some(s) => s,
                None => {
                    let mut buf = String::new();
                    std::io::stdin()
                        .read_to_string(&mut buf)
                        .map_err(|e| CliError::Failed(format!("read items from stdin: {e}")))?;
                    buf
                }
            };
            let parsed = parse_present_items(&raw)?;
            let result: PalettePresentResult = ui
                .call(
                    ops::PALETTE_PRESENT,
                    PalettePresentParams {
                        title,
                        placeholder,
                        items: parsed,
                    },
                )
                .await?;
            if json {
                print_json(&result)?;
            } else if let Some(id) = &result.selected_id {
                println!("{id}");
            }
            // Dismissed → print nothing; exit 0 either way.
        }
        Cmd::Rpc { op, params } => {
            let value = rpc_params(params)?;
            let result: serde_json::Value = ui.call(&op, value).await?;
            print_json(&result)?;
        }
        Cmd::Host(cmd) => return host::run(&cmd, ui, json).await,
        Cmd::Agent(cmd) => return agent_install::run_over_ipc(&cmd, ui, json).await,
        Cmd::ClaudeHook { .. }
        | Cmd::AgentHook { .. }
        | Cmd::Claude(_)
        | Cmd::Skill
        | Cmd::Doctor { .. }
        | Cmd::Session(_) => unreachable!("`run` serves these without the UI socket"),
    }
    Ok(0)
}

/// `skill`'s output. A reader that hung up (`roostctl skill | head`) got
/// what it wanted, so that is success, as it is for `events`.
fn write_skill(out: &mut impl Write, json: bool) -> Result<i32, CliError> {
    let body = if json {
        let value = serde_json::json!({
            "topic": "roost",
            "format": "markdown",
            "content": SKILL,
        });
        serde_json::to_string_pretty(&value)
            .map_err(|e| CliError::Failed(format!("encode the skill as JSON: {e}")))?
            + "\n"
    } else {
        SKILL.to_string()
    };
    match out.write_all(body.as_bytes()).and_then(|()| out.flush()) {
        Ok(()) => Ok(0),
        Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => Ok(0),
        Err(e) => Err(CliError::Failed(format!("write the skill: {e}"))),
    }
}

/// `--json`'s output: one pretty-printed document on stdout.
fn print_json<T: Serialize + ?Sized>(value: &T) -> Result<(), CliError> {
    let body = serde_json::to_string_pretty(value)
        .map_err(|e| CliError::Failed(format!("encode the result as JSON: {e}")))?;
    println!("{body}");
    Ok(())
}

/// Parse the `palette present` items payload. Accepts a bare JSON array
/// of rows or an object with an `items` array (the same shape a Roost
/// provider prints), so a script can pipe either form. Rejects an
/// empty/blank payload so the user gets a clear error instead of an
/// `invalid-param` from the daemon.
fn parse_present_items(raw: &str) -> Result<Vec<PaletteItemView>, CliError> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Err(CliError::Usage(
            "no items: pass --items <json> or pipe a JSON array on stdin".into(),
        ));
    }
    let value: serde_json::Value =
        serde_json::from_str(raw).map_err(|e| CliError::Usage(format!("parse items json: {e}")))?;
    let items_value = if value.is_array() {
        value
    } else {
        value.get("items").cloned().ok_or_else(|| {
            CliError::Usage("items json must be an array or have an `items` array".into())
        })?
    };
    let items: Vec<PaletteItemView> = serde_json::from_value(items_value)
        .map_err(|e| CliError::Usage(format!("decode items: {e}")))?;
    if items.is_empty() {
        return Err(CliError::Usage("items list is empty".into()));
    }
    Ok(items)
}

/// `rpc`'s params: the positional argument, `-` for stdin, or `{}` when
/// omitted. Parsed and object-checked before anything is dialled — the
/// same posture [`require_tab`] takes toward a mutating verb's `--tab`.
fn rpc_params(raw: Option<String>) -> Result<serde_json::Value, CliError> {
    let raw = match raw {
        None => return Ok(serde_json::json!({})),
        Some(s) if s == "-" => {
            let mut buf = String::new();
            std::io::stdin()
                .read_to_string(&mut buf)
                .map_err(|e| CliError::Failed(format!("read params from stdin: {e}")))?;
            buf
        }
        Some(s) => s,
    };
    let value: serde_json::Value = serde_json::from_str(&raw)
        .map_err(|e| CliError::Usage(format!("parse params json: {e}")))?;
    if !value.is_object() {
        return Err(CliError::Usage("params must be a JSON object".into()));
    }
    Ok(value)
}

/// Render a [`PaletteStateResult`] for the terminal: a header line, then
/// one row per item with `>` marking the highlighted selection. `--json`
/// emits the structured result verbatim instead.
fn print_palette(state: &PaletteStateResult, json: bool) -> Result<(), CliError> {
    if json {
        return print_json(state);
    }
    if !state.open {
        println!("palette: closed");
        return Ok(());
    }
    println!(
        "palette: {} (query {:?}, {} rows)",
        state.frame.as_deref().unwrap_or("?"),
        state.query,
        state.items.len()
    );
    for (i, item) in state.items.iter().enumerate() {
        let marker = if i as u32 == state.selection {
            '>'
        } else {
            ' '
        };
        match &item.subtitle {
            Some(sub) => println!("{marker} {:<24} {}  — {}", item.id, item.title, sub),
            None => println!("{marker} {:<24} {}", item.id, item.title),
        }
    }
    Ok(())
}

/// The global flags that pick which UI to talk to. `doctor` needs the
/// selector itself (it reports *how* resolution went), everything else
/// only needs the resolved path — one construction either way.
fn selector(args: &Args) -> TargetSelector {
    TargetSelector {
        socket_override: args.socket.clone(),
        kind_override: args.target.map(BundleProfileKind::from),
    }
}

async fn dial(socket: &Path) -> Result<IpcClient, CliError> {
    IpcClient::connect(socket)
        .await
        .map_err(|e| CliError::Connection(format!("{}: {e}", socket.display())))
}

async fn identify(client: &mut IpcClient) -> Result<IdentifyResult, CliError> {
    Ok(client
        .identify(IdentifyParams {
            client_name: CLIENT_NAME.into(),
            client_version: env!("CARGO_PKG_VERSION").into(),
        })
        .await?)
}

async fn list_tabs(client: &mut IpcClient) -> Result<TabListResult, CliError> {
    Ok(client.call(ops::TAB_LIST, serde_json::json!({})).await?)
}

/// `identify`, refusing `unsupported` (exit 1) if this server's `ops`
/// doesn't list `project.ensure` — an absent field (an older server, or
/// the Swift Mac app) and a list without the name both mean "not
/// served" (plan 066 §3.1). Named for the manual route rather than
/// falling back to it: a list-then-create fallback here would reopen the
/// #221 race `project.ensure` exists to close, so `open` and `project
/// ensure` refuse instead of ever sending `project.list`/`project.create`
/// themselves.
async fn require_project_ensure(ui: &mut UiSocket<'_>) -> Result<IdentifyResult, CliError> {
    let identity = identify(ui.client().await?).await?;
    let supported = identity
        .ops
        .as_deref()
        .is_some_and(|served| served.iter().any(|op| op == ops::PROJECT_ENSURE));
    if !supported {
        return Err(CliError::Unsupported(format!(
            "this Roost does not serve {}; find the project with `roostctl project list \
             --json` and open the tab directly with `roostctl tab open --project-id`",
            ops::PROJECT_ENSURE
        )));
    }
    Ok(identity)
}

/// `--cwd`, or `cwd_env` (`$PWD` as `main` resolved it) when the flag is
/// absent. `open` and `project ensure` both need an actual directory —
/// `project.ensure` only reads `cwd` on the create path, but neither verb
/// knows in advance whether this call creates, so both always send one
/// rather than leaving it to the op's own omit-if-you-already-know-it
/// allowance.
fn resolve_cwd(explicit: Option<String>, cwd_env: Option<&str>) -> Result<String, CliError> {
    if let Some(cwd) = explicit {
        return Ok(cwd);
    }
    cwd_env
        .map(str::to_string)
        .ok_or_else(|| CliError::Failed("no --cwd, and $PWD could not be resolved".into()))
}

/// Claude Code hook dispatch. Reads the JSON payload from stdin
/// (Claude's contract), maps the event to the reports
/// `roost-agent`'s pure adapter derives, and sends each as a
/// `tab.agent_report`. Best-effort — failures don't surface to Claude,
/// and the caller always exits 0.
async fn run_claude_hook(event: &str, selector: &TargetSelector, tab_env: Option<&str>) {
    // Drained first and unconditionally, exactly as in
    // [`run_agent_hook`]: Claude is writing into this pipe right now,
    // and returning on an unset `ROOST_TAB_ID` without consuming a byte
    // would hand it an EPIPE from a hook that is supposed to be
    // invisible.
    let stdin_buf = drain_stdin();

    let Some(tab_id) = tab_env.and_then(parse_tab_id) else {
        return;
    };
    let Some(payload) = hook_payload(&stdin_buf, tab_id) else {
        hook_debug(&format!(
            "claude-hook: unparseable payload for event: {event}"
        ));
        return;
    };

    let reports = canonical_hook_event(event)
        .map(|name| claude_event_to_reports(name, &payload, tab_id))
        .unwrap_or_default();
    if reports.is_empty() {
        hook_debug(&format!("claude-hook: no reports for event: {event}"));
        return;
    }

    // `claude install` writes a `PermissionRequest` entry on this verb,
    // so it carries a decision hook too and is held to the same budget
    // as `agent-hook` — see [`hook::CONNECT_TIMEOUT`].
    //
    // The target resolver, though, is the general one: unlike
    // `agent-hook` this verb is documented as a by-hand debugging tool
    // (`docs/development/claude-testing.md`) that is driven outside a
    // Roost tab, where the default profile path is the only answer
    // there is.
    let Ok(target) = selector.resolve(false).await else {
        return;
    };
    deliver_reports(reports, &target.socket_path).await;
}

/// Read stdin to the shared cap **and keep reading past it**.
///
/// `take(CAP).read_to_end(..)` alone declares EOF at exactly the cap and
/// leaves the rest in the pipe, so a payload one byte over the line
/// hands the writing agent an EPIPE the moment this process exits — the
/// one outcome a hook must never produce. Everything past the cap is
/// discarded (the truncated head no longer parses anyway); what matters
/// is that the writer's `write` returns.
fn drain_stdin() -> Vec<u8> {
    let mut stdin = std::io::stdin().lock();
    let mut buf = Vec::with_capacity(4096);
    let _ = (&mut stdin).take(hook::STDIN_CAP).read_to_end(&mut buf);
    let _ = std::io::copy(&mut stdin, &mut std::io::sink());
    buf
}

/// `{}` on stdout, whatever happened, and never a panic on the way.
///
/// A locked fallible writer rather than `println!`: Rust ignores
/// SIGPIPE, so `println!` turns a reader that has already gone into a
/// panic — exit 101 with no JSON, which is precisely the shape a
/// decision hook may read as a block.
fn hook_answer() {
    let mut stdout = std::io::stdout().lock();
    let _ = stdout.write_all(b"{}\n").and_then(|()| stdout.flush());
}

/// The generic agent hook entrypoint: `roostctl agent-hook <agent>`.
///
/// The event name comes from the payload (`hook_event_name`, or its
/// camelCase twin) rather than from argv, so one installed command
/// string serves every event an agent has. Everything else matches
/// [`run_claude_hook`]: a drained stdin, the `ROOST_TAB_ID` gate, a
/// bounded best-effort dial, and `{}` on stdout whatever happens — the
/// one exception being how the socket is found ([`agent_hook_socket`]).
///
/// Failures are deliberately swallowed rather than returned. The whole
/// contract of this path is exit 0 with `{}` on stdout — a hook that
/// reports its own trouble to a decision dialog may be read as a block —
/// so the only diagnostic channel is `ROOST_DEBUG` on stderr.
async fn run_agent_hook(agent: &str, selector: &TargetSelector, tab_env: Option<&str>) {
    // Drained first and unconditionally: the agent is writing into this
    // pipe right now, and every early return below would otherwise leave
    // it with an EPIPE from a hook that is supposed to be invisible.
    let stdin_buf = drain_stdin();

    let Some(adapter) = Agent::parse(agent) else {
        hook_debug(&format!("agent-hook: no adapter for agent: {agent}"));
        return;
    };
    let Some(tab_id) = tab_env.and_then(parse_tab_id) else {
        return;
    };
    let Some(socket) = agent_hook_socket(selector) else {
        hook_debug(&format!("agent-hook {agent}: no ROOST_SOCKET"));
        return;
    };
    let Some(payload) = hook_payload(&stdin_buf, tab_id) else {
        hook_debug(&format!("agent-hook {agent}: unparseable payload"));
        return;
    };

    let event = payload_event_name(&payload);
    if event.is_empty() {
        hook_debug(&format!("agent-hook {agent}: payload names no event"));
        return;
    }
    let reports = adapter.event_to_reports(event, &payload, tab_id);
    if reports.is_empty() {
        hook_debug(&format!(
            "agent-hook {agent}: no reports for event: {event}"
        ));
        return;
    }

    deliver_reports(reports, &socket).await;
}

/// The socket `agent-hook` reports into — `ROOST_SOCKET` and nothing
/// else, with `--socket` as the one explicit override.
///
/// Deliberately **not** [`TargetSelector::resolve`]. That ladder falls back to
/// the bundle profile's default path, and this verb runs inside a tab
/// whose `ROOST_TAB_ID` is only meaningful to the Roost that spawned it:
/// with the variable stripped (`env -i`, a sanitized launcher) but the
/// tab id kept, a `SessionStart` would claim tab 7 of some *other*
/// running Roost and evict whatever really owns it. No socket therefore
/// means no report — the drain and `{}` still happen.
///
/// `claude-hook` keeps the general resolver on purpose: it is documented
/// as a by-hand debugging verb driven from outside a tab
/// (`docs/development/claude-testing.md`), where the default path is the
/// only answer there is.
fn agent_hook_socket(selector: &TargetSelector) -> Option<PathBuf> {
    selector.socket_override.clone().or_else(|| {
        std::env::var_os("ROOST_SOCKET")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
    })
}

/// Dial the UI and send every report — the tail both hook verbs end in,
/// under one budget.
///
/// Separate from its callers so the total budget covers exactly the
/// socket work: the stdin drain above it is a blocking read no timeout
/// could cancel anyway, and draining it is the contract.
///
/// Both verbs are bounded, and by the same numbers: `claude install`
/// writes a `PermissionRequest` entry for `claude-hook` too, so a socket
/// that accepts and never answers would hold a decision dialog open on
/// either path.
async fn deliver_reports(reports: Vec<TabAgentReportParams>, socket: &Path) {
    let scale = timeout_scale();
    let _ = tokio::time::timeout(hook::TOTAL_BUDGET.mul_f64(scale), async move {
        let dialed = tokio::time::timeout(
            hook::CONNECT_TIMEOUT.mul_f64(scale),
            IpcClient::connect(socket),
        )
        .await;
        let Ok(Ok(mut client)) = dialed else {
            return;
        };
        for report in reports {
            let _ = client
                .call::<_, serde_json::Value>(ops::TAB_AGENT_REPORT, report)
                .await;
        }
    })
    .await;
}

/// `ROOST_DEBUG`'s one channel — fallible for the same reason
/// [`hook_answer`] is: `eprintln!` panics when stderr has been closed,
/// and it would do so *before* the `{}` this process owes stdout.
fn hook_debug(message: &str) {
    if std::env::var("ROOST_DEBUG").is_ok() {
        let mut stderr = std::io::stderr().lock();
        let _ = writeln!(stderr, "roostctl {message}");
    }
}

/// `~/.claude/settings.json` (or `$CLAUDE_CONFIG_DIR/settings.json`) —
/// Claude's own global settings file. `agent install claude` / `agent
/// ensure` merge Roost's hook entries into it; `doctor`'s `claude.*`
/// checks read it back. One definition so the two cannot drift about
/// where it lives, and the same resolution
/// `roost_agent_install::home::Home` uses, so `CLAUDE_CONFIG_DIR` means
/// one thing everywhere.
///
/// Before plan 046, `claude install` wrote a Roost-owned file at
/// `~/.config/roost/claude-settings.json` instead — `doctor`'s
/// `agent.claude.legacy_settings` check is what still knows about that
/// retired path.
fn claude_settings_path() -> anyhow::Result<PathBuf> {
    let home = std::env::var("HOME").map_err(|_| anyhow::anyhow!("$HOME not set"))?;
    let dir = std::env::var("CLAUDE_CONFIG_DIR")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .map(|v| {
            let p = PathBuf::from(v);
            if p.is_absolute() {
                p
            } else {
                PathBuf::from(&home).join(p)
            }
        })
        .unwrap_or_else(|| PathBuf::from(&home).join(".claude"));
    Ok(dir.join("settings.json"))
}

/// The legacy path `claude install` used to write, before plan 046.
/// Kept as its own function (rather than inlining the join) so
/// `agent.claude.legacy_settings` and `legacy_claude_uninstall` cannot
/// spell it two ways.
fn legacy_claude_settings_path() -> anyhow::Result<PathBuf> {
    let home = std::env::var("HOME").map_err(|_| anyhow::anyhow!("$HOME not set"))?;
    Ok(PathBuf::from(home)
        .join(".config")
        .join("roost")
        .join("claude-settings.json"))
}

/// `claude install` is now a bare alias of `agent install claude` (plan
/// 046 §3.5). It no longer writes `~/.config/roost/claude-settings.json`
/// or prints a shell alias snippet — wiring happens automatically
/// (`agent-hooks = auto`) via `$ROOST_AGENT_HOOK`, so there is nothing
/// left for a shell alias to route. Exit code follows `agent install`:
/// 0 whether this run wired Claude or found it already wired, matching
/// every other agent verb's "explicit wins" idempotency.
fn claude_install(json: bool) -> Result<i32, CliError> {
    eprintln!(
        "roostctl claude install: alias of `roostctl agent install claude` — it no longer \
         writes ~/.config/roost/claude-settings.json or prints a shell alias. Run `roostctl \
         agent status` to see what is wired, or `roostctl agent uninstall claude` to remove it."
    );
    agent_install::run(
        &agent_install::AgentCmd::Install {
            agent: Some("claude".to_string()),
            all: false,
        },
        json,
    )
}

/// Test-only now: production code stopped writing quoted commands when
/// `claude install` became a bare alias (plan 046 §3.5). Kept for
/// `legacy_claude_settings_matches_generated_shape`'s own tests, which
/// still need to build a fixture shaped like what a pre-046 install
/// wrote — the same shell quoting `roost-cli`'s `doctor::shell_split`
/// has to read back.
#[cfg(test)]
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

/// Every **complete** event set a shipped `claude install` ever wrote,
/// in canonical spellings. The list is closed: plan 046 retires the
/// writer, so no future release can add a fourth entry, and hard-coding
/// them (rather than pointing at `CLAUDE_HOOK_EVENTS`, which is free to
/// keep growing) is what keeps a real legacy file matching after the
/// adapter learns a new event.
///
/// A file whose event set is not one of these is a file somebody edited
/// — most obviously one trimmed down to the events they cared about —
/// and it is not Roost's to delete.
const LEGACY_GENERATED_EVENT_SETS: [&[&str]; 3] = [
    // `bde60c4` — the first writer. CamelCase keys, kebab-case command
    // tokens; `canonical_hook_event` resolves both.
    &[
        "SessionStart",
        "UserPromptSubmit",
        "Notification",
        "Stop",
        "SessionEnd",
    ],
    // `489b220` added `StopFailure`. This is the set every release up to
    // and including v0.0.19 wrote, so it is the one almost every file in
    // the wild carries.
    &[
        "SessionStart",
        "UserPromptSubmit",
        "Notification",
        "Stop",
        "StopFailure",
        "SessionEnd",
    ],
    // `1f5e9b7` added the five tool events. Never released — only a
    // development build between it and this commit wrote this set.
    &[
        "SessionStart",
        "UserPromptSubmit",
        "PreToolUse",
        "PermissionRequest",
        "PermissionDenied",
        "PostToolUse",
        "PostToolUseFailure",
        "Notification",
        "Stop",
        "StopFailure",
        "SessionEnd",
    ],
];

/// Whether a parsed legacy `claude-settings.json` still holds exactly
/// the shape a previous `claude install` wrote: one top-level key
/// (`hooks`), one of [`LEGACY_GENERATED_EVENT_SETS`] as its complete
/// event set, and under each event exactly one group whose only key is
/// `hooks`, holding exactly one entry whose only keys are `type` and
/// `command` — with that command's trailing token normalizing to its own
/// event through [`canonical_hook_event`].
///
/// Path-agnostic on purpose — the exe path baked into the command varies
/// per machine, which is one of the two reasons plan 046 retires this
/// file — so a match here only claims the *layout* is untouched, not
/// that any particular path is current.
///
/// Everything else about it is deliberately exact, because
/// `agent uninstall claude` **deletes** the file when this returns
/// `true`. A subset of the events, a `timeout` added to an entry, a
/// `matcher` added to a group, a second handler, a foreign top-level
/// key: each of those is somebody's edit, and the promise is that an
/// edited file is left alone and reported rather than removed.
fn legacy_claude_settings_matches_generated_shape(doc: &serde_json::Value) -> bool {
    let Some(obj) = doc.as_object() else {
        return false;
    };
    let [(key, hooks_value)] = obj.iter().collect::<Vec<_>>()[..] else {
        return false;
    };
    if key != "hooks" {
        return false;
    }
    let Some(hooks) = hooks_value.as_object() else {
        return false;
    };

    let mut events: Vec<&'static str> = Vec::with_capacity(hooks.len());
    for (event, groups) in hooks {
        let Some(canonical) = canonical_hook_event(event) else {
            return false;
        };
        // Two spellings of one event is not a shape any writer produced,
        // and it would let a set of the right size pass with an event
        // missing from it.
        if events.contains(&canonical) || !legacy_event_matches(groups, canonical) {
            return false;
        }
        events.push(canonical);
    }
    events.sort_unstable();
    LEGACY_GENERATED_EVENT_SETS.iter().any(|set| {
        let mut set = set.to_vec();
        set.sort_unstable();
        set == events
    })
}

/// One event's value, against what the writer put there: `[{ "hooks":
/// [{ "type": "command", "command": "<exe> claude-hook <event>" }] }]`
/// and nothing besides.
fn legacy_event_matches(groups: &serde_json::Value, event: &'static str) -> bool {
    let Some([group]) = groups.as_array().map(Vec::as_slice) else {
        return false;
    };
    let Some(group) = group.as_object() else {
        return false;
    };
    let [(group_key, entries)] = group.iter().collect::<Vec<_>>()[..] else {
        return false;
    };
    if group_key != "hooks" {
        return false;
    }
    let Some([entry]) = entries.as_array().map(Vec::as_slice) else {
        return false;
    };
    let Some(entry) = entry.as_object() else {
        return false;
    };
    if entry.len() != 2 || entry.get("type").and_then(|t| t.as_str()) != Some("command") {
        return false;
    }
    let Some(command) = entry.get("command").and_then(|c| c.as_str()) else {
        return false;
    };
    let Some(argv) = doctor::shell_split(command) else {
        return false;
    };
    matches!(
        argv.as_slice(),
        [_, sub, token] if sub == "claude-hook" && canonical_hook_event(token) == Some(event)
    )
}

/// `agent uninstall claude` also retires
/// `~/.config/roost/claude-settings.json` — but only when it still holds
/// exactly what `claude install` used to write
/// ([`legacy_claude_settings_matches_generated_shape`]). A file a human
/// has since edited is not Roost's to delete; this leaves it and says
/// so, the same posture the install engine takes toward every other
/// file it does not fully recognize.
///
/// Resolves the path, then defers to [`legacy_claude_uninstall_at`] —
/// which is the whole of the logic, and testable against a temporary
/// directory rather than the developer's own `$HOME`.
fn legacy_claude_uninstall(guard: Guard) {
    let Ok(path) = legacy_claude_settings_path() else {
        return;
    };
    legacy_claude_uninstall_at(&path, guard);
}

/// This is a **delete**, so it takes the same harness jail every other
/// write in the agent-hooks path does. `roost_agent_install` refuses to
/// touch a real dotfile under `ROOST_TEST_MODE=1` without an explicit
/// `ROOST_AGENT_HOOKS_FORCE=1`; a cleanup that ran outside that fence
/// would delete the one file the engine's own guard cannot see.
fn legacy_claude_uninstall_at(path: &Path, guard: Guard) {
    if guard.check().is_err() {
        return;
    }
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
        Err(e) => {
            eprintln!("roostctl agent uninstall: {}: {e}", path.display());
            return;
        }
    };
    let doc: serde_json::Value = match serde_json::from_slice(&bytes) {
        Ok(v) => v,
        Err(_) => {
            eprintln!(
                "roostctl agent uninstall: {} no longer matches the shape `claude install` \
                 wrote; left in place",
                path.display()
            );
            return;
        }
    };
    if !legacy_claude_settings_matches_generated_shape(&doc) {
        eprintln!(
            "roostctl agent uninstall: {} no longer matches the shape `claude install` wrote; \
             left in place — remove it by hand if you no longer need it",
            path.display()
        );
        return;
    }
    match std::fs::remove_file(path) {
        Ok(()) => eprintln!("roostctl agent uninstall: removed {}", path.display()),
        Err(e) => eprintln!("roostctl agent uninstall: {}: {e}", path.display()),
    }
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

/// `tab.send_file`'s paths as the op wants them: absolute and existing.
///
/// Canonicalized in **roostctl's** cwd, because that is where the
/// caller typed them and the UI's cwd is not it — the op refuses a
/// relative path for exactly that reason. A path that is not there is
/// this side's error, naming the path, rather than a wire round-trip
/// that comes back saying the same thing later.
fn canonicalize_for_send_file(paths: &[PathBuf]) -> Result<Vec<String>, CliError> {
    paths
        .iter()
        .map(|path| {
            let resolved = std::fs::canonicalize(path)
                .map_err(|e| CliError::Usage(format!("cannot send {}: {e}", path.display())))?;
            resolved.into_os_string().into_string().map_err(|bad| {
                CliError::Usage(format!(
                    "cannot send {}: the resolved path is not valid UTF-8",
                    Path::new(&bad).display()
                ))
            })
        })
        .collect()
}

/// How long `tab send-file` waits for its paste to be queued.
///
/// §3.4 blocks until then "up to the upload budget", and `IpcClient`
/// has no request timeout of its own — so a wedged UI would hang
/// `roostctl` forever. One dial's worth of slack plus §3.3's per-file
/// budget (~298 s for a 10 MiB file at the modelled rate, rounded), and
/// the harness's `ROOST_TEST_TIMEOUT_SCALE` stretches it as it does
/// every other budget.
fn send_file_budget(paths: usize, scale: f64) -> Duration {
    Duration::from_secs(60 + 300 * paths as u64).mul_f64(scale)
}

/// What a verb's `--tab` holds: a bare local id, or a [`WireTabRef`] that
/// can also name a connected host's tab. `ROOST_TAB_ID` is read into the
/// same type, so the env reaches both kinds of verb alike — and a local id
/// in it must be positive, as [`parse_tab_id`] (the hooks' and doctor's
/// reading) requires, or a mutating verb would send `0` rather than refuse.
trait TabRef: Sized {
    fn from_env(raw: &str) -> Option<Self>;
    fn local(id: i64) -> Self;
}

impl TabRef for i64 {
    fn from_env(raw: &str) -> Option<Self> {
        parse_tab_id(raw)
    }

    fn local(id: i64) -> Self {
        id
    }
}

impl TabRef for WireTabRef {
    fn from_env(raw: &str) -> Option<Self> {
        WireTabRef::parse(raw).filter(|tab| match tab {
            WireTabRef::Local(id) => *id > 0,
            WireTabRef::Host { .. } => true,
        })
    }

    fn local(id: i64) -> Self {
        WireTabRef::Local(id)
    }
}

/// `--tab`, else `ROOST_TAB_ID` (empty counts as unset), else nothing.
fn named_tab<T: TabRef>(flag: Option<T>, tab_env: Option<&str>) -> Result<Option<T>, CliError> {
    match (flag, tab_env.filter(|raw| !raw.is_empty())) {
        (Some(tab), _) => Ok(Some(tab)),
        (None, Some(raw)) => T::from_env(raw)
            .map(Some)
            .ok_or_else(|| CliError::Usage(format!("ROOST_TAB_ID={raw} is not a tab id"))),
        (None, None) => Ok(None),
    }
}

/// The tab a verb in [`MUTATING_TAB_VERBS`] acts on — never the UI's
/// active tab. Pure, so the refusal comes before anything is dialled.
fn require_tab<T: TabRef>(
    verb: &str,
    flag: Option<T>,
    tab_env: Option<&str>,
) -> Result<T, CliError> {
    debug_assert!(
        MUTATING_TAB_VERBS.contains(&verb),
        "{verb} requires a tab but is missing from MUTATING_TAB_VERBS"
    );
    named_tab(flag, tab_env)?.ok_or_else(|| CliError::Usage(NO_TAB.to_string()))
}

/// The tab a read-only verb reads: `--tab`, `ROOST_TAB_ID`, or else the
/// UI's active tab — which is always local.
async fn resolve_tab_or_active<T: TabRef>(
    ui: &mut UiSocket<'_>,
    flag: Option<T>,
    tab_env: Option<&str>,
) -> Result<T, CliError> {
    if let Some(tab) = named_tab(flag, tab_env)? {
        return Ok(tab);
    }
    let resp = identify(ui.client().await?).await?;
    active_tab(&resp).map(T::local)
}

/// The UI's active tab, refused when it has none rather than sent
/// `tab_id = 0` for a confusing `not-found`.
fn active_tab(identify: &IdentifyResult) -> Result<i64, CliError> {
    if identify.active_tab_id == 0 {
        return Err(CliError::Usage(
            "no --tab, ROOST_TAB_ID is unset, and the UI reports no active tab; pass --tab".into(),
        ));
    }
    Ok(identify.active_tab_id)
}

/// A `--tab` given as text: a bare id or `h<host>.<id>`, checked before
/// anything is dialled.
fn parse_tab_flag(raw: &str) -> Result<WireTabRef, CliError> {
    WireTabRef::parse(raw).ok_or_else(|| CliError::Usage(format!("invalid --tab reference: {raw}")))
}

fn parse_state(s: &str) -> Result<TabState, CliError> {
    Ok(match s {
        "none" => TabState::None,
        "running" => TabState::Running,
        "needs_input" => TabState::NeedsInput,
        "idle" => TabState::Idle,
        other => return Err(CliError::Usage(format!("unknown state '{other}'"))),
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

/// Wrap `argv` (a command) so the tab persists after it exits (hold=true):
/// run the command, then `exec` a fresh interactive shell. Uses the
/// positional-args trick — `"$@"` runs the command, `"$0"` is the shell —
/// so `argv` needs no quoting/escaping. The wrapper is **`/bin/sh`** (so
/// the POSIX `$@`/`$0` work regardless of the user's `$SHELL` — fish, for
/// one, doesn't expose them in `-c`); `$0` is the user's `$SHELL`, which
/// `exec "$0" -i` re-launches interactively. Caller ensures `argv` is
/// non-empty. Mirrors the launcher's hold path (`custom_command::launch_argv`).
fn held_argv(shell: &str, argv: &[String]) -> Vec<String> {
    let mut out = vec![
        "/bin/sh".to_string(),
        "-c".to_string(),
        // `set +e` so an inherited `errexit` can't abort before the
        // `exec` when the command returns nonzero — `--hold` must still
        // hand off to the interactive shell.
        r#"set +e; "$@"; exec "$0" -i"#.to_string(),
        // $0 = the user's shell (re-launched by `exec "$0"`), then $1.. = argv.
        shell.to_string(),
    ];
    out.extend(argv.iter().cloned());
    out
}

/// The project's tab-id order with `new` moved to immediately after
/// `after`. `new` is assumed already present (tab.open appended it). If
/// `after` isn't in the list, the order is returned unchanged (new stays
/// at the end).
fn order_with_after(ids: &[i64], new: i64, after: i64) -> Vec<i64> {
    let base: Vec<i64> = ids.iter().copied().filter(|&id| id != new).collect();
    match base.iter().position(|&id| id == after) {
        Some(i) => {
            let mut out = Vec::with_capacity(ids.len());
            out.extend_from_slice(&base[..=i]);
            out.push(new);
            out.extend_from_slice(&base[i + 1..]);
            out
        }
        None => ids.to_vec(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `--tab` is the one flag that can name a tab on another machine,
    /// so what it accepts is a contract: both spellings parse, and a
    /// non-canonical one is refused rather than normalized — an
    /// aliasing parser would let a crafted `--tab` reach a tab its
    /// literal text never named.
    #[test]
    fn an_explicit_tab_ref_takes_both_spellings_and_only_canonical_ones() {
        assert_eq!(WireTabRef::parse("7"), Some(WireTabRef::Local(7)));
        assert_eq!(
            WireTabRef::parse("h2.7"),
            Some(WireTabRef::Host { host: 2, tab: 7 })
        );
        // `h0.*` is the local id-space, which is always spelled bare.
        for refused in ["h0.7", "007", "+7", "h2.", "", "h.7"] {
            assert!(WireTabRef::parse(refused).is_none(), "{refused:?}");
        }
    }

    /// `--scrollback` defaults to `0` — the wire value that
    /// `TabDumpParams` omits entirely — and otherwise parses through
    /// unvalidated (server-side clamping is the contract, not the CLI).
    #[test]
    fn dump_scrollback_defaults_to_zero_and_otherwise_parses_through() {
        let args = Args::try_parse_from(["roostctl", "tab", "dump", "--tab", "7"])
            .expect("a bare tab id parses");
        let Cmd::Tab(TabCmd::Dump { tab, scrollback }) = args.command else {
            panic!("expected tab dump")
        };
        assert_eq!(tab.as_deref(), Some("7"));
        assert_eq!(scrollback, 0);

        let args = Args::try_parse_from([
            "roostctl",
            "tab",
            "dump",
            "--tab",
            "7",
            "--scrollback",
            "500",
        ])
        .expect("--scrollback takes a count");
        let Cmd::Tab(TabCmd::Dump { scrollback, .. }) = args.command else {
            panic!("expected tab dump")
        };
        assert_eq!(scrollback, 500);
    }

    /// `tab send-file` requires `--tab` — unlike every other per-tab
    /// verb — and at least one path.
    #[test]
    fn send_file_requires_a_tab_and_at_least_one_path() {
        let args = Args::try_parse_from(["roostctl", "tab", "send-file", "--tab", "h2.7", "a.png"])
            .expect("a tab and a path parse");
        let Cmd::Tab(TabCmd::SendFile { tab, paths }) = args.command else {
            panic!("expected tab send-file")
        };
        assert_eq!(tab, "h2.7");
        assert_eq!(paths, vec![PathBuf::from("a.png")]);

        assert!(
            Args::try_parse_from(["roostctl", "tab", "send-file", "a.png"]).is_err(),
            "--tab must not fall back to the active tab"
        );
        assert!(
            Args::try_parse_from(["roostctl", "tab", "send-file", "--tab", "h2.7"]).is_err(),
            "a send with no files is a typo, not an empty batch"
        );
    }

    /// Paths are resolved in **roostctl's** cwd before they go on the
    /// wire, and a path that is not there is this side's error, naming
    /// the path.
    #[test]
    fn send_file_canonicalizes_in_its_own_cwd_and_names_a_missing_path() {
        let exe = std::env::current_exe().expect("the test binary is a real file");
        let canonical = std::fs::canonicalize(&exe).expect("canonicalize");
        let dir = canonical.parent().expect("the binary has a directory");

        // A detour through `..` is the same file once resolved: what
        // goes on the wire is absolute, `..`-free and symlink-free.
        let indirect = dir
            .join("..")
            .join(dir.file_name().expect("a named directory"))
            .join(canonical.file_name().expect("a named file"));
        assert_eq!(
            canonicalize_for_send_file(&[indirect]).expect("an existing path resolves"),
            vec![canonical.to_string_lossy().to_string()]
        );

        for missing in [
            PathBuf::from("roost-047-no-such-file"),
            std::env::temp_dir().join("roost-047-no-such-file"),
        ] {
            let error = canonicalize_for_send_file(std::slice::from_ref(&missing))
                .expect_err("a path that is not there is our error, not the wire's");
            assert!(
                error.message().contains(&missing.display().to_string()),
                "{error:?}"
            );
            assert_eq!(error.code(), "usage");
        }
    }

    /// A5 / §3.4: the block is bounded, and the bound grows with the
    /// batch rather than being one flat number a big drop outruns.
    #[test]
    fn the_send_file_wait_is_one_dial_plus_a_per_file_upload_budget() {
        assert_eq!(send_file_budget(1, 1.0), Duration::from_secs(360));
        assert_eq!(send_file_budget(4, 1.0), Duration::from_secs(1260));
        assert_eq!(
            send_file_budget(4, 2.5),
            Duration::from_secs(3150),
            "a scaled harness stretches it like every other budget"
        );
        assert!(
            send_file_budget(1, 1.0) > roost_ipc::session_launch::IPC_TIMEOUT,
            "and it is never the ordinary request timeout"
        );
    }

    #[test]
    fn held_argv_wraps_command_and_execs_shell() {
        let argv = held_argv("/bin/zsh", &["shed".into(), "console".into(), "x".into()]);
        assert_eq!(
            argv,
            vec![
                "/bin/sh", // POSIX wrapper (works even if $SHELL is fish)
                "-c",
                r#"set +e; "$@"; exec "$0" -i"#,
                "/bin/zsh", // $0 — the user's shell, re-launched interactively
                "shed",     // $1
                "console",  // $2
                "x",        // $3
            ]
        );
    }

    #[test]
    fn order_with_after_places_new_after_target() {
        // tab.open appended `3`; move it after `1`.
        assert_eq!(order_with_after(&[1, 2, 3], 3, 1), vec![1, 3, 2]);
        // After the (now second-to-last) tab — a no-op shuffle.
        assert_eq!(order_with_after(&[1, 2, 3], 3, 2), vec![1, 2, 3]);
        // `after` not present → unchanged (new stays at the end).
        assert_eq!(order_with_after(&[1, 2, 3], 3, 99), vec![1, 2, 3]);
    }

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
            BundleProfileKind::from(TargetArg::Linux),
            BundleProfileKind::Linux
        ));
        assert!(matches!(
            BundleProfileKind::from(TargetArg::Iced),
            BundleProfileKind::Iced
        ));
    }

    /// HS-0 fence: `BundleProfileKind::Session` exists, but `roostctl`
    /// cannot be pointed at a session. HS-1 defines session targeting;
    /// until then `--target session` must be rejected outright.
    #[test]
    fn session_is_not_a_target_flag_value() {
        use clap::ValueEnum;
        assert!(TargetArg::from_str("session", true).is_err());
        assert_eq!(
            TargetArg::value_variants()
                .iter()
                .filter_map(|v| v.to_possible_value().map(|p| p.get_name().to_string()))
                .collect::<Vec<_>>(),
            vec!["mac", "linux", "iced"]
        );
    }

    /// The two hook verbs, as the installed configs spell them.
    /// `agent-hook` takes the *agent*, never an event — the event comes
    /// from the payload, which is what lets one command string serve
    /// every event an agent has.
    #[test]
    fn both_hook_verbs_parse_from_argv() {
        let args = Args::try_parse_from(["roostctl", "agent-hook", "claude"]).unwrap();
        assert!(matches!(args.command, Cmd::AgentHook { agent } if agent == "claude"));
        // An agent with no adapter still parses: refusing here would
        // make a stale config exit non-zero at a decision dialog.
        assert!(Args::try_parse_from(["roostctl", "agent-hook", "amp"]).is_ok());
        assert!(Args::try_parse_from(["roostctl", "agent-hook"]).is_err());

        let legacy = Args::try_parse_from(["roostctl", "claude-hook", "Stop"]).unwrap();
        assert!(matches!(legacy.command, Cmd::ClaudeHook { event } if event == "Stop"));
    }

    /// The exact document a shipped writer produced — the complete
    /// event set, one group and one entry per event, every command
    /// `<exe> claude-hook <its own key>` — matches, whatever the exe
    /// path is (the path is the thing that varies per machine), and for
    /// every set that ever shipped.
    #[test]
    fn every_generated_event_set_matches() {
        for events in LEGACY_GENERATED_EVENT_SETS {
            for exe in ["/usr/local/bin/roostctl", "/Apps/My Roost.app/roostctl"] {
                let doc = legacy_document(exe, events, |event| event.to_string());
                assert!(
                    legacy_claude_settings_matches_generated_shape(&doc),
                    "{exe}: {doc}"
                );
            }
        }
    }

    /// The kebab-case command tokens the first writer (`bde60c4`) used
    /// under CamelCase keys normalize through `canonical_hook_event` the
    /// same as the canonical ones, so the oldest file in the wild still
    /// matches.
    #[test]
    fn the_first_writers_kebab_case_commands_still_match() {
        let doc = legacy_document(
            "/usr/local/bin/roostctl",
            LEGACY_GENERATED_EVENT_SETS[0],
            |event| match event {
                "SessionStart" => "session-start".into(),
                "UserPromptSubmit" => "prompt-submit".into(),
                "Notification" => "notification".into(),
                "Stop" => "stop".into(),
                "SessionEnd" => "session-end".into(),
                other => other.to_string(),
            },
        );
        assert!(
            legacy_claude_settings_matches_generated_shape(&doc),
            "{doc}"
        );
    }

    /// A generated document over `events`, with each event's command
    /// token spelled by `token`.
    fn legacy_document(
        exe: &str,
        events: &[&str],
        token: impl Fn(&str) -> String,
    ) -> serde_json::Value {
        let exe_quoted = quote_for_shell(exe);
        let hooks: serde_json::Map<String, serde_json::Value> = events
            .iter()
            .map(|event| {
                (
                    (*event).to_string(),
                    serde_json::json!([{
                        "hooks": [{
                            "type": "command",
                            "command": format!("{exe_quoted} claude-hook {}", token(event)),
                        }]
                    }]),
                )
            })
            .collect();
        serde_json::json!({ "hooks": hooks })
    }

    /// The whole point of the tightening: a file trimmed to the events
    /// its owner cared about is a **hand-edited** file, and
    /// `agent uninstall claude` may not delete it. A subset of a
    /// generated set — including the single-event case a looser matcher
    /// accepted — is the commonest edit there is.
    #[test]
    fn an_event_set_no_writer_produced_does_not_match() {
        let full = LEGACY_GENERATED_EVENT_SETS[1];
        let cases: &[&[&str]] = &[
            // Trimmed to one event, and to a few.
            &["Stop"],
            &["Stop", "SessionEnd"],
            &full[..full.len() - 1],
            // A complete set plus an event that set never carried.
            &[
                "SessionStart",
                "UserPromptSubmit",
                "Notification",
                "Stop",
                "StopFailure",
                "SessionEnd",
                "PreToolUse",
            ],
        ];
        for events in cases {
            let doc = legacy_document("/usr/local/bin/roostctl", events, |e| e.to_string());
            assert!(
                !legacy_claude_settings_matches_generated_shape(&doc),
                "{events:?} should not match: {doc}"
            );
        }
    }

    /// Two spellings of one event fill a set to the right size while an
    /// event is actually missing from it.
    #[test]
    fn one_event_under_two_spellings_does_not_match() {
        let mut doc = legacy_document(
            "/usr/local/bin/roostctl",
            LEGACY_GENERATED_EVENT_SETS[1],
            |e| e.to_string(),
        );
        let hooks = doc["hooks"].as_object_mut().unwrap();
        hooks.remove("SessionEnd");
        hooks.insert(
            "session-end".into(),
            serde_json::json!([{"hooks": [{
                "type": "command",
                "command": "/usr/local/bin/roostctl claude-hook session-end",
            }]}]),
        );
        // That one is still a real generated file, spelled the old way.
        assert!(
            legacy_claude_settings_matches_generated_shape(&doc),
            "{doc}"
        );

        // But both spellings at once is not.
        doc["hooks"].as_object_mut().unwrap().insert(
            "SessionEnd".into(),
            serde_json::json!([{"hooks": [{
                "type": "command",
                "command": "/usr/local/bin/roostctl claude-hook SessionEnd",
            }]}]),
        );
        assert!(
            !legacy_claude_settings_matches_generated_shape(&doc),
            "{doc}"
        );
    }

    /// A field added anywhere inside the generated shape is an edit: a
    /// `timeout` or a `matcher` are the two Claude's own docs invite,
    /// and the writer emitted neither.
    #[test]
    fn a_field_the_writer_never_emitted_does_not_match() {
        let inject: &[(&str, &str, serde_json::Value)] = &[
            ("entry", "timeout", serde_json::json!(30)),
            ("entry", "note", serde_json::json!("mine")),
            ("group", "matcher", serde_json::json!("*")),
            ("group", "note", serde_json::json!("mine")),
        ];
        for (where_, key, value) in inject {
            let mut doc = legacy_document(
                "/usr/local/bin/roostctl",
                LEGACY_GENERATED_EVENT_SETS[1],
                |e| e.to_string(),
            );
            assert!(legacy_claude_settings_matches_generated_shape(&doc));
            let group = &mut doc["hooks"]["Stop"][0];
            let target = match *where_ {
                "entry" => &mut group["hooks"][0],
                _ => group,
            };
            target
                .as_object_mut()
                .unwrap()
                .insert((*key).to_string(), value.clone());
            assert!(
                !legacy_claude_settings_matches_generated_shape(&doc),
                "{where_}.{key} should not match: {doc}"
            );
        }
    }

    /// Every other way a human (or a foreign tool) touching the file
    /// stops it from matching: an extra top-level key, a second group, a
    /// second handler, a non-command type, a command pointing at the
    /// wrong event or at another subcommand entirely, and a key
    /// `roost-agent` cannot resolve at all.
    #[test]
    fn a_hand_edited_or_foreign_document_does_not_match() {
        let full = |mutate: &dyn Fn(&mut serde_json::Value)| {
            let mut doc = legacy_document(
                "/usr/local/bin/roostctl",
                LEGACY_GENERATED_EVENT_SETS[1],
                |e| e.to_string(),
            );
            mutate(&mut doc);
            doc
        };
        let cases: Vec<serde_json::Value> = vec![
            serde_json::json!({}),
            serde_json::json!({"hooks": {}}),
            full(&|doc| {
                doc.as_object_mut()
                    .unwrap()
                    .insert("permissions".into(), serde_json::json!({}));
            }),
            full(&|doc| {
                doc["hooks"]["Stop"].as_array_mut().unwrap().push(
                    serde_json::json!({"hooks": [{"type": "command", "command": "echo mine"}]}),
                );
            }),
            full(&|doc| {
                doc["hooks"]["Stop"][0]["hooks"]
                    .as_array_mut()
                    .unwrap()
                    .push(serde_json::json!({"type": "command", "command": "echo mine"}));
            }),
            full(&|doc| {
                doc["hooks"]["Stop"][0]["hooks"][0]["type"] = serde_json::json!("prompt");
            }),
            full(&|doc| {
                doc["hooks"]["Stop"][0]["hooks"][0]["command"] =
                    serde_json::json!("/usr/local/bin/roostctl claude-hook SessionEnd");
            }),
            full(&|doc| {
                doc["hooks"]["Stop"][0]["hooks"][0]["command"] =
                    serde_json::json!("/usr/local/bin/roostctl tab list");
            }),
            full(&|doc| {
                let hooks = doc["hooks"].as_object_mut().unwrap();
                let stop = hooks.remove("Stop").unwrap();
                hooks.insert("NotAnEvent".into(), stop);
            }),
        ];
        for (i, doc) in cases.iter().enumerate() {
            assert!(
                !legacy_claude_settings_matches_generated_shape(doc),
                "case {i} should not match: {doc}"
            );
        }
    }

    /// The delete is real, so it takes the same harness fence every
    /// other agent-hooks write does. `ROOST_TEST_MODE=1` without an
    /// explicit force leaves the file exactly where it is — the install
    /// engine's own guard never sees this file, so nothing else would
    /// stop it.
    /// A unique scratch directory, the same way `session::tests` makes
    /// one: this crate has no tempdir dependency, and one test file is
    /// not a reason to take one.
    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "roostctl-legacy-test-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn the_legacy_delete_honours_the_harness_jail() {
        let dir = scratch("jail");
        let path = dir.join("claude-settings.json");
        let doc = legacy_document(
            "/usr/local/bin/roostctl",
            LEGACY_GENERATED_EVENT_SETS[1],
            |e| e.to_string(),
        );
        std::fs::write(&path, serde_json::to_string(&doc).unwrap()).unwrap();

        legacy_claude_uninstall_at(
            &path,
            Guard {
                test_mode: true,
                forced: false,
            },
        );
        assert!(path.exists(), "a jailed run deleted a real dotfile");

        legacy_claude_uninstall_at(
            &path,
            Guard {
                test_mode: true,
                forced: true,
            },
        );
        assert!(
            !path.exists(),
            "an explicitly forced run must still clean up"
        );
    }

    /// And a file that no longer matches survives a permitted run — the
    /// shape check, not the guard, is what stopped it.
    #[test]
    fn the_legacy_delete_leaves_a_hand_edited_file_alone() {
        let dir = scratch("hand-edited");
        let path = dir.join("claude-settings.json");
        let doc = legacy_document("/usr/local/bin/roostctl", &["Stop"], |e| e.to_string());
        std::fs::write(&path, serde_json::to_string(&doc).unwrap()).unwrap();

        legacy_claude_uninstall_at(&path, Guard::PERMITTED);
        assert!(path.exists(), "a trimmed file is not Roost's to delete");

        // Not even a file that is no longer JSON at all.
        std::fs::write(&path, "# my notes\n").unwrap();
        legacy_claude_uninstall_at(&path, Guard::PERMITTED);
        assert!(path.exists());
    }

    /// Every spelling a previously shipped `claude_install` wrote into
    /// `claude-settings.json` must still reach the same `roost-agent`
    /// arm as Claude's own `hook_event_name` — otherwise an
    /// already-installed settings file silently stops working the
    /// moment this binary is rebuilt.
    #[test]
    fn legacy_claude_install_spellings_reach_the_same_arm_as_canonical() {
        let payload = serde_json::json!({ "session_id": "s-1" });
        for (legacy, canonical) in [
            ("session-start", "SessionStart"),
            ("prompt-submit", "UserPromptSubmit"),
            ("notification", "Notification"),
            ("stop", "Stop"),
            ("session-end", "SessionEnd"),
        ] {
            let resolved = canonical_hook_event(legacy);
            assert_eq!(resolved, Some(canonical), "{legacy}");
            let via_legacy = claude_event_to_reports(resolved.unwrap(), &payload, 7);
            let via_canonical = claude_event_to_reports(canonical, &payload, 7);
            assert_eq!(via_legacy, via_canonical, "{legacy} vs {canonical}");
            assert!(
                !via_canonical.is_empty(),
                "{canonical} should map to a report"
            );
        }
    }

    // ------------------------------------------------------------------
    // Global `--json`, the error contract, and the target policy
    // ------------------------------------------------------------------

    /// Representative argv for every leaf verb — the least each needs to
    /// parse. [`every_verb_takes_json_before_and_after_itself`] fails when
    /// a verb is missing, so a new one cannot skip the global flag.
    const EVERY_VERB: &[&[&str]] = &[
        &["notify", "--title", "t"],
        &["set-title", "--title", "t"],
        &["identify"],
        &["wait", "--state", "idle"],
        &["events"],
        &["tab", "focus"],
        &["tab", "send-file", "--tab", "7", "a.png"],
        &["tab", "list"],
        &["tab", "set-state", "--state", "idle"],
        &["tab", "clear-notification"],
        &["tab", "open", "--project-id", "1"],
        &["tab", "close"],
        &["tab", "send", "--bytes", "x"],
        &["tab", "resize", "--cols", "80", "--rows", "24"],
        &["tab", "dump"],
        &["tab", "reorder", "--project-id", "1", "--order", "1,2"],
        &["project", "list"],
        &["project", "create"],
        &["project", "ensure", "--name", "n"],
        &["project", "rename", "--id", "1", "--name", "n"],
        &["project", "delete", "--id", "1"],
        &["project", "reorder", "--order", "1,2"],
        &["open", "--project", "n"],
        &["palette", "open"],
        &["palette", "state"],
        &["palette", "query", "q"],
        &["palette", "activate", "new_tab"],
        &["palette", "dismiss"],
        &["palette", "present", "--items", "[]"],
        &["screenshot", "--out", "shot.png"],
        &["render-stats"],
        &["claude-hook", "Stop"],
        &["agent-hook", "claude"],
        &["agent", "ensure"],
        &["agent", "set", "claude"],
        &["agent", "install", "claude"],
        &["agent", "uninstall", "claude"],
        &["agent", "status"],
        &["claude", "install"],
        &["session", "start"],
        &["session", "stop"],
        &["session", "status"],
        &["host", "add", "--label", "l", "--target", "t"],
        &["host", "list"],
        &["host", "status"],
        &["host", "remove", "--id", "a"],
        &["host", "connect", "--id", "a"],
        &["host", "disconnect", "--id", "a"],
        &["rpc", "tab.list"],
        &["skill"],
        &["doctor"],
    ];

    /// Every leaf verb as a user types it (`tab send`), with its clap
    /// command.
    fn leaf_verbs() -> Vec<(String, clap::Command)> {
        use clap::CommandFactory;
        let mut leaves = Vec::new();
        for sub in Args::command().get_subcommands() {
            if sub.has_subcommands() {
                for leaf in sub.get_subcommands() {
                    leaves.push((
                        format!("{} {}", sub.get_name(), leaf.get_name()),
                        leaf.clone(),
                    ));
                }
            } else {
                leaves.push((sub.get_name().to_string(), sub.clone()));
            }
        }
        leaves
    }

    fn verb_name(argv: &[&str]) -> String {
        use clap::CommandFactory;
        let root = Args::command();
        let first = root.find_subcommand(argv[0]).expect("a verb");
        if first.has_subcommands() {
            format!("{} {}", argv[0], argv[1])
        } else {
            argv[0].to_string()
        }
    }

    fn parse(argv: &[&str]) -> Result<Args, clap::Error> {
        Args::try_parse_from(std::iter::once("roostctl").chain(argv.iter().copied()))
    }

    /// A propagated name declared twice panics when clap builds the
    /// command, which is at every startup — so this is the test that the
    /// per-verb `--json` flags are really gone.
    #[test]
    fn the_command_line_definition_is_well_formed() {
        use clap::CommandFactory;
        Args::command().debug_assert();
    }

    #[test]
    fn every_verb_takes_json_before_and_after_itself() {
        let mut tabled: Vec<String> = EVERY_VERB.iter().map(|argv| verb_name(argv)).collect();
        tabled.sort();
        let mut leaves: Vec<String> = leaf_verbs().into_iter().map(|(name, _)| name).collect();
        leaves.sort();
        assert_eq!(
            tabled, leaves,
            "EVERY_VERB must name every leaf verb exactly once"
        );

        for argv in EVERY_VERB {
            let bare = parse(argv).unwrap_or_else(|e| panic!("{argv:?}: {e}"));
            assert!(!bare.json, "{argv:?}");

            let before: Vec<&str> = std::iter::once("--json")
                .chain(argv.iter().copied())
                .collect();
            let after: Vec<&str> = argv.iter().copied().chain(["--json"]).collect();
            for spelled in [before, after] {
                let args = parse(&spelled).unwrap_or_else(|e| panic!("{spelled:?}: {e}"));
                assert!(args.json, "{spelled:?}");
            }
        }
    }

    /// The Mac app spawns these and decodes their stdout; there is no
    /// Linux lane that runs the spawn, so the argv is pinned here.
    #[test]
    fn the_argv_the_mac_app_spawns_still_parses() {
        let args = Args::try_parse_from(["roostctl", "agent", "ensure", "--json"])
            .expect("the Mac app's agent ensure argv");
        assert!(args.json);
        assert!(matches!(
            args.command,
            Cmd::Agent(agent_install::AgentCmd::Ensure { startup: false })
        ));

        for argv in [
            &["roostctl", "agent", "ensure", "--startup", "--json"][..],
            &["roostctl", "agent", "status", "--json"],
            &["roostctl", "agent", "set", "--local", "claude", "--json"],
        ] {
            let args = Args::try_parse_from(argv).unwrap_or_else(|e| panic!("{argv:?}: {e}"));
            assert!(args.json, "{argv:?}");
            assert!(matches!(args.command, Cmd::Agent(_)), "{argv:?}");
        }
    }

    #[test]
    fn a_refused_command_line_reads_json_off_the_raw_argv() {
        let argv: Vec<OsString> = ["roostctl", "--json", "tab", "list", "--bogus"]
            .into_iter()
            .map(OsString::from)
            .collect();
        let error = Args::try_parse_from(&argv).expect_err("--bogus is refused");
        assert!(asks_for_json(&argv));
        let usage = CliError::from_clap(&error);
        assert_eq!(usage.code(), "usage");
        let rendered: serde_json::Value =
            serde_json::from_str(&usage.render(true)).expect("one JSON document");
        assert_eq!(rendered["error"]["code"], "usage");
        assert!(
            rendered["error"]["message"]
                .as_str()
                .is_some_and(|m| m.contains("--bogus")),
            "{rendered}"
        );
        assert_eq!(refuse_command_line(&error, &argv), 2);

        let plain: Vec<OsString> = ["roostctl", "tab", "list", "--bogus"]
            .into_iter()
            .map(OsString::from)
            .collect();
        assert!(!asks_for_json(&plain));

        // Past `--` the words are the command `tab open` runs.
        let separated: Vec<OsString> = ["roostctl", "tab", "open", "--", "jq", "--json"]
            .into_iter()
            .map(OsString::from)
            .collect();
        assert!(!asks_for_json(&separated));
    }

    /// A socket path in a fresh scratch directory that nothing listens on.
    fn nowhere(tag: &str) -> String {
        scratch(tag).join("nothing.sock").display().to_string()
    }

    async fn run_argv(argv: &[&str], tab_env: Option<&str>) -> Result<i32, CliError> {
        run_argv_env(argv, tab_env, None).await
    }

    /// [`run_argv`] plus an injected `$PWD` — the `open` / `project
    /// ensure` `--cwd` default, tested the same way [`run_argv`] tests
    /// `ROOST_TAB_ID`: a parameter, never a real `chdir` or a dependency
    /// on the test runner's own working directory.
    async fn run_argv_env(
        argv: &[&str],
        tab_env: Option<&str>,
        cwd_env: Option<&str>,
    ) -> Result<i32, CliError> {
        run(
            parse(argv).unwrap_or_else(|e| panic!("{argv:?}: {e}")),
            tab_env,
            cwd_env,
        )
        .await
    }

    #[tokio::test]
    async fn no_ui_listening_is_a_connection_failure() {
        let socket = nowhere("no-ui");
        let error = run_argv(&["--socket", &socket, "tab", "list"], None)
            .await
            .expect_err("nothing is listening");
        assert!(matches!(error, CliError::Connection(_)), "{error:?}");
        assert_eq!(error.exit_code(), 1);
        assert!(error.message().contains(&socket), "{error:?}");
    }

    /// A stand-in UI on a real socket. It answers each request through
    /// `answer` and records what it was asked, so a test can say what
    /// `roostctl` sent.
    struct FakeUi {
        socket: String,
        requests: std::sync::Arc<std::sync::Mutex<Vec<(String, serde_json::Value)>>>,
    }

    type Answer = fn(&str) -> Result<serde_json::Value, (&'static str, &'static str)>;

    impl FakeUi {
        fn start(tag: &str, answer: Answer) -> Self {
            use roost_ipc::messages::{RawRequest, Response};
            use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

            let socket = scratch(tag).join("ui.sock");
            let listener = tokio::net::UnixListener::bind(&socket).expect("bind the fake UI");
            let requests = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
            let seen = requests.clone();
            tokio::spawn(async move {
                while let Ok((stream, _)) = listener.accept().await {
                    let seen = seen.clone();
                    tokio::spawn(async move {
                        let (read, mut write) = stream.into_split();
                        let mut lines = BufReader::new(read).lines();
                        while let Ok(Some(line)) = lines.next_line().await {
                            let request: RawRequest =
                                serde_json::from_str(&line).expect("a request frame");
                            seen.lock()
                                .unwrap()
                                .push((request.op.clone(), request.params.clone()));
                            let response = match answer(&request.op) {
                                Ok(result) => Response::ok(request.id, result),
                                Err((code, message)) => Response::err(request.id, code, message),
                            };
                            let mut frame = serde_json::to_vec(&response).unwrap();
                            frame.push(b'\n');
                            if write.write_all(&frame).await.is_err() {
                                return;
                            }
                        }
                    });
                }
            });
            Self {
                socket: socket.display().to_string(),
                requests,
            }
        }

        fn take(&self) -> Vec<(String, serde_json::Value)> {
            std::mem::take(&mut self.requests.lock().unwrap())
        }
    }

    fn fake_identify(active_tab_id: i64) -> serde_json::Value {
        serde_json::json!({
            "socket_path": "/tmp/fake.sock",
            "pid": 1,
            "active_project_id": "1",
            "active_tab_id": active_tab_id.to_string(),
            "app_label": "Roost",
            "app_id": "test",
            "ui_version": "0",
            "protocol_version": 1,
        })
    }

    #[tokio::test]
    async fn a_server_refusal_reaches_the_caller_verbatim() {
        let ui = FakeUi::start("server-refusal", |_| Err(("not-found", "no tab 7")));
        let error = run_argv(
            &["--socket", &ui.socket, "tab", "close", "--tab", "7"],
            None,
        )
        .await
        .expect_err("the server refused");
        assert_eq!(
            error,
            CliError::Server {
                code: "not-found".into(),
                message: "no tab 7".into()
            }
        );
        assert_eq!(error.exit_code(), 1);
    }

    #[tokio::test]
    async fn a_wait_that_never_holds_times_out_with_exit_4() {
        let ui = FakeUi::start("wait-timeout", |op| match op {
            "identify" => Ok(fake_identify(0)),
            "tab.list" => Ok(serde_json::json!({"projects": []})),
            _ => Err(("unknown-op", "not faked")),
        });
        let error = run_argv(
            &[
                "--socket",
                &ui.socket,
                "wait",
                "--tab",
                "7",
                "--state",
                "idle",
                "--timeout",
                "0",
            ],
            None,
        )
        .await
        .expect_err("tab 7 never appears");
        assert!(matches!(error, CliError::Timeout(_)), "{error:?}");
        assert_eq!(error.exit_code(), 4);
        assert_eq!(error.code(), "timeout");
    }

    fn mutating_argv(verb: &str) -> Vec<&'static str> {
        EVERY_VERB
            .iter()
            .find(|argv| verb_name(argv) == verb)
            .unwrap_or_else(|| panic!("{verb} is in MUTATING_TAB_VERBS with no argv in EVERY_VERB"))
            .to_vec()
    }

    /// Refused against a socket nothing listens on, so a verb that dialled
    /// before checking its tab would fail `connection`, never `usage`.
    #[tokio::test]
    async fn a_mutating_verb_with_no_tab_refuses_before_dialling() {
        let socket = nowhere("refuse");
        for verb in MUTATING_TAB_VERBS {
            let argv: Vec<&str> = ["--socket", socket.as_str()]
                .into_iter()
                .chain(mutating_argv(verb))
                .collect();
            for unset in [None, Some("")] {
                let refused = run_argv(&argv, unset).await.expect_err(verb);
                assert_eq!(refused, CliError::Usage(NO_TAB.into()), "{verb} {unset:?}");
                assert_eq!(refused.exit_code(), 2);
            }
        }
    }

    #[tokio::test]
    async fn roost_tab_id_or_the_flag_satisfies_a_mutating_verb() {
        let ui = FakeUi::start("satisfied", |_| Ok(serde_json::json!({})));
        for verb in MUTATING_TAB_VERBS {
            let argv: Vec<&str> = ["--socket", ui.socket.as_str()]
                .into_iter()
                .chain(mutating_argv(verb))
                .collect();
            assert_eq!(run_argv(&argv, Some("7")).await, Ok(0), "{verb}");
            let flagged: Vec<&str> = argv.iter().copied().chain(["--tab", "8"]).collect();
            assert_eq!(run_argv(&flagged, Some("7")).await, Ok(0), "{verb}");

            let sent = ui.take();
            let tab_ids: Vec<&serde_json::Value> =
                sent.iter().map(|(_, params)| &params["tab_id"]).collect();
            assert_eq!(tab_ids, ["7", "8"], "{verb}: {sent:?}");
            assert!(
                sent.iter().all(|(op, _)| op != ops::IDENTIFY),
                "{verb} asked for the active tab: {sent:?}"
            );
        }
    }

    #[tokio::test]
    async fn an_unusable_roost_tab_id_is_usage() {
        let socket = nowhere("bad-env");
        for argv in [
            vec!["--socket", socket.as_str(), "tab", "close"],
            vec!["--socket", socket.as_str(), "tab", "focus"],
        ] {
            for raw in ["seven", "0", "-3"] {
                let refused = run_argv(&argv, Some(raw)).await.expect_err("refused");
                assert_eq!(
                    refused,
                    CliError::Usage(format!("ROOST_TAB_ID={raw} is not a tab id")),
                    "{argv:?}"
                );
            }
        }
    }

    #[tokio::test]
    async fn the_read_only_verbs_fall_back_to_the_active_tab() {
        let ui = FakeUi::start("fallback", |op| match op {
            "identify" => Ok(fake_identify(5)),
            "tab.list" => Ok(serde_json::json!({"projects": []})),
            "tab.dump" => Ok(serde_json::json!({"cols": 80, "rows": 1, "rows_text": [""]})),
            _ => Err(("unknown-op", "not faked")),
        });

        assert_eq!(
            run_argv(&["--socket", &ui.socket, "tab", "dump"], None).await,
            Ok(0)
        );
        let sent = ui.take();
        assert_eq!(sent[0].0, ops::IDENTIFY, "{sent:?}");
        assert_eq!(
            (sent[1].0.as_str(), &sent[1].1["tab_id"]),
            (ops::TAB_DUMP, &"5".into())
        );

        assert_eq!(
            run_argv(
                &["--socket", &ui.socket, "wait", "--gone", "--timeout", "0"],
                None
            )
            .await,
            Ok(0)
        );
        assert_eq!(ui.take()[0].0, ops::IDENTIFY);

        // `ROOST_TAB_ID` still comes before the active tab.
        assert_eq!(
            run_argv(&["--socket", &ui.socket, "tab", "dump"], Some("9")).await,
            Ok(0)
        );
        let sent = ui.take();
        assert_eq!(sent.len(), 1, "{sent:?}");
        assert_eq!(sent[0].1["tab_id"], "9");
    }

    // ------------------------------------------------------------------
    // `open` / `project ensure` (plan 066 §3.2)
    // ------------------------------------------------------------------

    /// `identify.ops` advertising `project.ensure` (and the rest `open`
    /// needs), with no backend switch in flight.
    fn fake_identify_supports_ensure() -> serde_json::Value {
        let mut identity = fake_identify(0);
        identity["ops"] =
            serde_json::json!(["identify", "project.ensure", "tab.open", "tab.focus",]);
        identity
    }

    /// The same, with a backend switch reported in progress.
    fn fake_identify_ensure_mid_switch() -> serde_json::Value {
        let mut identity = fake_identify_supports_ensure();
        identity["local_backend_switch"] = serde_json::json!("attach_remote");
        identity
    }

    fn fake_project(id: i64, cwd: &str) -> serde_json::Value {
        serde_json::json!({
            "id": id.to_string(),
            "name": "proj",
            "cwd": cwd,
            "position": 0,
            "created_at": 0,
        })
    }

    fn fake_project_ensure_result() -> serde_json::Value {
        serde_json::json!({"project": fake_project(42, "/ensured/cwd"), "created": true})
    }

    fn fake_tab(id: i64, project_id: i64) -> serde_json::Value {
        serde_json::json!({
            "id": id.to_string(),
            "project_id": project_id.to_string(),
            "title": "roostctl",
            "cwd": "/ensured/cwd",
            "state": "none",
            "has_notification": false,
            "is_active": false,
            "user_titled": false,
            "position": 0,
            "created_at": 0,
            "last_active": 0,
            "hook_active": false,
        })
    }

    fn fake_tab_open_result() -> serde_json::Value {
        serde_json::json!({"tab": fake_tab(99, 42)})
    }

    fn answer_ensure_and_open(op: &str) -> Result<serde_json::Value, (&'static str, &'static str)> {
        match op {
            "identify" => Ok(fake_identify_supports_ensure()),
            "project.ensure" => Ok(fake_project_ensure_result()),
            "tab.open" => Ok(fake_tab_open_result()),
            "tab.focus" => Ok(serde_json::json!({})),
            _ => Err(("unknown-op", "not faked")),
        }
    }

    #[test]
    fn open_argv_parses_the_project_command_and_json_either_side() {
        let parsed = parse(&["open", "--project", "X", "--", "cmd", "a", "b"]).expect("parses");
        match parsed.command {
            Cmd::Open {
                project,
                argv,
                hold,
                focus,
                ..
            } => {
                assert_eq!(project, "X");
                assert_eq!(argv, vec!["cmd", "a", "b"]);
                assert!(!hold);
                assert!(!focus);
            }
            other => panic!("{other:?}"),
        }

        for spelled in [
            vec!["--json", "open", "--project", "X"],
            vec!["open", "--project", "X", "--json"],
        ] {
            let args = parse(&spelled).unwrap_or_else(|e| panic!("{spelled:?}: {e}"));
            assert!(args.json, "{spelled:?}");
            assert!(matches!(args.command, Cmd::Open { .. }), "{spelled:?}");
        }
    }

    #[test]
    fn project_ensure_argv_parses() {
        let parsed = parse(&["project", "ensure", "--name", "X"]).expect("parses");
        match parsed.command {
            Cmd::Project(ProjectCmd::Ensure { name, cwd }) => {
                assert_eq!(name, "X");
                assert_eq!(cwd, None);
            }
            other => panic!("{other:?}"),
        }
    }

    #[tokio::test]
    async fn open_and_project_ensure_default_cwd_to_the_injected_pwd() {
        let ui = FakeUi::start("cwd-default", answer_ensure_and_open);

        assert_eq!(
            run_argv_env(
                &["--socket", &ui.socket, "project", "ensure", "--name", "proj"],
                None,
                Some("/injected/pwd"),
            )
            .await,
            Ok(0)
        );
        let sent = ui.take();
        assert_eq!(sent[1].0, ops::PROJECT_ENSURE, "{sent:?}");
        assert_eq!(sent[1].1["cwd"], "/injected/pwd", "{sent:?}");

        assert_eq!(
            run_argv_env(
                &["--socket", &ui.socket, "open", "--project", "proj"],
                None,
                Some("/injected/pwd"),
            )
            .await,
            Ok(0)
        );
        let sent = ui.take();
        assert_eq!(
            (sent[1].0.as_str(), &sent[1].1["cwd"]),
            (ops::PROJECT_ENSURE, &"/injected/pwd".into())
        );
        assert_eq!(
            (sent[2].0.as_str(), &sent[2].1["cwd"]),
            (ops::TAB_OPEN, &"/injected/pwd".into())
        );
    }

    #[tokio::test]
    async fn open_sequences_ensure_then_tab_open_and_focuses_only_with_the_flag() {
        let ui = FakeUi::start("open-sequence", answer_ensure_and_open);

        assert_eq!(
            run_argv_env(
                &[
                    "--socket",
                    &ui.socket,
                    "open",
                    "--project",
                    "proj",
                    "--cwd",
                    "/ensured/cwd",
                ],
                None,
                None,
            )
            .await,
            Ok(0)
        );
        let sent = ui.take();
        let seen: Vec<&str> = sent.iter().map(|(op, _)| op.as_str()).collect();
        assert_eq!(
            seen,
            [ops::IDENTIFY, ops::PROJECT_ENSURE, ops::TAB_OPEN],
            "{sent:?}"
        );
        assert_eq!(sent[1].1["name"], "proj");
        assert_eq!(
            sent[2].1["project_id"], "42",
            "must use the ensured project's id"
        );

        assert_eq!(
            run_argv_env(
                &[
                    "--socket",
                    &ui.socket,
                    "open",
                    "--project",
                    "proj",
                    "--cwd",
                    "/ensured/cwd",
                    "--focus",
                ],
                None,
                None,
            )
            .await,
            Ok(0)
        );
        let sent = ui.take();
        let seen: Vec<&str> = sent.iter().map(|(op, _)| op.as_str()).collect();
        assert_eq!(
            seen,
            [
                ops::IDENTIFY,
                ops::PROJECT_ENSURE,
                ops::TAB_OPEN,
                ops::TAB_FOCUS
            ],
            "{sent:?}"
        );
        assert_eq!(sent[3].1["tab_id"], "99", "focuses the newly opened tab");
    }

    #[tokio::test]
    async fn open_hold_rewrites_argv_like_tab_open_does() {
        let ui = FakeUi::start("open-hold", answer_ensure_and_open);
        assert_eq!(
            run_argv_env(
                &[
                    "--socket",
                    &ui.socket,
                    "open",
                    "--project",
                    "proj",
                    "--cwd",
                    "/x",
                    "--hold",
                    "--",
                    "make",
                    "test",
                ],
                None,
                None,
            )
            .await,
            Ok(0)
        );
        let sent = ui.take();
        let opened = sent
            .iter()
            .find(|(op, _)| op == ops::TAB_OPEN)
            .expect("tab.open sent");
        let argv: Vec<String> = serde_json::from_value(opened.1["argv"].clone()).unwrap();
        assert_eq!(
            argv,
            held_argv(
                &std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into()),
                &["make".to_string(), "test".to_string()]
            )
        );
    }

    #[tokio::test]
    async fn unsupported_when_identify_has_no_ops_field_sends_nothing_else() {
        let ui = FakeUi::start("no-ops-field", |op| match op {
            "identify" => Ok(fake_identify(0)), // no `ops` field at all
            _ => Err(("unknown-op", "not faked")),
        });
        for argv in [
            vec![
                "--socket",
                ui.socket.as_str(),
                "project",
                "ensure",
                "--name",
                "x",
            ],
            vec!["--socket", ui.socket.as_str(), "open", "--project", "x"],
        ] {
            let error = run_argv(&argv, None).await.expect_err(&format!("{argv:?}"));
            assert_eq!(error.code(), "unsupported", "{argv:?}: {error:?}");
            assert_eq!(error.exit_code(), 1, "{argv:?}");
            let sent = ui.take();
            assert_eq!(
                sent.iter().map(|(op, _)| op.as_str()).collect::<Vec<_>>(),
                vec![ops::IDENTIFY],
                "{argv:?}: {sent:?}"
            );
        }
    }

    #[tokio::test]
    async fn unsupported_when_ops_lacks_project_ensure_sends_nothing_else() {
        let ui = FakeUi::start("ops-without-ensure", |op| match op {
            "identify" => {
                let mut identity = fake_identify(0);
                identity["ops"] = serde_json::json!(["identify", "tab.open", "tab.list"]);
                Ok(identity)
            }
            _ => Err(("unknown-op", "not faked")),
        });
        for argv in [
            vec![
                "--socket",
                ui.socket.as_str(),
                "project",
                "ensure",
                "--name",
                "x",
            ],
            vec!["--socket", ui.socket.as_str(), "open", "--project", "x"],
        ] {
            let error = run_argv(&argv, None).await.expect_err(&format!("{argv:?}"));
            assert_eq!(error.code(), "unsupported", "{argv:?}: {error:?}");
            let sent = ui.take();
            assert_eq!(
                sent.iter().map(|(op, _)| op.as_str()).collect::<Vec<_>>(),
                vec![ops::IDENTIFY],
                "{argv:?}: {sent:?}"
            );
        }
    }

    #[tokio::test]
    async fn open_refuses_usage_during_a_backend_switch_before_project_ensure() {
        let ui = FakeUi::start("switch-in-progress", |op| match op {
            "identify" => Ok(fake_identify_ensure_mid_switch()),
            _ => Err(("unknown-op", "not faked")),
        });
        let error = run_argv(
            &[
                "--socket",
                &ui.socket,
                "open",
                "--project",
                "x",
                "--cwd",
                "/x",
            ],
            None,
        )
        .await
        .expect_err("a switch is in progress");
        assert_eq!(
            error,
            CliError::Usage("a backend switch is in progress; retry".into())
        );
        let sent = ui.take();
        assert_eq!(
            sent.iter().map(|(op, _)| op.as_str()).collect::<Vec<_>>(),
            vec![ops::IDENTIFY],
            "{sent:?}"
        );
    }

    #[tokio::test]
    async fn open_passes_through_a_not_found_from_tab_open_verbatim() {
        let ui = FakeUi::start("open-not-found", |op| match op {
            "identify" => Ok(fake_identify_supports_ensure()),
            "project.ensure" => Ok(fake_project_ensure_result()),
            "tab.open" => Err(("not-found", "project 42 is gone")),
            _ => Err(("unknown-op", "not faked")),
        });
        let error = run_argv(
            &[
                "--socket",
                &ui.socket,
                "open",
                "--project",
                "x",
                "--cwd",
                "/x",
            ],
            None,
        )
        .await
        .expect_err("the project vanished between the two calls");
        assert_eq!(
            error,
            CliError::Server {
                code: "not-found".into(),
                message: "project 42 is gone".into(),
            }
        );
    }

    // ------------------------------------------------------------------
    // `rpc`
    // ------------------------------------------------------------------

    #[test]
    fn rpc_argv_parses_with_and_without_params() {
        let bare = parse(&["rpc", "tab.list"]).expect("no params");
        match bare.command {
            Cmd::Rpc { op, params } => {
                assert_eq!(op, "tab.list");
                assert_eq!(params, None);
            }
            other => panic!("{other:?}"),
        }

        let with_params =
            parse(&["rpc", "tab.write", r#"{"tab_id":"4"}"#]).expect("params argument");
        match with_params.command {
            Cmd::Rpc { op, params } => {
                assert_eq!(op, "tab.write");
                assert_eq!(params.as_deref(), Some(r#"{"tab_id":"4"}"#));
            }
            other => panic!("{other:?}"),
        }

        let from_stdin = parse(&["rpc", "tab.write", "-"]).expect("dash for stdin");
        assert!(
            matches!(&from_stdin.command, Cmd::Rpc { params: Some(p), .. } if p == "-"),
            "{:?}",
            from_stdin.command
        );

        for spelled in [
            vec!["--json", "rpc", "tab.list"],
            vec!["rpc", "tab.list", "--json"],
        ] {
            let args = parse(&spelled).unwrap_or_else(|e| panic!("{spelled:?}: {e}"));
            assert!(args.json, "{spelled:?}");
            assert!(matches!(args.command, Cmd::Rpc { .. }), "{spelled:?}");
        }
    }

    /// Params that don't parse as JSON, or that parse as something other
    /// than an object, are refused before the socket is dialled — the
    /// same "checked before it's dialled" posture [`require_tab`] takes,
    /// proven the same way: nothing is listening at `socket`, so a verb
    /// that dialled first would fail `connection`, never `usage`.
    #[tokio::test]
    async fn rpc_params_must_be_a_json_object() {
        let socket = nowhere("rpc-bad-params");
        for bad in ["[1,2,3]", "\"a string\"", "42", "not json", ""] {
            let refused = run_argv(&["--socket", &socket, "rpc", "tab.list", bad], None)
                .await
                .expect_err(bad);
            assert!(matches!(refused, CliError::Usage(_)), "{bad}: {refused:?}");
            assert_eq!(refused.exit_code(), 2, "{bad}");
        }
    }

    #[tokio::test]
    async fn rpc_sends_the_op_verbatim_with_params_or_an_empty_object() {
        let ui = FakeUi::start("rpc-ok", |_| Ok(serde_json::json!({"ok": true})));

        assert_eq!(
            run_argv(&["--socket", &ui.socket, "rpc", "tab.list"], None).await,
            Ok(0)
        );
        assert_eq!(
            ui.take(),
            vec![("tab.list".to_string(), serde_json::json!({}))]
        );

        assert_eq!(
            run_argv(
                &[
                    "--socket",
                    &ui.socket,
                    "rpc",
                    "tab.write",
                    r#"{"tab_id":"4"}"#,
                ],
                None,
            )
            .await,
            Ok(0)
        );
        assert_eq!(
            ui.take(),
            vec![("tab.write".to_string(), serde_json::json!({"tab_id": "4"}))]
        );
    }

    /// The server's own `unknown-op` reaches the caller verbatim — `rpc`
    /// validates nothing about the op name itself.
    #[tokio::test]
    async fn rpc_passes_an_unknown_op_through_to_the_servers_refusal() {
        let ui = FakeUi::start("rpc-unknown-op", |_| {
            Err(("unknown-op", "no such op: no.such.op"))
        });
        let error = run_argv(&["--socket", &ui.socket, "rpc", "no.such.op"], None)
            .await
            .expect_err("the server refused");
        assert_eq!(
            error,
            CliError::Server {
                code: "unknown-op".into(),
                message: "no such op: no.such.op".into(),
            }
        );
        assert_eq!(error.exit_code(), 1);
    }

    /// `require_tab`'s call sites are the policy; the const is what a
    /// reader (and the skill) sees of it. Read off this file's own source
    /// so a handler cannot refuse without being listed, nor be listed
    /// without refusing.
    #[test]
    fn the_mutating_list_names_exactly_the_verbs_that_call_require_tab() {
        let source = include_str!("main.rs");
        let needle = concat!("require", "_tab(");
        let mut called: Vec<&str> = source
            .match_indices(needle)
            .filter_map(|(at, _)| {
                let rest = source[at + needle.len()..].trim_start().strip_prefix('"')?;
                Some(&rest[..rest.find('"')?])
            })
            .collect();
        called.sort_unstable();
        let mut listed = MUTATING_TAB_VERBS.to_vec();
        listed.sort_unstable();
        assert_eq!(called, listed);
    }

    /// A new verb that takes `--tab` has to be put on one side or the
    /// other, so it cannot pick up the active-tab fallback by omission.
    #[test]
    fn every_verb_that_takes_a_tab_is_mutating_or_a_known_reader() {
        const READERS: &[&str] = &["wait", "events", "tab dump", "tab send-file", "doctor"];
        let mut with_tab: Vec<String> = leaf_verbs()
            .into_iter()
            .filter(|(_, cmd)| cmd.get_arguments().any(|arg| arg.get_id() == "tab"))
            .map(|(name, _)| name)
            .collect();
        with_tab.sort();
        let mut classified: Vec<String> = MUTATING_TAB_VERBS
            .iter()
            .chain(READERS)
            .map(|verb| verb.to_string())
            .collect();
        classified.sort();
        assert_eq!(with_tab, classified);
    }

    // `roostctl skill` and the SKILL.md it embeds (plan 066 §3.3).

    /// The executable fence marker; a harness replays exactly these.
    const RECIPE_FENCE: &str = "```bash roost-recipe";

    /// Each ` ```bash roost-recipe ` fence as its logical lines — a
    /// trailing `\` joins the next line — with the SKILL.md line number
    /// each starts on.
    fn recipe_fences(doc: &str) -> Vec<Vec<(usize, String)>> {
        let mut fences = Vec::new();
        let mut open: Option<Vec<(usize, String)>> = None;
        let mut continued: Option<(usize, String)> = None;
        for (index, line) in doc.lines().enumerate() {
            let number = index + 1;
            let Some(fence) = open.as_mut() else {
                if line.trim_end() == RECIPE_FENCE {
                    open = Some(Vec::new());
                } else {
                    assert!(
                        !(line.starts_with("```") && line.contains("roost-recipe")),
                        "SKILL.md:{number}: a misspelt recipe fence would be skipped: {line}"
                    );
                }
                continue;
            };
            if line.trim_end() == "```" {
                assert!(
                    continued.is_none(),
                    "SKILL.md:{number}: a fence ends on `\\`"
                );
                if let Some(fence) = open.take() {
                    fences.push(fence);
                }
                continue;
            }
            let (start, mut text) = continued.take().unwrap_or((number, String::new()));
            match line.strip_suffix('\\') {
                Some(head) => {
                    text.push_str(head);
                    continued = Some((start, text));
                }
                None => {
                    text.push_str(line);
                    fence.push((start, text));
                }
            }
        }
        assert!(open.is_none(), "SKILL.md ends inside a recipe fence");
        fences
    }

    #[derive(Debug, PartialEq)]
    enum ShellToken {
        Word(String),
        /// `|`, `;`, `&`, `$(`, `)`, a redirection: whatever ends a command.
        Operator,
    }

    /// `"$PWD"` → `/tmp`; any other variable (`$tab`, `$ROOST_TAB_ID`) → a
    /// tab id, which is also a valid string for every other flag.
    fn expand_variable(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) -> String {
        let braced = chars.next_if_eq(&'{').is_some();
        let mut name = String::new();
        while let Some(c) = chars.next_if(|c| c.is_ascii_alphanumeric() || *c == '_') {
            name.push(c);
        }
        if braced {
            assert_eq!(
                chars.next(),
                Some('}'),
                "only a plain ${{name}} is expanded"
            );
        }
        if name.is_empty() {
            return "$".into();
        }
        if name == "PWD" {
            "/tmp".into()
        } else {
            "7".into()
        }
    }

    /// Split one recipe line the way `sh` would, as far as recovering each
    /// `roostctl` argv needs: quotes, escapes, variables, comments, and the
    /// operators that end a command. A construct it does not model panics
    /// rather than being skipped.
    fn shell_tokens(line: &str) -> Vec<ShellToken> {
        let mut tokens = Vec::new();
        let mut word: Option<String> = None;
        let mut chars = line.chars().peekable();
        fn end(word: &mut Option<String>, tokens: &mut Vec<ShellToken>) {
            tokens.extend(word.take().map(ShellToken::Word));
        }
        while let Some(c) = chars.next() {
            match c {
                c if c.is_whitespace() => end(&mut word, &mut tokens),
                '#' if word.is_none() => break,
                '\'' => {
                    let text = word.get_or_insert_with(String::new);
                    loop {
                        match chars.next().expect("an unclosed single quote") {
                            '\'' => break,
                            c => text.push(c),
                        }
                    }
                }
                '"' => {
                    let text = word.get_or_insert_with(String::new);
                    loop {
                        match chars.next().expect("an unclosed double quote") {
                            '"' => break,
                            '\\' => {
                                let next = chars.next().expect("a trailing backslash");
                                if !matches!(next, '"' | '\\' | '$' | '`') {
                                    text.push('\\');
                                }
                                text.push(next);
                            }
                            '$' => {
                                assert!(
                                    chars.peek() != Some(&'('),
                                    "unparsed: $( inside quotes in `{line}`"
                                );
                                text.push_str(&expand_variable(&mut chars));
                            }
                            '`' => panic!("unparsed: a backtick in `{line}`"),
                            c => text.push(c),
                        }
                    }
                }
                '\\' => {
                    let next = chars.next().expect("a trailing backslash");
                    word.get_or_insert_with(String::new).push(next);
                }
                '$' if chars.peek() == Some(&'(') => {
                    chars.next();
                    end(&mut word, &mut tokens);
                    tokens.push(ShellToken::Operator);
                }
                '$' => {
                    let value = expand_variable(&mut chars);
                    word.get_or_insert_with(String::new).push_str(&value);
                }
                '`' => panic!("unparsed: a backtick in `{line}`"),
                '|' | ';' | '&' | '(' | ')' | '<' | '>' => {
                    end(&mut word, &mut tokens);
                    tokens.push(ShellToken::Operator);
                }
                c => word.get_or_insert_with(String::new).push(c),
            }
        }
        end(&mut word, &mut tokens);
        tokens
    }

    /// Every `roostctl` a recipe line runs, as the argv after the program
    /// name, with the placeholders `N` and `X` given real values.
    fn roostctl_argvs(line: &str) -> Vec<Vec<String>> {
        let tokens = shell_tokens(line);
        tokens
            .iter()
            .enumerate()
            .filter(|(_, token)| **token == ShellToken::Word("roostctl".into()))
            .map(|(at, _)| {
                tokens[at + 1..]
                    .iter()
                    .map_while(|token| match token {
                        ShellToken::Word(word) => Some(match word.as_str() {
                            "N" => "7".to_string(),
                            "X" => "review".to_string(),
                            _ => word.clone(),
                        }),
                        ShellToken::Operator => None,
                    })
                    .collect()
            })
            .collect()
    }

    /// The splitter is what the parity test trusts, so it is pinned on the
    /// shapes the recipes use.
    #[test]
    fn the_recipe_splitter_recovers_each_roostctl_argv() {
        let strings = |words: &[&str]| words.iter().map(|w| w.to_string()).collect::<Vec<_>>();
        assert_eq!(
            roostctl_argvs(
                r#"tab=$(roostctl open --project X --cwd "$PWD" --json -- sh -c 'a; echo "b $?"' | jq -r .tab.id)"#
            ),
            vec![strings(&[
                "open",
                "--project",
                "review",
                "--cwd",
                "/tmp",
                "--json",
                "--",
                "sh",
                "-c",
                r#"a; echo "b $?""#,
            ])]
        );
        assert_eq!(
            roostctl_argvs("timeout 30 roostctl events --tab N | jq -c . # watch"),
            vec![strings(&["events", "--tab", "7"])]
        );
        assert_eq!(
            roostctl_argvs(
                r#"roostctl wait --tab "$tab" --text 'a b'; roostctl tab dump --tab ${tab}"#
            ),
            vec![
                strings(&["wait", "--tab", "7", "--text", "a b"]),
                strings(&["tab", "dump", "--tab", "7"]),
            ]
        );
        assert_eq!(
            roostctl_argvs(
                r#"roostctl tab send --tab N --bytes 'echo "x "y $?\n' && roostctl identify"#
            ),
            vec![
                strings(&["tab", "send", "--tab", "7", "--bytes", r#"echo "x "y $?\n"#]),
                strings(&["identify"]),
            ]
        );
        assert!(roostctl_argvs("# roostctl identify").is_empty());
    }

    /// Every `roostctl` in every executable fence parses through clap, so
    /// a recipe pins its options and not only its verb names.
    #[test]
    fn every_skill_recipe_parses_as_a_roostctl_command_line() {
        let fences = recipe_fences(SKILL);
        assert!(!fences.is_empty(), "SKILL.md has no `{RECIPE_FENCE}` fence");
        for fence in &fences {
            let mut invocations = 0;
            for (number, line) in fence {
                let argvs = roostctl_argvs(line);
                if line.contains("roostctl") {
                    assert!(
                        !argvs.is_empty(),
                        "SKILL.md:{number}: `{line}` names roostctl but runs none"
                    );
                }
                for argv in argvs {
                    invocations += 1;
                    let full = std::iter::once("roostctl".to_string()).chain(argv.iter().cloned());
                    if let Err(error) = Args::try_parse_from(full) {
                        panic!("SKILL.md:{number}: `{line}` does not parse as {argv:?}:\n{error}");
                    }
                }
            }
            let first = fence.first().map_or(0, |(number, _)| *number);
            assert!(
                invocations > 0,
                "the recipe fence at SKILL.md:{first} runs no roostctl"
            );
        }
    }

    /// The skill's rule about `--tab` is the agent-facing copy of
    /// [`MUTATING_TAB_VERBS`], so a verb added there must be named here.
    #[test]
    fn the_skill_tab_rule_names_every_mutating_verb() {
        let (_, rules) = SKILL.split_once("\n## Rules\n").expect("a Rules section");
        let rules = rules.split("\n## ").next().unwrap_or(rules);
        let mut items: Vec<String> = Vec::new();
        let mut in_item = false;
        for line in rules.lines() {
            if let Some(start) = line.strip_prefix("- ") {
                items.push(start.to_string());
                in_item = true;
            } else if line.trim().is_empty() || line.starts_with('|') {
                in_item = false;
            } else if in_item {
                let item = items.last_mut().expect("a continued item");
                item.push(' ');
                item.push_str(line.trim());
            }
        }
        let items: Vec<String> = items
            .iter()
            .map(|item| item.split_whitespace().collect::<Vec<_>>().join(" "))
            .collect();
        let rule = items
            .iter()
            .find(|item| item.contains("`--tab`"))
            .expect("a Rules item about `--tab`");
        assert!(rule.starts_with("Always pass `--tab`"), "{rule}");
        for verb in MUTATING_TAB_VERBS {
            assert!(
                rule.contains(&format!("`{verb}`")),
                "the --tab rule does not name `{verb}`: {rule}"
            );
        }
    }

    /// AC3: what `skill` prints is the file the plugin installs, read from
    /// disk rather than through the same `include_str!`.
    #[test]
    fn skill_prints_skill_md_byte_for_byte() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../skills/roost/SKILL.md");
        let on_disk = std::fs::read_to_string(&path).expect("skills/roost/SKILL.md");

        let mut printed = Vec::new();
        assert_eq!(write_skill(&mut printed, false), Ok(0));
        assert!(
            printed == on_disk.as_bytes(),
            "`roostctl skill` printed {} bytes, SKILL.md has {}",
            printed.len(),
            on_disk.len()
        );

        let mut printed = Vec::new();
        assert_eq!(write_skill(&mut printed, true), Ok(0));
        let envelope: serde_json::Value =
            serde_json::from_slice(&printed).expect("one JSON document");
        assert_eq!(
            envelope,
            serde_json::json!({"topic": "roost", "format": "markdown", "content": on_disk})
        );
    }

    #[test]
    fn a_reader_that_hangs_up_on_skill_is_not_a_failure() {
        struct Refuses(std::io::ErrorKind);
        impl Write for Refuses {
            fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
                Err(self.0.into())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        assert_eq!(
            write_skill(&mut Refuses(std::io::ErrorKind::BrokenPipe), false),
            Ok(0)
        );
        let error = write_skill(&mut Refuses(std::io::ErrorKind::StorageFull), false)
            .expect_err("a full disk is a failure");
        assert_eq!(error.code(), "failed");
    }

    #[test]
    fn the_skill_frontmatter_names_roost_and_says_when_to_use_it() {
        let rest = SKILL
            .strip_prefix("---\n")
            .expect("SKILL.md opens with frontmatter");
        let (front, _) = rest.split_once("\n---\n").expect("the frontmatter closes");
        let field = |key: &str| {
            front
                .lines()
                .find_map(|line| line.strip_prefix(key)?.strip_prefix(": "))
        };
        assert_eq!(field("name"), Some("roost"));
        let description = field("description").expect("a description");
        // Quoted: it holds `: `, which bare YAML would read as a mapping.
        let text = description
            .strip_prefix('"')
            .and_then(|d| d.strip_suffix('"'))
            .unwrap_or_else(|| panic!("the description is not double-quoted: {description}"));
        assert!(text.contains("Roost"), "{text}");
    }

    #[test]
    fn the_plugin_manifests_carry_proxs_fields_at_the_workspace_version() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let read = |rel: &str| {
            std::fs::read_to_string(root.join(rel)).unwrap_or_else(|e| panic!("{rel}: {e}"))
        };
        let cargo = read("Cargo.toml");
        let (_, package) = cargo
            .split_once("\n[workspace.package]\n")
            .expect("a [workspace.package] table");
        let version = package
            .lines()
            .take_while(|line| !line.starts_with('['))
            .find_map(|line| {
                let (key, value) = line.split_once('=')?;
                (key.trim() == "version").then(|| value.trim().trim_matches('"'))
            })
            .expect("[workspace.package] version");

        let plugin: serde_json::Value =
            serde_json::from_str(&read(".claude-plugin/plugin.json")).expect("plugin.json");
        assert_eq!(plugin["name"], "roost");
        assert_eq!(plugin["version"], version);
        assert_eq!(plugin["repository"], "https://github.com/charliek/roost");
        for key in ["description", "homepage", "license"] {
            assert!(
                plugin[key].as_str().is_some_and(|value| !value.is_empty()),
                "plugin.json {key}"
            );
        }
        assert!(
            plugin["author"]["name"]
                .as_str()
                .is_some_and(|name| !name.is_empty()),
            "plugin.json author.name"
        );
        assert!(
            plugin["keywords"]
                .as_array()
                .is_some_and(|keywords| keywords.iter().any(|k| k == "roost")),
            "plugin.json keywords"
        );

        let marketplace: serde_json::Value =
            serde_json::from_str(&read(".claude-plugin/marketplace.json"))
                .expect("marketplace.json");
        assert_eq!(marketplace["name"], "roost");
        assert_eq!(marketplace["plugins"][0]["name"], "roost");
        assert_eq!(marketplace["plugins"][0]["source"], "./");
    }
}
