//! Cursor — a merge into `~/.cursor/hooks.json`.
//!
//! Cursor's shape is flatter than Claude's: `hooks.<event>` goes
//! straight to a list of handlers, with no matcher group in between, and
//! the document carries a `version` beside `hooks`. The handler needs an
//! explicit `"type": "command"` — without it cursor does not fire the
//! hook at all.

use std::path::PathBuf;

use roost_agent::{Agent, CURSOR_HOOK_EVENTS};

use crate::command::{installed_command, HOOK_TIMEOUT_SECS};
use crate::error::InstallError;
use crate::home::Home;
use crate::json::Json;
use crate::jsonedit::{self, Opened};
use crate::plan::{InstallPlan, Intent};
use crate::state::Prior;
use crate::write::PRIVATE_MODE;

const AGENT: Agent = Agent::Cursor;

/// The schema version cursor writes into a file it created.
const HOOKS_VERSION: i64 = 1;

pub fn hooks_path(home: &Home) -> PathBuf {
    home.agent_dir(AGENT).join("hooks.json")
}

pub fn plan_install(home: &Home) -> Result<InstallPlan, InstallError> {
    let path = hooks_path(home);
    let mut file = match jsonedit::open(&path, PRIVATE_MODE)? {
        Opened::Skip(reason) => return Ok(InstallPlan::skip(AGENT, Intent::Install, reason)),
        Opened::Ready(file) => file,
    };

    let entry = jsonedit::handler(&installed_command(AGENT), HOOK_TIMEOUT_SECS, None);
    let warnings =
        match jsonedit::merge_flat(&mut file.doc, &path, AGENT, &CURSOR_HOOK_EVENTS, &entry) {
            Ok(warnings) => warnings,
            Err(reason) => return Ok(InstallPlan::skip(AGENT, Intent::Install, reason)),
        };
    // Ensured, not enforced: a `version` cursor itself moved on is
    // cursor's business, and overwriting it would be Roost picking a
    // fight with the tool it is integrating with.
    if file.doc.get("version").is_none() {
        file.doc
            .insert("version", Json::Number(HOOKS_VERSION.into()));
    }

    Ok(InstallPlan {
        agent: AGENT,
        intent: Intent::Install,
        edits: file.finish().into_iter().collect(),
        skipped: None,
        warnings,
        files: vec![path],
    })
}

pub fn plan_uninstall(home: &Home, prior: &Prior) -> Result<InstallPlan, InstallError> {
    let path = hooks_path(home);
    let mut file = match jsonedit::open(&path, PRIVATE_MODE)? {
        Opened::Skip(reason) => return Ok(InstallPlan::skip(AGENT, Intent::Uninstall, reason)),
        Opened::Ready(file) => file,
    };
    if !file.existed() {
        return Ok(InstallPlan {
            agent: AGENT,
            intent: Intent::Uninstall,
            edits: Vec::new(),
            skipped: None,
            warnings: Vec::new(),
            files: Vec::new(),
        });
    }

    let removed = match jsonedit::remove_flat(&mut file.doc, &path, AGENT) {
        Ok(removed) => removed,
        Err(reason) => return Ok(InstallPlan::skip(AGENT, Intent::Uninstall, reason)),
    };
    // A lone `version` is the husk of a file that only ever held Roost's
    // hooks; dropping it lets the file itself go, which is what "before
    // Roost" looked like. A file that still has anything else in it
    // keeps its version untouched.
    if removed
        && file
            .doc
            .as_object()
            .is_some_and(|entries| entries.len() == 1 && entries[0].0 == "version")
    {
        file.doc.remove("version");
    }

    Ok(InstallPlan {
        agent: AGENT,
        intent: Intent::Uninstall,
        edits: file
            .finish_after_removal(prior.created(&path))
            .into_iter()
            .collect(),
        skipped: None,
        warnings: Vec::new(),
        files: Vec::new(),
    })
}
