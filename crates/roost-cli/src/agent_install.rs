//! `roostctl agent` — wire Roost's hook entries into the supported
//! agents' own config files, and take them out again.
//!
//! Four verbs over `roost-agent-install`. None of them dials a UI: they
//! read and write dotfiles, so they work with nothing running, which is
//! exactly when a user reaches for them.
//!
//! `ensure` is what the UIs run at startup and what a host session runs
//! on connect; the other three are the manual controls the toast points
//! at.
//!
//! Only `ensure` reads the config, and it reads the **same** file and
//! the same parser the UIs do (`roost-ui-model`), so `agent-hooks = off`
//! means one thing on this machine rather than one thing per surface.
//! `install` and `uninstall` are explicit instructions and deliberately
//! ignore it — `agent install codex` while the key says `off` still
//! wires codex — and `status` changes nothing at all.

use clap::Subcommand;
use roost_agent::Agent;
use roost_agent_install::{
    agent_names, ensure, install, skip_list, status, uninstall, AgentSkip, Guard, Home, Mode,
    Outcome, Status, ALL_AGENTS,
};
use roost_ui_model::config::{AgentHooks, RoostConfig};

/// How this client identifies itself in the state record.
const BY: &str = "local";

#[derive(Subcommand, Debug)]
pub enum AgentCmd {
    /// Wire every present agent whose entries are missing or stale, per
    /// `agent-hooks` / `agent-hooks-skip` in `config.conf`. Safe to run
    /// any number of times, and a run with nothing to do writes nothing.
    /// With `agent-hooks = off` it takes Roost's entries back out.
    Ensure {
        #[arg(long, default_value_t = false)]
        json: bool,
    },
    /// Wire one agent, or all of them. Explicit wins: this works even
    /// when `agent-hooks = off` would otherwise leave the agent alone.
    Install {
        /// `claude`, `codex`, `grok`, `cursor`, or `opencode`.
        agent: Option<String>,
        #[arg(long, default_value_t = false)]
        all: bool,
    },
    /// Remove Roost's entries from one agent, or all of them. Only what
    /// Roost wrote comes out — a hook you wrote that happens to mention
    /// `$ROOST_AGENT_HOOK` stays exactly where it is.
    Uninstall {
        agent: Option<String>,
        #[arg(long, default_value_t = false)]
        all: bool,
    },
    /// Per agent: installed, wired at which integration version, and
    /// whether anything is out of date.
    Status {
        #[arg(long, default_value_t = false)]
        json: bool,
    },
}

/// Exit code, not a `Result`: a partial failure still has an outcome
/// worth printing, so the report goes to stdout and the code says
/// whether anything in it failed.
pub fn run(cmd: &AgentCmd) -> i32 {
    let home = match Home::from_env() {
        Ok(home) => home,
        Err(e) => {
            eprintln!("roostctl agent: {e}");
            return 1;
        }
    };
    let guard = Guard::from_env();

    match cmd {
        AgentCmd::Ensure { json } => {
            let (mode, skip) = configured();
            report(ensure(&home, mode, &skip, BY, guard), *json)
        }
        AgentCmd::Install { agent, all } => match targets(agent.as_deref(), *all) {
            Ok(agents) => report(install(&home, &agents, BY, guard), false),
            Err(code) => code,
        },
        AgentCmd::Uninstall { agent, all } => match targets(agent.as_deref(), *all) {
            Ok(agents) => {
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
                let code = report(outcome, false);
                if cleanup {
                    crate::legacy_claude_uninstall(guard);
                }
                code
            }
            Err(code) => code,
        },
        AgentCmd::Status { json } => match status(&home) {
            Ok(rows) => {
                print_status(&rows, *json);
                0
            }
            Err(e) => {
                eprintln!("roostctl agent status: {e}");
                1
            }
        },
    }
}

/// `agent-hooks` / `agent-hooks-skip`, as `ensure` wants them.
///
/// A skip name no agent answers to is reported and otherwise ignored:
/// refusing to run would turn one typo into "no agent is wired and
/// nothing says why", and silence would do the same without the line.
fn configured() -> (Mode, Vec<Agent>) {
    let config = RoostConfig::load_default();
    let (skip, unknown) = skip_list(config.agent_hooks_skip.iter().map(String::as_str));
    for name in unknown {
        eprintln!(
            "roostctl agent: agent-hooks-skip: no agent named {name:?} ({})",
            agent_names()
        );
    }
    let mode = match config.agent_hooks {
        AgentHooks::Auto => Mode::Auto,
        AgentHooks::Off => Mode::Off,
    };
    (mode, skip)
}

fn targets(agent: Option<&str>, all: bool) -> Result<Vec<Agent>, i32> {
    match (agent, all) {
        (Some(_), true) => {
            eprintln!("roostctl agent: pass an agent name or --all, not both");
            Err(2)
        }
        (None, false) => {
            eprintln!(
                "roostctl agent: name an agent ({}) or pass --all",
                ALL_AGENTS
                    .iter()
                    .map(|a| a.source())
                    .collect::<Vec<_>>()
                    .join(", ")
            );
            Err(2)
        }
        (None, true) => Ok(ALL_AGENTS.to_vec()),
        (Some(name), false) => match Agent::parse(name) {
            Some(agent) => Ok(vec![agent]),
            None => {
                eprintln!("roostctl agent: unknown agent: {name}");
                Err(2)
            }
        },
    }
}

fn report(outcome: Result<Outcome, roost_agent_install::InstallError>, json: bool) -> i32 {
    let outcome = match outcome {
        Ok(outcome) => outcome,
        Err(e) => {
            eprintln!("roostctl agent: {e}");
            return 1;
        }
    };

    if json {
        println!("{}", outcome_json(&outcome));
    } else {
        print_outcome(&outcome);
    }
    i32::from(!outcome.is_clean())
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

fn print_status(rows: &[Status], json: bool) {
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
                    "skipped": row.skipped.as_ref().map(ToString::to_string),
                    "warnings": row.warnings.iter().map(ToString::to_string).collect::<Vec<_>>(),
                })
            })
            .collect();
        println!("{}", serde_json::Value::Array(body));
        return;
    }

    // `wired` is the state record's claim and `entries_on_disk` is the
    // agent's own files; they can disagree, and saying which is which is
    // the difference between a status line and a guess.
    for row in rows {
        let state = match (row.present, row.entries_on_disk, row.wired, row.up_to_date) {
            (false, _, _, _) => "not installed".to_string(),
            (true, false, None, _) => "present, not wired".to_string(),
            (true, false, Some(version), _) => {
                format!("record says wired@v{version}, nothing wired on disk")
            }
            (true, true, None, _) => "wired on disk, not in the state record".to_string(),
            (true, true, Some(version), false) => format!("wired@v{version}, out of date"),
            (true, true, Some(version), true)
                if version != roost_agent_install::INTEGRATION_VERSION =>
            {
                format!("wired and current on disk, record still says v{version}")
            }
            (true, true, Some(version), true) => format!("wired@v{version}"),
        };
        println!("{:<9} {state}", row.agent.source());
        if let Some(reason) = &row.skipped {
            if row.present {
                println!("          skipped: {reason}");
            }
        }
        for warning in &row.warnings {
            println!("          warning: {warning}");
        }
    }
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

    #[test]
    fn the_four_verbs_parse_the_way_the_docs_spell_them() {
        assert!(matches!(
            parse(&["ensure"]),
            AgentCmd::Ensure { json: false }
        ));
        assert!(matches!(
            parse(&["ensure", "--json"]),
            AgentCmd::Ensure { json: true }
        ));
        assert!(matches!(
            parse(&["status"]),
            AgentCmd::Status { json: false }
        ));
        assert!(matches!(
            parse(&["install", "--all"]),
            AgentCmd::Install { all: true, .. }
        ));
        assert!(
            matches!(parse(&["uninstall", "codex"]), AgentCmd::Uninstall { agent: Some(name), .. } if name == "codex")
        );
    }

    /// The argument shapes that are mistakes rather than instructions.
    /// Each has to be refused before anything is written, not resolved
    /// to a guess about what the user meant.
    #[test]
    fn an_ambiguous_or_empty_target_is_refused() {
        assert_eq!(targets(Some("codex"), false), Ok(vec![Agent::Codex]));
        assert_eq!(targets(None, true), Ok(ALL_AGENTS.to_vec()));
        assert_eq!(targets(Some("codex"), true), Err(2));
        assert_eq!(targets(None, false), Err(2));
        assert_eq!(targets(Some("gemini"), false), Err(2));
        // gx reports as grok and has no name of its own.
        assert_eq!(targets(Some("gx"), false), Err(2));
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
