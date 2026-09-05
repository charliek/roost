//! Claude Code — a merge into `~/.claude/settings.json`.
//!
//! The most visible file Roost writes into, and the one the guarantees
//! were written for: it is the user's own settings, it holds `model`,
//! `permissions` and `statusLine` beside the hooks, and Claude rewrites
//! it on its own schedule. So the merge is additive, the entries are
//! identified by exact command match, and a file we cannot parse is left
//! exactly as found.
//!
//! `CLAUDE_CONFIG_DIR` relocates the directory; `settings.json` is
//! created `0600` because it can hold tokens.

use std::path::PathBuf;

use roost_agent::{Agent, CLAUDE_HOOK_EVENTS};

use crate::command::{installed_command, HOOK_TIMEOUT_SECS};
use crate::error::InstallError;
use crate::home::Home;
use crate::jsonedit::{self, Opened};
use crate::plan::{InstallPlan, Intent};
use crate::state::Prior;
use crate::write::PRIVATE_MODE;

const AGENT: Agent = Agent::Claude;

pub fn settings_path(home: &Home) -> PathBuf {
    home.agent_dir(AGENT).join("settings.json")
}

pub fn plan_install(home: &Home) -> Result<InstallPlan, InstallError> {
    let path = settings_path(home);
    let mut file = match jsonedit::open(&path, PRIVATE_MODE)? {
        Opened::Skip(reason) => return Ok(InstallPlan::skip(AGENT, Intent::Install, reason)),
        Opened::Ready(file) => file,
    };

    let entry = jsonedit::handler(&installed_command(AGENT), HOOK_TIMEOUT_SECS, None);
    let warnings =
        match jsonedit::merge_grouped(&mut file.doc, &path, AGENT, &CLAUDE_HOOK_EVENTS, &entry) {
            Ok(warnings) => warnings,
            Err(reason) => return Ok(InstallPlan::skip(AGENT, Intent::Install, reason)),
        };

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
    let path = settings_path(home);
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

    if let Err(reason) = jsonedit::remove_grouped(&mut file.doc, &path, AGENT) {
        return Ok(InstallPlan::skip(AGENT, Intent::Uninstall, reason));
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
