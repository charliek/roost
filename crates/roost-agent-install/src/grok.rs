//! grok / gx — a file of Roost's own, in Claude's shape.
//!
//! grok loads Claude-format hook settings from files under
//! `$GROK_HOME/hooks/`, so Roost gets a whole file to itself
//! (`hooks/roost.json`) rather than merging into one the user edits.
//! gx shares `$GROK_HOME` and reports under the same source, so one
//! install covers both binaries by construction.
//!
//! Owning the file does not mean owning the *name*: a `roost.json` that
//! contains a command Roost never wrote is somebody else's, and is
//! skipped rather than overwritten.

use std::path::PathBuf;

use roost_agent::{Agent, GROK_HOOK_EVENTS};

use crate::command::{installed_command, is_roost_command, HOOK_TIMEOUT_SECS};
use crate::error::{InstallError, SkipReason};
use crate::home::Home;
use crate::json::{Json, Style};
use crate::jsonedit::{self, Opened};
use crate::plan::{FileEdit, InstallPlan, Intent};
use crate::write::PRIVATE_MODE;

const AGENT: Agent = Agent::Grok;

pub fn hooks_path(home: &Home) -> PathBuf {
    home.agent_dir(AGENT).join("hooks/roost.json")
}

/// The whole document, every time: this file has no foreign content to
/// preserve, so it is written rather than merged.
fn desired(home: &Home) -> Json {
    let mut doc = Json::object();
    let entry = jsonedit::handler(&installed_command(AGENT), HOOK_TIMEOUT_SECS, None);
    let path = hooks_path(home);
    jsonedit::merge_grouped(&mut doc, &path, AGENT, &GROK_HOOK_EVENTS, &entry)
        .expect("a fresh object always merges");
    doc
}

/// Everything in this file is a command Roost wrote, **and there is at
/// least one** — so replacing it loses nothing of the user's.
///
/// The second half is not pedantry. "Every command in here is ours" is
/// vacuously true of a file with no commands in it at all, and this
/// answer decides whether Roost overwrites and then *deletes* a file of
/// that name. A `{}`, a `{"hooks":{}}` or an empty event array is
/// somebody's half-finished edit, not a Roost install.
fn is_ours(doc: &Json) -> bool {
    let Some(entries) = doc.as_object() else {
        return false;
    };
    if entries.iter().any(|(key, _)| key != "hooks") {
        return false;
    }
    let Some(hooks) = doc.get("hooks") else {
        return false;
    };
    let Some(events) = hooks.as_object() else {
        return false;
    };
    let mut ours = 0usize;
    for (_, groups) in events {
        let Some(groups) = groups.as_array() else {
            return false;
        };
        for group in groups {
            let Some(handlers) = group.get("hooks").and_then(Json::as_array) else {
                return false;
            };
            for handler in handlers {
                match handler.get("command").and_then(Json::as_str) {
                    Some(command) if is_roost_command(AGENT, command) => ours += 1,
                    _ => return false,
                }
            }
        }
    }
    ours > 0
}

pub fn plan_install(home: &Home) -> Result<InstallPlan, InstallError> {
    let path = hooks_path(home);
    let file = match jsonedit::open(&path, PRIVATE_MODE)? {
        Opened::Skip(reason) => return Ok(InstallPlan::skip(AGENT, Intent::Install, reason)),
        Opened::Ready(file) => file,
    };
    if file.existed() && !is_ours(&file.doc) {
        return Ok(InstallPlan::skip(
            AGENT,
            Intent::Install,
            SkipReason::ForeignFile { path },
        ));
    }

    let want = desired(home).render(&Style::default()).into_bytes();
    let edits = if file.image.bytes.as_deref() == Some(want.as_slice()) {
        Vec::new()
    } else {
        vec![FileEdit {
            image: file.image,
            after: Some(want),
        }]
    };

    Ok(InstallPlan {
        agent: AGENT,
        intent: Intent::Install,
        edits,
        skipped: None,
        warnings: Vec::new(),
        files: vec![path],
    })
}

pub fn plan_uninstall(home: &Home) -> Result<InstallPlan, InstallError> {
    let path = hooks_path(home);
    let file = match jsonedit::open(&path, PRIVATE_MODE)? {
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
    if !is_ours(&file.doc) {
        return Ok(InstallPlan::skip(
            AGENT,
            Intent::Uninstall,
            SkipReason::ForeignFile { path },
        ));
    }

    Ok(InstallPlan {
        agent: AGENT,
        intent: Intent::Uninstall,
        edits: vec![FileEdit {
            image: file.image,
            after: None,
        }],
        skipped: None,
        warnings: Vec::new(),
        files: Vec::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_written_file_carries_every_grok_event() {
        let doc = desired(&Home::rooted("/home/u"));
        let hooks = doc.get("hooks").unwrap().as_object().unwrap();
        assert_eq!(hooks.len(), GROK_HOOK_EVENTS.len());
        for event in GROK_HOOK_EVENTS {
            assert!(doc.get("hooks").unwrap().get(event).is_some(), "{event}");
        }
    }

    /// Ownership by "every command in here is ours" is vacuously true
    /// of a file with no commands in it at all — and the answer decides
    /// whether Roost overwrites and then *deletes* someone's file.
    #[test]
    fn a_file_with_no_roost_command_in_it_is_never_ours() {
        for src in [
            r#"{}"#,
            r#"{"hooks":{}}"#,
            r#"{"hooks":{"Stop":[]}}"#,
            r#"{"hooks":{"Stop":[{"hooks":[]}]}}"#,
        ] {
            let doc = Json::parse(src.as_bytes()).unwrap();
            assert!(!is_ours(&doc), "{src} was claimed as Roost's");
        }
    }

    /// The whole point of the check: a file of that name the user made
    /// is skipped, not overwritten and not deleted.
    #[test]
    fn a_users_empty_roost_json_is_skipped_by_both_verbs() {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::rooted(dir.path());
        let path = hooks_path(&home);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"{}\n").unwrap();

        for plan in [plan_install(&home).unwrap(), plan_uninstall(&home).unwrap()] {
            assert!(plan.edits.is_empty(), "{:?}", plan.skipped);
            assert!(
                matches!(plan.skipped, Some(SkipReason::ForeignFile { .. })),
                "{:?}",
                plan.skipped
            );
        }
        assert_eq!(std::fs::read(&path).unwrap(), b"{}\n");
    }

    #[test]
    fn a_file_of_ours_is_recognised_and_a_foreign_one_is_not() {
        assert!(is_ours(&desired(&Home::rooted("/home/u"))));

        let foreign = Json::parse(
            br#"{"hooks":{"Stop":[{"hooks":[{"type":"command","command":"my own script"}]}]}}"#,
        )
        .unwrap();
        assert!(!is_ours(&foreign));

        // Ours plus something the user added at the top level: not ours
        // to overwrite.
        let mut mixed = desired(&Home::rooted("/home/u"));
        mixed.insert("note", Json::String("mine".into()));
        assert!(!is_ours(&mixed));
    }
}
