//! `roostctl agent` — wire Roost's hook entries into the supported
//! agents' own config files, and take them out again.
//!
//! Four verbs over `roost-agent-install`. None of them dials a UI: they
//! read and write dotfiles, so they work with nothing running, which is
//! exactly when a user reaches for them.
//!
//! `ensure` here is the explicit reconcile — it wires what the key allows
//! **and takes out what it does not**. `--startup` is the other shape,
//! the one a UI launch runs: wire and refresh, remove nothing (plan 064
//! §3.2). The Mac app spawns that one, because a launch must not undo a
//! hand edit.
//!
//! Every verb reads the config, and reads the **same** file and parser
//! the UIs do (`roost-ui-model`), so `agent-hooks` means one thing on
//! this machine rather than one thing per surface. `install` and
//! `uninstall` are explicit instructions and *move the key* rather than
//! ignoring it: `agent install codex` while the key says `off` wires
//! codex and adds it to the list, so the next launch does not treat what
//! the user just asked for as unconsented. `status` changes nothing at
//! all.

use clap::Subcommand;
use roost_agent::Agent;
use roost_agent_install::{
    ensure, install, reconcile, status, uninstall, AgentSkip, Guard, Home, Mode, Outcome, Status,
    ALL_AGENTS,
};
use roost_ui_model::config::RoostConfig;

/// How this client identifies itself in the state record.
const BY: &str = "local";

#[derive(Subcommand, Debug)]
pub enum AgentCmd {
    /// Bring this machine in line with `agent-hooks` in `config.conf`:
    /// wire and refresh what it names, and take Roost's entries out of
    /// what it does not. Safe to run any number of times, and a run with
    /// nothing to do writes nothing.
    Ensure {
        #[arg(long, default_value_t = false)]
        json: bool,
        /// Wire and refresh what `agent-hooks` names, and remove
        /// nothing. What a UI runs at launch: a launch must never undo a
        /// hook somebody added by hand, and it must never act on a key
        /// that changed while the app was closed (plan 064 §3.2).
        #[arg(long, default_value_t = false)]
        startup: bool,
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
        AgentCmd::Ensure { json, startup } => match configured() {
            Some(mode) if *startup => report(ensure(&home, &mode, BY, guard), *json),
            Some(mode) => report(reconcile(&home, &mode, BY, guard), *json),
            None => {
                // `--json` is a machine contract — the Mac app spawns
                // exactly this and decodes stdout — so the unconfigured
                // path answers in the shape an ensure would (an empty
                // outcome) and puts its one human sentence on stderr,
                // rather than breaking the decode with prose.
                let note = "agent-hooks is not configured; nothing was wired. Choose agents \
                            in Roost (Agent Hooks… in the command palette).";
                if *json {
                    println!("{}", outcome_json(&Outcome::default()));
                    eprintln!("{note}");
                } else {
                    println!("{note}");
                }
                0
            }
        },
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
        AgentCmd::Status { json } => match status(&home, &resolved_or_nothing()) {
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

/// The resolved `agent-hooks` key — `None` when nobody has answered the
/// consent dialog yet (plan 064), which `Ensure` reports rather than
/// wiring or unwiring anything.
///
/// `pub(crate)` for doctor, which renders the same rows `status` prints
/// and must resolve the key the same way.
pub(crate) fn configured() -> Option<Mode> {
    Mode::from_config(&RoostConfig::load_default().agent_hooks)
}

/// The key for a *reader*: unanswered means nothing is allowed yet.
/// Reading is never a reason to guess at a consent nobody gave.
pub(crate) fn resolved_or_nothing() -> Mode {
    configured().unwrap_or(Mode::Allow(Vec::new()))
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
                roost_agent_install::agent_names()
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
                    "allowed": row.allowed,
                    "files": row.files.iter().map(|p| p.display().to_string()).collect::<Vec<_>>(),
                    "skipped": row.skipped.as_ref().map(ToString::to_string),
                    "warnings": row.warnings.iter().map(ToString::to_string).collect::<Vec<_>>(),
                })
            })
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

    /// `roost_ui_model::config::AGENT_NAMES` cannot depend on
    /// `roost-agent-install` (it would pull the install engine into
    /// every consumer of the config parser), so the dialog's row order
    /// and the install engine's agent inventory are two separately
    /// maintained tables. This crate is the one place both are already
    /// linked, so it is where drift between them gets caught.
    #[test]
    fn the_config_name_table_matches_the_agent_inventory() {
        let inventory: Vec<&str> = roost_agent_install::ALL_AGENTS
            .iter()
            .map(|a| a.source())
            .collect();
        assert_eq!(roost_ui_model::config::AGENT_NAMES.to_vec(), inventory);
    }

    #[test]
    fn the_four_verbs_parse_the_way_the_docs_spell_them() {
        assert!(matches!(
            parse(&["ensure"]),
            AgentCmd::Ensure {
                json: false,
                startup: false
            }
        ));
        assert!(matches!(
            parse(&["ensure", "--json"]),
            AgentCmd::Ensure { json: true, .. }
        ));
        // What the Mac app spawns at launch, spelled here so a rename
        // breaks this before it breaks a launch nobody is watching.
        assert!(matches!(
            parse(&["ensure", "--startup", "--json"]),
            AgentCmd::Ensure {
                json: true,
                startup: true
            }
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
