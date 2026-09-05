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
    jsonedit::merge_grouped(&mut doc, &path, AGENT, &GROK_HOOK_EVENTS, |_| entry.clone())
        .expect("a fresh object always merges");
    doc
}

/// The keys a handler object Roost wrote can carry. A file an older
/// Roost wrote may hold **fewer** — ownership has to survive a shape
/// change, or an upgrade orphans the file it is meant to refresh — but
/// never more.
const HANDLER_FIELDS: [&str; 4] = ["type", "command", "timeout", "async"];

/// This whole file is Roost's own work, **and there is at least one
/// command in it** — so replacing it, and then deleting it, loses
/// nothing of the user's.
///
/// Every part of that is load-bearing, because this answer is what
/// stands between a user's file and an overwrite followed by an
/// `unlink`:
///
/// * **At least one Roost command.** "Every command in here is ours" is
///   vacuously true of a file with no commands in it at all. A `{}`, a
///   `{"hooks":{}}` or an empty event array is somebody's half-finished
///   edit, not a Roost install.
/// * **Nothing but the shape Roost writes.** A Roost command sitting in
///   the file does not make the *rest* of it Roost's: a `matcher` on the
///   group, a note on the handler, an event name Roost never installs —
///   each is the user's work, and each is gone the moment `desired()` is
///   written over the top of it. So every event name has to be one of
///   [`GROK_HOOK_EVENTS`], every group may hold `hooks` and nothing
///   else, and every handler is limited to [`HANDLER_FIELDS`].
///
/// Answering `false` costs the user a `ForeignFile` skip and a doctor
/// line; answering `true` wrongly costs them the file.
fn is_ours(doc: &Json) -> bool {
    let Some(entries) = doc.as_object() else {
        return false;
    };
    if entries.iter().any(|(key, _)| key != "hooks") {
        return false;
    }
    let Some(events) = doc.get("hooks").and_then(Json::as_object) else {
        return false;
    };
    let mut ours = 0usize;
    for (event, groups) in events {
        if !GROK_HOOK_EVENTS.contains(&event.as_str()) {
            return false;
        }
        let Some(groups) = groups.as_array() else {
            return false;
        };
        for group in groups {
            if !only_keys(group, &["hooks"]) {
                return false;
            }
            let Some(handlers) = group.get("hooks").and_then(Json::as_array) else {
                return false;
            };
            for handler in handlers {
                if !only_keys(handler, &HANDLER_FIELDS) {
                    return false;
                }
                match handler.get("command").and_then(Json::as_str) {
                    Some(command) if is_roost_command(AGENT, command) => ours += 1,
                    _ => return false,
                }
            }
        }
    }
    ours > 0
}

/// Is `value` an object whose keys are all drawn from `allowed`?
fn only_keys(value: &Json, allowed: &[&str]) -> bool {
    value.as_object().is_some_and(|entries| {
        entries
            .iter()
            .all(|(key, _)| allowed.contains(&key.as_str()))
    })
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

    /// A file that still holds a Roost command but is no longer only
    /// Roost's work. Each of these is a user edit that a claim would
    /// overwrite on the next `ensure` and `unlink` on the next
    /// uninstall, so each one has to end the claim.
    #[test]
    fn a_roost_command_beside_a_user_edit_does_not_claim_the_file() {
        let home = Home::rooted("/home/u");

        let mut group_field = desired(&home);
        group_field
            .get_mut("hooks")
            .unwrap()
            .get_mut("Stop")
            .unwrap()
            .as_array_mut()
            .unwrap()[0]
            .insert("matcher", Json::String("Bash".into()));
        assert!(!is_ours(&group_field), "group field");

        let mut handler_field = desired(&home);
        handler_field
            .get_mut("hooks")
            .unwrap()
            .get_mut("Stop")
            .unwrap()
            .as_array_mut()
            .unwrap()[0]
            .get_mut("hooks")
            .unwrap()
            .as_array_mut()
            .unwrap()[0]
            .insert("description", Json::String("mine".into()));
        assert!(!is_ours(&handler_field), "handler field");

        let mut unknown_event = desired(&home);
        let ours = unknown_event
            .get("hooks")
            .unwrap()
            .get("Stop")
            .unwrap()
            .clone();
        unknown_event
            .get_mut("hooks")
            .unwrap()
            .insert("MyOwnEvent", ours);
        assert!(!is_ours(&unknown_event), "unknown event");
    }
}
