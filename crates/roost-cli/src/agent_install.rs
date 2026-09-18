//! `roostctl agent` — wire Roost's hook entries into the supported
//! agents' own config files, and take them out again.
//!
//! Five verbs over `roost-agent-install`. Four of them never dial a UI:
//! they read and write dotfiles, so they work with nothing running,
//! which is exactly when a user reaches for them. `set --local` is the
//! fifth and shares that property.
//!
//! **`set` without `--local` is the one verb that does dial** ([`run_over_ipc`]).
//! It puts `agent.set_hooks` to the running UI, which sets this
//! machine's key *and* raises every connected non-localhost host in the
//! same call — the half a `--local` write cannot do, because a host is
//! reached through a connection only the UI holds.
//!
//! `ensure` here is the explicit reconcile — it wires what the key allows
//! **and takes out what it does not**. `--startup` is the other shape,
//! the one a UI launch runs: wire and refresh, remove nothing (plan 064
//! §3.2). The Mac app spawns that one, because a launch must not undo a
//! hand edit.
//!
//! Every verb reads the config, and reads the **same** file and parser
//! the UIs do (`roost-ui-model`), so `agent-hooks` means one thing on
//! this machine rather than one thing per surface. `install`, `uninstall`
//! and `set --local` are explicit instructions and *move the key* rather
//! than ignoring it: `agent install codex` while the key says `off` wires
//! codex and adds it to the list, so the next launch does not treat what
//! the user just asked for as unconsented. `status` changes nothing at
//! all.

use clap::Subcommand;
use roost_agent::Agent;
use roost_agent_install::{
    ensure, install, reconcile, resolve_names, set_hooks, status, uninstall, AgentSkip, Guard,
    Home, Mode, Outcome, Status, ALL_AGENTS,
};
use roost_ipc::messages::{
    ops, AgentHooksOutcome, AgentSetHooksAgents, AgentSetHooksHostOutcome, AgentSetHooksParams,
    AgentSetHooksResult,
};
use roost_ui_model::config::{AgentHooks, RoostConfig};

use crate::error::CliError;
use crate::UiSocket;

/// How this client identifies itself in the state record.
const BY: &str = "local";

#[derive(Subcommand, Debug)]
pub enum AgentCmd {
    /// Bring this machine in line with `agent-hooks` in `config.conf`:
    /// wire and refresh what it names, and take Roost's entries out of
    /// what it does not. Safe to run any number of times, and a run with
    /// nothing to do writes nothing.
    Ensure {
        /// Wire and refresh what `agent-hooks` names, and remove
        /// nothing. What a UI runs at launch: a launch must never undo a
        /// hook somebody added by hand, and it must never act on a key
        /// that changed while the app was closed (plan 064 §3.2).
        #[arg(long, default_value_t = false)]
        startup: bool,
    },
    /// Set `agent-hooks` to exactly this list (or `off`), then bring
    /// this machine's files in line with it — the explicit answer to
    /// the consent dialog's question, from a terminal.
    ///
    /// By default this dials the running UI, which sets the key here and
    /// raises every connected non-localhost host to at least the same
    /// list in the same call. `--local` writes this machine's
    /// `config.conf` directly instead, under the install lock and with
    /// nothing running — and reaches no host at all, so a connected one
    /// does not learn about the change until it connects again.
    Set {
        /// A comma list of agent names (`claude`, `codex`, `grok`,
        /// `cursor`, `opencode`), or the literal `off`. `ask`/`auto` are
        /// not accepted: this verb answers the consent question, and
        /// "unanswered" is not an answer to give it.
        spec: String,
        /// Write this machine's `agent-hooks` key directly instead of
        /// dialing the running UI. No host is told.
        #[arg(long, default_value_t = false)]
        local: bool,
    },
    /// Wire one agent, or all of them, and add it to `agent-hooks`.
    /// Explicit wins: this works even when the key says `off`.
    Install {
        /// `claude`, `codex`, `grok`, `cursor`, or `opencode`.
        agent: Option<String>,
        #[arg(long, default_value_t = false)]
        all: bool,
    },
    /// Remove Roost's entries from one agent, or all of them, and take
    /// it back out of `agent-hooks` (`--all` writes `off`). Only what
    /// Roost wrote comes out — a hook you wrote that happens to mention
    /// `$ROOST_AGENT_HOOK` stays exactly where it is.
    Uninstall {
        agent: Option<String>,
        #[arg(long, default_value_t = false)]
        all: bool,
    },
    /// Per agent: installed, wired at which integration version, and
    /// whether anything is out of date.
    Status,
}

/// `Ok(1)` rather than an error for a partial failure: it still has an
/// outcome worth printing, so the report goes to stdout and the code
/// says whether anything in it failed.
pub fn run(cmd: &AgentCmd, json: bool) -> Result<i32, CliError> {
    let home = Home::from_env().map_err(CliError::failed)?;
    let guard = Guard::from_env();

    match cmd {
        AgentCmd::Ensure { startup } => match configured() {
            // `--startup` passes no mode: `ensure` re-reads the key
            // inside the lock. This read decides only whether there is
            // an answer to act on at all — the Mac spawns exactly this
            // at launch, and the key can be answered between its read
            // and this process's write.
            Some(_) if *startup => report(ensure(&home, BY, guard), json),
            Some(mode) => report(reconcile(&home, &mode, BY, guard), json),
            None => {
                // `--json` is a machine contract — the Mac app spawns
                // exactly this and decodes stdout — so the unconfigured
                // path answers in the shape an ensure would (an empty
                // outcome) and puts its one human sentence on stderr,
                // rather than breaking the decode with prose.
                let note = "agent-hooks is not configured; nothing was wired. Choose agents \
                            in Roost (Agent Hooks… in the command palette) or run `roostctl \
                            agent set <list|off> --local`.";
                if json {
                    println!("{}", outcome_json(&Outcome::default()));
                    eprintln!("{note}");
                } else {
                    println!("{note}");
                }
                Ok(0)
            }
        },
        // Unreachable through `main`, which sends this shape to
        // [`run_over_ipc`]. Refused rather than written: the one thing a
        // misroute must never do is quietly set the local key when the
        // caller asked for every connected host to be set too.
        AgentCmd::Set { local: false, .. } => Err(CliError::Usage(
            "agent set: the UI-routed form is not served here".into(),
        )),
        AgentCmd::Set { spec, .. } => {
            let mode = parse_set_spec(spec).map_err(CliError::Usage)?;
            let allow = matches!(mode, Mode::Allow(_));
            let code = report(set_hooks(&home, &mode, BY, guard), json)?;
            if allow {
                let note = "a connected host will not see this until it connects again";
                if json {
                    eprintln!("{note}");
                } else {
                    println!("{note}");
                }
            }
            Ok(code)
        }
        AgentCmd::Install { agent, all } => {
            let agents = targets(agent.as_deref(), *all)?;
            report(install(&home, &agents, BY, guard), json)
        }
        AgentCmd::Uninstall { agent, all } => {
            let agents = targets(agent.as_deref(), *all)?;
            let outcome = uninstall(&home, &agents, guard);
            // The legacy `claude-settings.json` this crate never
            // touches — it predates this crate — so `agent
            // uninstall claude` cleans it up as a side effect,
            // never in place of the ordinary uninstall above.
            //
            // "As a side effect" is load-bearing: a run that never
            // unwired Claude has no business deleting Claude's
            // legacy file. `uninstall` returns `Err` when the
            // harness guard refused it or the lock could not be
            // taken, and names a per-agent error when the write
            // failed — either way the delete is off, or a refused
            // `agent uninstall claude` would exit 1 while reporting
            // it had removed the file it just deleted.
            let cleanup = agents.contains(&Agent::Claude)
                && matches!(&outcome, Ok(o) if o.errors.iter().all(|e| e.agent != Agent::Claude));
            let code = report(outcome, json)?;
            if cleanup {
                crate::legacy_claude_uninstall(guard);
            }
            Ok(code)
        }
        AgentCmd::Status => {
            let (key, mode) = resolved_or_nothing();
            let rows =
                status(&home, &mode).map_err(|e| CliError::Failed(format!("agent status: {e}")))?;
            print_status(&rows, key.unknown(), json);
            Ok(0)
        }
    }
}

/// Whether this verb is served by [`run_over_ipc`] instead of [`run`].
///
/// Exactly one shape is: `set` without `--local`. Every other `agent`
/// verb must keep working with nothing running, which is why `main`
/// asks this before it dials the UI socket rather than after.
pub fn dials_the_ui(cmd: &AgentCmd) -> bool {
    matches!(cmd, AgentCmd::Set { local: false, .. })
}

/// `roostctl agent set <list|off>` — the UI-routed form, over
/// `agent.set_hooks` (plan 064 §3.4).
///
/// The spec is parsed here, before anything is dialled, so an unknown
/// name is the same exit 2 `--local` gives rather than a round trip that
/// ends in `invalid-param`.
pub async fn run_over_ipc(
    cmd: &AgentCmd,
    ui: &mut UiSocket<'_>,
    json: bool,
) -> Result<i32, CliError> {
    let AgentCmd::Set { spec, .. } = cmd else {
        return Err(CliError::Usage(format!(
            "agent: {cmd:?} does not dial the UI"
        )));
    };
    let mode = parse_set_spec(spec).map_err(CliError::Usage)?;
    let params = AgentSetHooksParams {
        agents: match &mode {
            Mode::Off => AgentSetHooksAgents::Off,
            Mode::Allow(agents) => {
                AgentSetHooksAgents::List(agents.iter().map(|a| a.source().to_string()).collect())
            }
        },
    };
    let result: AgentSetHooksResult = ui.call(ops::AGENT_SET_HOOKS, params).await?;
    if json {
        let body = serde_json::to_string(&result).map_err(CliError::failed)?;
        println!("{body}");
    } else {
        print_set_result(&result);
    }
    // A host that could not be asked counts, and so does one that was
    // asked and could not write: the caller told this machine and every
    // host it is connected to, and only part of that happened. A
    // per-agent failure riding inside a nominally successful `result` is
    // still a file that did not get written — reporting 0 for it is how
    // a script concludes every host is wired when one is not.
    let failed = !result.local.errors.is_empty()
        || result.hosts.iter().any(|host| match host {
            AgentSetHooksHostOutcome::Error { .. } => true,
            AgentSetHooksHostOutcome::Result { result, .. } => !result.errors.is_empty(),
        });
    Ok(i32::from(failed))
}

/// `agent.set_hooks`'s reply in [`print_outcome`]'s shape, plus the two
/// things only this op has: where the key landed, and one line per host.
fn print_set_result(result: &AgentSetHooksResult) {
    println!("config: {}", result.config_path);
    print_wire_outcome(&result.local);
    for entry in &result.hosts {
        match entry {
            AgentSetHooksHostOutcome::Result { host, result } => {
                let changed: Vec<&str> = result
                    .wired
                    .iter()
                    .chain(&result.refreshed)
                    .map(String::as_str)
                    .collect();
                if changed.is_empty() {
                    // "already current" only when there is genuinely
                    // nothing to say. A host that wired nothing because
                    // the agent is not installed there, or because its
                    // `config.toml` would not parse, has a reason — and
                    // reporting that as "current" is how somebody
                    // concludes a host is wired when it is not.
                    if result.skipped.is_empty() {
                        println!("on {host}: already current");
                    } else {
                        println!("on {host}: nothing wired");
                    }
                } else {
                    println!("on {host}: {}", changed.join(", "));
                }
                for skip in &result.skipped {
                    println!("on {host}: skipped {}: {}", skip.agent, skip.reason);
                }
                for failure in &result.errors {
                    eprintln!("on {host}: error {}: {}", failure.agent, failure.error);
                }
            }
            AgentSetHooksHostOutcome::Error { host, error } => {
                eprintln!("on {host}: {error}");
            }
        }
    }
}

/// [`print_outcome`] for the wire shape. The two differ in what they
/// have to say — an [`Outcome`] carries `current` and `warnings`, which
/// no reply does — so they are two renderings of two types rather than
/// one over a lowest common denominator.
fn print_wire_outcome(outcome: &AgentHooksOutcome) {
    for (label, agents) in [
        ("wired", &outcome.wired),
        ("refreshed", &outcome.refreshed),
        ("removed", &outcome.removed),
    ] {
        if !agents.is_empty() {
            println!("{label}: {}", agents.join(", "));
        }
    }
    for skip in &outcome.skipped {
        println!("skipped {}: {}", skip.agent, skip.reason);
    }
    for error in &outcome.errors {
        eprintln!("error {}: {}", error.agent, error.error);
    }
    if outcome.wired.is_empty() && outcome.refreshed.is_empty() && outcome.removed.is_empty() {
        println!("nothing to do");
    }
}

/// The resolved `agent-hooks` key — `None` when nobody has answered the
/// consent dialog yet (plan 064), which `Ensure` reports rather than
/// wiring or unwiring anything.
fn configured() -> Option<Mode> {
    Mode::from_config(&RoostConfig::load_default().agent_hooks)
}

/// The key as it stands, and what a *reader* resolves it to: unanswered
/// means nothing is allowed yet, because reading is never a reason to
/// guess at a consent nobody gave.
///
/// Both halves, from one load: `status` prints a row for every name in
/// the key this build has no adapter for, and parsing `config.conf` a
/// second time to reach it would repeat every warning its other keys
/// emit.
///
/// `pub(crate)` for doctor, which renders the same rows `status` prints
/// and must resolve the key the same way.
pub(crate) fn resolved_or_nothing() -> (AgentHooks, Mode) {
    let key = RoostConfig::load_default().agent_hooks;
    let mode = Mode::from_config(&key).unwrap_or(Mode::Allow(Vec::new()));
    (key, mode)
}

/// Parse `agent set`'s argument: a comma list of agent names, or the
/// literal `off`.
///
/// Not [`roost_ui_model::config::AgentHooks::parse`], which is the
/// config *reader*: it is deliberately lenient — a value naming one
/// unrecognised agent beside real ones still resolves, with a warning,
/// to the names it does know, because a stale `config.conf` must keep
/// working. `set` is the opposite kind of call: a user command whose
/// only job is recording consent has no honest partial answer, so an
/// unknown name refuses the whole list rather than silently narrowing
/// it — and it is refused here for **both** routes, the UI-routed form
/// included ([`run_over_ipc`] parses the spec before it sends),
/// so this reuses it instead of a third copy of the rule.
///
/// It stays that way after plan 065 §3.1 made the *wire* ops skip an
/// unknown name instead of refusing it: those serve a peer that may know
/// agents this binary does not, where this serves a person who has just
/// typed one in.
fn parse_set_spec(spec: &str) -> Result<Mode, String> {
    let trimmed = spec.trim();
    if trimmed.eq_ignore_ascii_case("off") {
        return Ok(Mode::Off);
    }
    let (agents, unknown) = resolve_names(trimmed.split(','));
    if let Some(name) = unknown.first() {
        return Err(format!(
            "no agent named {name:?} ({})",
            roost_agent_install::agent_names()
        ));
    }
    if agents.is_empty() {
        return Err(format!(
            "name at least one agent ({}), or pass `off`",
            roost_agent_install::agent_names()
        ));
    }
    Ok(Mode::Allow(agents))
}

fn targets(agent: Option<&str>, all: bool) -> Result<Vec<Agent>, CliError> {
    match (agent, all) {
        (Some(_), true) => Err(CliError::Usage(
            "agent: pass an agent name or --all, not both".into(),
        )),
        (None, false) => Err(CliError::Usage(format!(
            "agent: name an agent ({}) or pass --all",
            roost_agent_install::agent_names()
        ))),
        (None, true) => Ok(ALL_AGENTS.to_vec()),
        (Some(name), false) => Agent::parse(name)
            .map(|agent| vec![agent])
            .ok_or_else(|| CliError::Usage(format!("agent: unknown agent: {name}"))),
    }
}

fn report(
    outcome: Result<Outcome, roost_agent_install::InstallError>,
    json: bool,
) -> Result<i32, CliError> {
    let outcome = outcome.map_err(|e| CliError::Failed(format!("agent: {e}")))?;
    if json {
        println!("{}", outcome_json(&outcome));
    } else {
        print_outcome(&outcome);
    }
    Ok(i32::from(!outcome.is_clean()))
}

fn names(agents: &[Agent]) -> Vec<&'static str> {
    agents.iter().map(|a| a.source()).collect()
}

fn skip_pairs(skipped: &[AgentSkip]) -> Vec<(&'static str, String)> {
    skipped
        .iter()
        .map(|skip| (skip.agent.source(), skip.reason.to_string()))
        .collect()
}

fn outcome_json(outcome: &Outcome) -> serde_json::Value {
    serde_json::json!({
        "wired": names(&outcome.wired),
        "refreshed": names(&outcome.refreshed),
        "current": names(&outcome.current),
        "removed": names(&outcome.removed),
        "skipped": skip_pairs(&outcome.skipped)
            .into_iter()
            .map(|(agent, reason)| serde_json::json!({ "agent": agent, "reason": reason }))
            .collect::<Vec<_>>(),
        "warnings": outcome.warnings.iter()
            .map(|w| serde_json::json!({
                "agent": w.agent.source(),
                "warning": w.warning.to_string(),
            }))
            .collect::<Vec<_>>(),
        "errors": outcome.errors.iter()
            .map(|e| serde_json::json!({
                "agent": e.agent.source(),
                "error": e.error.to_string(),
            }))
            .collect::<Vec<_>>(),
    })
}

fn print_outcome(outcome: &Outcome) {
    for (label, agents) in [
        ("wired", &outcome.wired),
        ("refreshed", &outcome.refreshed),
        ("already current", &outcome.current),
        ("removed", &outcome.removed),
    ] {
        if !agents.is_empty() {
            println!("{label}: {}", names(agents).join(", "));
        }
    }
    for (agent, reason) in skip_pairs(&outcome.skipped) {
        println!("skipped {agent}: {reason}");
    }
    for warning in &outcome.warnings {
        println!("warning {}: {}", warning.agent.source(), warning.warning);
    }
    for error in &outcome.errors {
        eprintln!("error {}: {}", error.agent.source(), error.error);
    }
    if outcome.wired.is_empty()
        && outcome.refreshed.is_empty()
        && outcome.removed.is_empty()
        && outcome.current.is_empty()
    {
        println!("nothing to do");
    }
}

/// The status table, plus one row per `agent-hooks` name this build has
/// no adapter for.
///
/// They are shown rather than filtered because they are the *only*
/// evidence a user has that their key says something this binary cannot
/// act on — the key itself keeps them (a newer Roost put them there), so
/// a status that hid them would read as if the name had been thrown
/// away.
fn print_status(rows: &[Status], unknown: &[String], json: bool) {
    if json {
        let body: Vec<serde_json::Value> = rows
            .iter()
            .map(|row| {
                serde_json::json!({
                    "agent": row.agent.source(),
                    "present": row.present,
                    "wired": row.wired,
                    "entries_on_disk": row.entries_on_disk,
                    "up_to_date": row.up_to_date,
                    "noticed": row.noticed,
                    "allowed": row.allowed,
                    "files": row.files.iter().map(|p| p.display().to_string()).collect::<Vec<_>>(),
                    "skipped": row.skipped.as_ref().map(ToString::to_string),
                    "warnings": row.warnings.iter().map(ToString::to_string).collect::<Vec<_>>(),
                })
            })
            .chain(
                unknown
                    .iter()
                    .map(|name| serde_json::json!({"agent": name, "unknown_to_this_build": true})),
            )
            .collect();
        println!("{}", serde_json::Value::Array(body));
        return;
    }

    for row in rows {
        println!("{:<9} {}", row.agent.source(), status_line(row));
        if let Some(reason) = &row.skipped {
            if row.present {
                println!("          skipped: {reason}");
            }
        }
        for warning in &row.warnings {
            println!("          warning: {warning}");
        }
    }
    for name in unknown {
        println!("{name:<9} unknown to this build");
    }
}

/// Three independent facts per row, in the order that reads: is the
/// agent here, may Roost touch it, and what is actually wired.
///
/// `wired` is the state record's claim and `entries_on_disk` is the
/// agent's own files; they can disagree, and saying which is which is
/// the difference between a status line and a guess.
fn status_line(row: &Status) -> String {
    if !row.present {
        return "not installed".to_string();
    }
    let allowed = if row.allowed {
        "allowed"
    } else {
        "not allowed"
    };
    let mut clauses = vec!["present".to_string(), allowed.to_string()];
    // Nothing wired and nothing claimed adds no third clause: "present"
    // has already said it.
    match (row.entries_on_disk, row.wired, row.up_to_date) {
        (false, None, _) => {}
        (false, Some(version), _) => clauses.push(format!(
            "record says wired@v{version}, nothing wired on disk"
        )),
        (true, None, _) => clauses.push("wired on disk, not in the state record".to_string()),
        (true, Some(version), false) => clauses.push(format!("wired@v{version}, out of date")),
        (true, Some(version), true) if version != roost_agent_install::INTEGRATION_VERSION => {
            clauses.push(format!(
                "wired and current on disk, record still says v{version}"
            ))
        }
        (true, Some(version), true) => clauses.push(format!("wired@v{version}")),
    }
    clauses.join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[derive(Parser, Debug)]
    struct Wrapper {
        #[command(subcommand)]
        cmd: AgentCmd,
    }

    fn parse(args: &[&str]) -> AgentCmd {
        Wrapper::try_parse_from(std::iter::once("agent").chain(args.iter().copied()))
            .unwrap()
            .cmd
    }

    /// `--json` is `roostctl`'s global flag, so the argv the Mac app
    /// spawns with it is pinned against the real `Args` in `main.rs`.
    #[test]
    fn the_five_verbs_parse_the_way_the_docs_spell_them() {
        assert!(matches!(
            parse(&["ensure"]),
            AgentCmd::Ensure { startup: false }
        ));
        assert!(matches!(
            parse(&["ensure", "--startup"]),
            AgentCmd::Ensure { startup: true }
        ));
        assert!(matches!(parse(&["status"]), AgentCmd::Status));
        assert!(matches!(
            parse(&["install", "--all"]),
            AgentCmd::Install { all: true, .. }
        ));
        assert!(
            matches!(parse(&["uninstall", "codex"]), AgentCmd::Uninstall { agent: Some(name), .. } if name == "codex")
        );
        assert!(matches!(
            parse(&["set", "claude,codex", "--local"]),
            AgentCmd::Set { spec, local: true } if spec == "claude,codex"
        ));
        assert!(matches!(
            parse(&["set", "off", "--local"]),
            AgentCmd::Set { spec, local: true } if spec == "off"
        ));
        assert!(matches!(
            parse(&["set", "claude"]),
            AgentCmd::Set { local: false, .. }
        ));
    }

    #[test]
    fn only_a_bare_agent_set_dials_the_ui() {
        assert!(dials_the_ui(&parse(&["set", "claude"])));
        assert!(!dials_the_ui(&parse(&["set", "claude", "--local"])));
        for offline in [
            vec!["ensure"],
            vec!["ensure", "--startup"],
            vec!["status"],
            vec!["install", "--all"],
            vec!["uninstall", "--all"],
        ] {
            assert!(!dials_the_ui(&parse(&offline)), "{offline:?}");
        }
    }

    /// The argument shapes that are mistakes rather than instructions.
    /// Each has to be refused before anything is written, not resolved
    /// to a guess about what the user meant.
    #[test]
    fn an_ambiguous_or_empty_target_is_refused() {
        assert_eq!(targets(Some("codex"), false), Ok(vec![Agent::Codex]));
        assert_eq!(targets(None, true), Ok(ALL_AGENTS.to_vec()));
        for (agent, all) in [
            (Some("codex"), true),
            (None, false),
            (Some("gemini"), false),
            // gx reports as grok and has no name of its own.
            (Some("gx"), false),
        ] {
            let refused = targets(agent, all).expect_err("refused");
            assert_eq!(refused.exit_code(), 2, "{agent:?} {all}: {refused:?}");
            assert_eq!(refused.code(), "usage");
        }
    }

    /// `set`'s argument, unlike `AgentHooks::parse`, refuses a list that
    /// names even one agent it does not know — see the function's own
    /// doc for why a reader's leniency is wrong for a writer.
    #[test]
    fn parse_set_spec_accepts_a_list_or_off_and_refuses_everything_else() {
        assert_eq!(
            parse_set_spec("claude,codex"),
            Ok(Mode::Allow(vec![Agent::Claude, Agent::Codex]))
        );
        assert_eq!(parse_set_spec(" OFF "), Ok(Mode::Off));
        assert_eq!(parse_set_spec("off"), Ok(Mode::Off));
        assert!(parse_set_spec("").is_err());
        assert!(parse_set_spec("banana").is_err());
        // One unknown name refuses the whole list, not just the token
        // that named nothing.
        let refused = parse_set_spec("claude,banana").unwrap_err();
        assert!(refused.contains("banana"), "{refused}");
        // `ask`/`auto` are the "unanswered" spelling, not something this
        // verb can be told to *set*.
        assert!(parse_set_spec("ask").is_err());
        assert!(parse_set_spec("auto").is_err());
    }

    fn a_row(agent: Agent, allowed: bool) -> Status {
        Status {
            agent,
            present: true,
            wired: Some(roost_agent_install::INTEGRATION_VERSION),
            entries_on_disk: true,
            up_to_date: true,
            noticed: true,
            allowed,
            files: vec!["/home/u/.codex/hooks.json".into()],
            skipped: None,
            warnings: Vec::new(),
        }
    }

    #[test]
    fn a_status_line_says_present_allowed_and_wired_separately() {
        let version = roost_agent_install::INTEGRATION_VERSION;
        assert_eq!(
            status_line(&a_row(Agent::Codex, true)),
            format!("present, allowed, wired@v{version}")
        );
        assert_eq!(
            status_line(&Status {
                entries_on_disk: false,
                wired: None,
                up_to_date: false,
                ..a_row(Agent::Grok, false)
            }),
            "present, not allowed"
        );
        // The record-vs-disk disagreement is kept word for word.
        assert_eq!(
            status_line(&Status {
                entries_on_disk: false,
                ..a_row(Agent::Claude, true)
            }),
            format!("present, allowed, record says wired@v{version}, nothing wired on disk")
        );
        assert_eq!(
            status_line(&Status {
                present: false,
                ..a_row(Agent::Cursor, true)
            }),
            "not installed"
        );
    }

    #[test]
    fn the_json_outcome_carries_every_list_even_when_empty() {
        let value = outcome_json(&Outcome::default());
        for key in [
            "wired",
            "refreshed",
            "current",
            "removed",
            "skipped",
            "warnings",
            "errors",
        ] {
            assert!(value.get(key).unwrap().is_array(), "{key}");
        }
    }
}
