//! Opening, merging into, and pruning the JSON files three of the five
//! agents keep their hooks in.
//!
//! Two shapes cover all of them. Claude, grok and codex use a **grouped**
//! one — `hooks.<Event>` is a list of `{matcher?, hooks: [handler]}` —
//! and cursor a **flat** one, `hooks.<event>` straight to a list of
//! handlers. Everything else about the merge is identical, including the
//! rule that matters most: a handler is Roost's only when its `command`
//! is byte-equal to a string [`crate::command`] has produced, so
//! anything else in the file is the user's and comes out the other side
//! untouched.
//!
//! Pruning is deliberately narrow. An empty group, an empty event array
//! or an empty `hooks` object is removed **only when Roost's own removal
//! is what emptied it** — a container the user already had empty is
//! theirs and stays.

use std::path::Path;

use roost_agent::Agent;

use crate::command::{is_roost_command, looks_edited};
use crate::error::{InstallError, SkipReason, Warning};
use crate::json::{Json, Style};
use crate::plan::FileEdit;
use crate::write::{self, Image};

/// A JSON config file, open for editing.
pub struct JsonDoc {
    pub image: Image,
    /// What the file parsed to. Compared against [`Self::doc`] to decide
    /// whether anything is worth writing at all.
    original: Json,
    pub doc: Json,
    style: Style,
}

/// The result of opening a file: either something to edit, or a reason
/// to leave it alone.
pub enum Opened {
    Ready(Box<JsonDoc>),
    Skip(SkipReason),
}

/// Read `path`, treating an absent file as an empty object.
///
/// A file that does not parse, or parses to something that is not an
/// object, comes back as a [`SkipReason`]. It is never coerced and never
/// rewritten: a file Roost cannot read is a file Roost cannot put back.
pub fn open(path: &Path, create_mode: u32) -> Result<Opened, InstallError> {
    let image = write::read_image(path, create_mode)?;
    let Some(bytes) = image.bytes.as_deref() else {
        return Ok(Opened::Ready(Box::new(JsonDoc {
            image,
            original: Json::object(),
            doc: Json::object(),
            style: Style::default(),
        })));
    };

    // Decoded strictly, and skipped if it does not decode. A lossy read
    // would put U+FFFD where the user's byte was and the write below
    // would make the substitution permanent.
    let text = match std::str::from_utf8(bytes) {
        Ok(text) => text,
        Err(e) => {
            return Ok(Opened::Skip(SkipReason::Unparseable {
                path: image.path.clone(),
                detail: format!("not valid UTF-8 ({e})"),
            }))
        }
    };
    // An existing but empty file is what a half-finished editor session
    // leaves behind; treating it as `{}` is both what the agents do and
    // the only reading that lets us write something useful.
    let parsed = if text.trim().is_empty() {
        Json::object()
    } else {
        match Json::parse(bytes) {
            Ok(value) => value,
            Err(e) => {
                return Ok(Opened::Skip(SkipReason::Unparseable {
                    path: image.path.clone(),
                    detail: e.to_string(),
                }))
            }
        }
    };
    if parsed.as_object().is_none() {
        return Ok(Opened::Skip(SkipReason::UnexpectedShape {
            path: image.path.clone(),
            detail: "the document is not a JSON object".to_string(),
        }));
    }
    let style = Style::detect(text);

    Ok(Opened::Ready(Box::new(JsonDoc {
        style,
        original: parsed.clone(),
        doc: parsed,
        image,
    })))
}

impl JsonDoc {
    pub fn existed(&self) -> bool {
        self.image.exists()
    }

    /// The edit this document implies, or `None` when the parsed value
    /// is unchanged.
    ///
    /// This is the "a write happens only when the parsed value would
    /// change" rule, and it is what makes a second `ensure` plan zero
    /// edits. Comparing values rather than bytes also means a file whose
    /// layout the printer cannot reproduce exactly is left alone as long
    /// as it already says the right thing.
    pub fn finish(self) -> Option<FileEdit> {
        self.finish_after_removal(false)
    }

    /// [`Self::finish`] for an uninstall, which is the only caller that
    /// may **delete**.
    ///
    /// `may_delete` is "the state record says Roost created this file".
    /// A document Roost emptied and Roost created goes away entirely —
    /// that, not an orphan `{}`, is what the machine looked like before.
    /// One the user already had is written back empty instead: an empty
    /// file they made is still a file they made.
    pub fn finish_after_removal(self, may_delete: bool) -> Option<FileEdit> {
        if self.doc == self.original {
            return None;
        }
        let after = if may_delete && self.doc.is_empty_object() && self.image.exists() {
            None
        } else {
            Some(self.doc.render(&self.style).into_bytes())
        };
        Some(FileEdit {
            image: self.image,
            after,
        })
    }
}

/// The handler object Roost writes.
pub fn handler(command: &str, timeout_secs: u64, is_async: Option<bool>) -> Json {
    let mut entries = vec![
        ("type".to_string(), Json::String("command".to_string())),
        ("command".to_string(), Json::String(command.to_string())),
        ("timeout".to_string(), Json::Number(timeout_secs.into())),
    ];
    if let Some(value) = is_async {
        entries.push(("async".to_string(), Json::Bool(value)));
    }
    Json::Object(entries)
}

fn command_of(value: &Json) -> Option<&str> {
    value.get("command")?.as_str()
}

fn shape(path: &Path, detail: &str) -> SkipReason {
    SkipReason::UnexpectedShape {
        path: path.to_path_buf(),
        detail: detail.to_string(),
    }
}

/// The `hooks` object, created when absent.
fn hooks_object<'a>(doc: &'a mut Json, path: &Path) -> Result<&'a mut Json, SkipReason> {
    let hooks = doc
        .entry("hooks", Json::object())
        .ok_or_else(|| shape(path, "the document is not a JSON object"))?;
    if hooks.as_object().is_none() {
        return Err(shape(path, "`hooks` is present but is not an object"));
    }
    Ok(hooks)
}

/// Report every handler in `hooks` that looks like a Roost entry someone
/// has edited. Ownership is untouched by this — it only produces the
/// fact doctor renders.
fn edited_warnings(hooks: &Json, agent: Agent, path: &Path, grouped: bool) -> Vec<Warning> {
    let mut out = Vec::new();
    let Some(events) = hooks.as_object() else {
        return out;
    };
    for (event, value) in events {
        let Some(items) = value.as_array() else {
            continue;
        };
        let handlers: Vec<&Json> = if grouped {
            items
                .iter()
                .filter_map(|group| group.get("hooks")?.as_array())
                .flatten()
                .collect()
        } else {
            items.iter().collect()
        };
        for candidate in handlers {
            if command_of(candidate).is_some_and(|c| looks_edited(agent, c)) {
                out.push(Warning::ModifiedRoostEntry {
                    path: path.to_path_buf(),
                    event: event.clone(),
                });
            }
        }
    }
    out
}

/// Put one Roost group per event into a grouped `hooks` object.
pub fn merge_grouped(
    doc: &mut Json,
    path: &Path,
    agent: Agent,
    events: &[&str],
    entry: &Json,
) -> Result<Vec<Warning>, SkipReason> {
    let warnings = doc
        .get("hooks")
        .map(|hooks| edited_warnings(hooks, agent, path, true))
        .unwrap_or_default();
    let hooks = hooks_object(doc, path)?;

    for event in events {
        let groups = hooks
            .entry(event, Json::Array(Vec::new()))
            .ok_or_else(|| shape(path, "`hooks` is present but is not an object"))?;
        let Some(groups) = groups.as_array_mut() else {
            return Err(shape(path, &format!("`hooks.{event}` is not an array")));
        };

        let existing = groups.iter().enumerate().find_map(|(gi, group)| {
            let handlers = group.get("hooks")?.as_array()?;
            let hi = handlers
                .iter()
                .position(|h| command_of(h).is_some_and(|c| is_roost_command(agent, c)))?;
            Some((gi, hi))
        });

        match existing {
            // An entry of ours from an older integration version is
            // refreshed in place, so the group keeps its position and
            // anything the user put beside it (a `matcher`, a second
            // handler) survives.
            Some((gi, hi)) => {
                if let Some(slot) = groups[gi]
                    .get_mut("hooks")
                    .and_then(Json::as_array_mut)
                    .and_then(|handlers| handlers.get_mut(hi))
                {
                    *slot = entry.clone();
                }
            }
            None => groups.push(Json::Object(vec![(
                "hooks".to_string(),
                Json::Array(vec![entry.clone()]),
            )])),
        }
    }
    Ok(warnings)
}

/// Take Roost's entries back out of a grouped `hooks` object.
/// Answers whether anything was removed.
pub fn remove_grouped(doc: &mut Json, path: &Path, agent: Agent) -> Result<bool, SkipReason> {
    let Some(events) = event_names(doc, path)? else {
        return Ok(false);
    };

    let mut removed_anything = false;
    for event in events {
        let mut emptied: Vec<usize> = Vec::new();
        {
            let Some(hooks) = doc.get_mut("hooks") else {
                break;
            };
            let Some(groups) = hooks.get_mut(&event) else {
                continue;
            };
            let Some(groups) = groups.as_array_mut() else {
                return Err(shape(path, &format!("`hooks.{event}` is not an array")));
            };

            for (gi, group) in groups.iter_mut().enumerate() {
                // A group can carry the user's own keys beside `hooks` —
                // a `matcher`, a note. Roost writes groups that hold
                // nothing else, so only *those* are Roost's to remove
                // once they are empty; anything annotated stays, minus
                // our handler.
                let annotated = group
                    .as_object()
                    .is_some_and(|entries| entries.iter().any(|(key, _)| key != "hooks"));
                let Some(handlers) = group.get_mut("hooks").and_then(Json::as_array_mut) else {
                    continue;
                };
                let before = handlers.len();
                handlers.retain(|h| !command_of(h).is_some_and(|c| is_roost_command(agent, c)));
                if handlers.len() == before {
                    continue;
                }
                removed_anything = true;
                if handlers.is_empty() && !annotated {
                    emptied.push(gi);
                }
            }
            for gi in emptied.iter().rev() {
                groups.remove(*gi);
            }
            let now_empty = groups.is_empty();
            if !emptied.is_empty() && now_empty {
                hooks.remove(&event);
            }
        }
    }

    prune_empty_hooks(doc, removed_anything);
    Ok(removed_anything)
}

/// The event keys of a `hooks` object, or `None` when there is no
/// `hooks` key at all.
fn event_names(doc: &Json, path: &Path) -> Result<Option<Vec<String>>, SkipReason> {
    let Some(hooks) = doc.get("hooks") else {
        return Ok(None);
    };
    let Some(entries) = hooks.as_object() else {
        return Err(shape(path, "`hooks` is present but is not an object"));
    };
    Ok(Some(entries.iter().map(|(k, _)| k.clone()).collect()))
}

/// Drop a `hooks` object that Roost's own removal emptied. One the user
/// already had empty is theirs.
fn prune_empty_hooks(doc: &mut Json, removed_anything: bool) {
    if removed_anything && doc.get("hooks").is_some_and(Json::is_empty_object) {
        doc.remove("hooks");
    }
}

/// Put one Roost handler per event into a flat `hooks` object.
pub fn merge_flat(
    doc: &mut Json,
    path: &Path,
    agent: Agent,
    events: &[&str],
    entry: &Json,
) -> Result<Vec<Warning>, SkipReason> {
    let warnings = doc
        .get("hooks")
        .map(|hooks| edited_warnings(hooks, agent, path, false))
        .unwrap_or_default();
    let hooks = hooks_object(doc, path)?;

    for event in events {
        let handlers = hooks
            .entry(event, Json::Array(Vec::new()))
            .ok_or_else(|| shape(path, "`hooks` is present but is not an object"))?;
        let Some(handlers) = handlers.as_array_mut() else {
            return Err(shape(path, &format!("`hooks.{event}` is not an array")));
        };
        match handlers
            .iter()
            .position(|h| command_of(h).is_some_and(|c| is_roost_command(agent, c)))
        {
            Some(index) => handlers[index] = entry.clone(),
            None => handlers.push(entry.clone()),
        }
    }
    Ok(warnings)
}

/// Take Roost's entries back out of a flat `hooks` object.
pub fn remove_flat(doc: &mut Json, path: &Path, agent: Agent) -> Result<bool, SkipReason> {
    let Some(events) = event_names(doc, path)? else {
        return Ok(false);
    };

    let mut removed_anything = false;
    for event in events {
        let Some(hooks) = doc.get_mut("hooks") else {
            break;
        };
        let emptied = {
            let Some(handlers) = hooks.get_mut(&event) else {
                continue;
            };
            let Some(handlers) = handlers.as_array_mut() else {
                return Err(shape(path, &format!("`hooks.{event}` is not an array")));
            };
            let before = handlers.len();
            handlers.retain(|h| !command_of(h).is_some_and(|c| is_roost_command(agent, c)));
            if handlers.len() == before {
                continue;
            }
            removed_anything = true;
            handlers.is_empty()
        };
        if emptied {
            hooks.remove(&event);
        }
    }

    prune_empty_hooks(doc, removed_anything);
    Ok(removed_anything)
}

/// Where Roost's handler for `event` ended up: `(group index, handler
/// index)` as codex counts them.
///
/// Both are the raw positions in the file. `findings.md` says an
/// empty-command handler shifts the indices after it; the source does
/// not — `discovery.rs:487,503` take both from `.enumerate()` and only
/// then `continue` past a handler they reject, so a skipped entry costs
/// its own key and nothing else.
pub fn locate_grouped(doc: &Json, agent: Agent, event: &str) -> Option<(usize, usize)> {
    let groups = doc.get("hooks")?.get(event)?.as_array()?;
    groups.iter().enumerate().find_map(|(gi, group)| {
        let handlers = group.get("hooks")?.as_array()?;
        let hi = handlers
            .iter()
            .position(|h| command_of(h).is_some_and(|c| is_roost_command(agent, c)))?;
        Some((gi, hi))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::installed_command;

    fn parse(text: &str) -> Json {
        Json::parse(text.as_bytes()).unwrap()
    }

    fn ours() -> Json {
        handler(&installed_command(Agent::Claude), 10, None)
    }

    #[test]
    fn a_merge_appends_and_leaves_foreign_groups_where_they_are() {
        let mut doc = parse(
            r#"{"permissions":{"defaultMode":"auto"},
                "hooks":{"SessionStart":[{"matcher":"*","hooks":[{"type":"command","command":"herdr"}]}]}}"#,
        );
        let warnings = merge_grouped(
            &mut doc,
            Path::new("/x"),
            Agent::Claude,
            &["SessionStart"],
            &ours(),
        )
        .unwrap();
        assert!(warnings.is_empty());

        let groups = doc.get("hooks").unwrap().get("SessionStart").unwrap();
        let groups = groups.as_array().unwrap();
        assert_eq!(groups.len(), 2);
        // The user's group is untouched and still first.
        assert_eq!(
            groups[0].get("hooks").unwrap().as_array().unwrap()[0]
                .get("command")
                .unwrap()
                .as_str(),
            Some("herdr")
        );
        assert_eq!(groups[0].get("matcher").unwrap().as_str(), Some("*"));
        // And nothing else in the document moved.
        assert!(doc.get("permissions").is_some());
        let keys: Vec<&str> = doc
            .as_object()
            .unwrap()
            .iter()
            .map(|(k, _)| k.as_str())
            .collect();
        assert_eq!(keys, ["permissions", "hooks"]);
    }

    #[test]
    fn a_second_merge_changes_nothing() {
        let mut doc = parse(r#"{"hooks":{}}"#);
        merge_grouped(&mut doc, Path::new("/x"), Agent::Claude, &["Stop"], &ours()).unwrap();
        let once = doc.clone();
        merge_grouped(&mut doc, Path::new("/x"), Agent::Claude, &["Stop"], &ours()).unwrap();
        assert_eq!(doc, once);
    }

    /// An older version's entry is refreshed where it stands, not
    /// duplicated beside itself.
    #[test]
    fn an_older_roost_entry_is_replaced_in_place() {
        let mut doc = parse(&format!(
            r#"{{"hooks":{{"Stop":[{{"hooks":[{{"type":"command","command":{cmd}}}]}}]}}}}"#,
            cmd = serde_json::Value::String(installed_command(Agent::Claude))
        ));
        // Same command, stale timeout: still ours, so it is rewritten.
        merge_grouped(&mut doc, Path::new("/x"), Agent::Claude, &["Stop"], &ours()).unwrap();
        let groups = doc
            .get("hooks")
            .unwrap()
            .get("Stop")
            .unwrap()
            .as_array()
            .unwrap();
        assert_eq!(groups.len(), 1);
        let handlers = groups[0].get("hooks").unwrap().as_array().unwrap();
        assert_eq!(handlers.len(), 1);
        assert_eq!(
            handlers[0].get("timeout").unwrap(),
            &Json::Number(10u64.into())
        );
    }

    /// The fixture case the ownership rule exists for.
    #[test]
    fn a_foreign_hook_that_mentions_the_variable_is_never_ours() {
        let text = r#"{"hooks":{"Stop":[{"hooks":[{"type":"command","command":"echo \"$ROOST_AGENT_HOOK\" >> ~/hooks.log"}]}]}}"#;
        let mut doc = parse(text);
        let removed = remove_grouped(&mut doc, Path::new("/x"), Agent::Claude).unwrap();
        assert!(!removed);
        assert_eq!(doc, parse(text));
    }

    /// A Roost entry someone edited: reported, and left exactly where it
    /// is. Ownership is byte equality, so this is not ours to rewrite.
    #[test]
    fn an_edited_roost_entry_is_reported_and_left_alone() {
        let edited = installed_command(Agent::Claude).replace("2>/dev/null", "2>>/tmp/log");
        let text = format!(
            r#"{{"hooks":{{"Stop":[{{"hooks":[{{"type":"command","command":{cmd}}}]}}]}}}}"#,
            cmd = serde_json::Value::String(edited.clone())
        );

        let mut doc = parse(&text);
        let warnings =
            merge_grouped(&mut doc, Path::new("/x"), Agent::Claude, &["Stop"], &ours()).unwrap();
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        // Still there, beside the fresh entry Roost added.
        let groups = doc
            .get("hooks")
            .unwrap()
            .get("Stop")
            .unwrap()
            .as_array()
            .unwrap();
        assert_eq!(groups.len(), 2);
        assert_eq!(
            groups[0].get("hooks").unwrap().as_array().unwrap()[0]
                .get("command")
                .unwrap()
                .as_str(),
            Some(edited.as_str())
        );

        // And an uninstall does not take it either.
        let mut doc = parse(&text);
        assert!(!remove_grouped(&mut doc, Path::new("/x"), Agent::Claude).unwrap());
        assert_eq!(doc, parse(&text));
    }

    #[test]
    fn removing_prunes_only_what_it_emptied() {
        let mut doc = parse(&format!(
            r#"{{"model":"opus","hooks":{{
                 "Stop":[{{"hooks":[{{"type":"command","command":{cmd}}}]}}],
                 "SessionStart":[{{"hooks":[]}},{{"hooks":[{{"command":"herdr"}}]}}]
               }}}}"#,
            cmd = serde_json::Value::String(installed_command(Agent::Claude))
        ));
        assert!(remove_grouped(&mut doc, Path::new("/x"), Agent::Claude).unwrap());

        // Our event is gone; the user's already-empty group is not.
        let hooks = doc.get("hooks").unwrap();
        assert!(hooks.get("Stop").is_none());
        assert_eq!(
            hooks.get("SessionStart").unwrap().as_array().unwrap().len(),
            2
        );
        assert_eq!(doc.get("model").unwrap().as_str(), Some("opus"));
    }

    /// A group is a container the user can annotate. Taking our handler
    /// out of it must not take their `matcher` and their notes with it.
    #[test]
    fn removing_our_handler_keeps_a_group_the_user_annotated() {
        let mut doc = parse(&format!(
            r#"{{"hooks":{{"Stop":[{{"matcher":"Bash","note":"mine","hooks":[{{"command":{cmd}}}]}}]}}}}"#,
            cmd = serde_json::Value::String(installed_command(Agent::Claude))
        ));
        assert!(remove_grouped(&mut doc, Path::new("/x"), Agent::Claude).unwrap());

        let groups = doc
            .get("hooks")
            .and_then(|hooks| hooks.get("Stop"))
            .and_then(Json::as_array)
            .expect("the annotated group was removed with our handler");
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].get("matcher").unwrap().as_str(), Some("Bash"));
        assert_eq!(groups[0].get("note").unwrap().as_str(), Some("mine"));
        assert!(groups[0]
            .get("hooks")
            .unwrap()
            .as_array()
            .unwrap()
            .is_empty());
    }

    /// The flat shape has the same problem at the handler level: there
    /// is no group, so nothing is lost — pinned so it stays that way.
    #[test]
    fn the_flat_shape_keeps_a_foreign_handler_beside_ours() {
        let mut doc = parse(&format!(
            r#"{{"hooks":{{"stop":[{{"command":"orca"}},{{"command":{cmd}}}]}}}}"#,
            cmd = serde_json::Value::String(installed_command(Agent::Cursor))
        ));
        assert!(remove_flat(&mut doc, Path::new("/x"), Agent::Cursor).unwrap());
        assert_eq!(doc, parse(r#"{"hooks":{"stop":[{"command":"orca"}]}}"#));
    }

    #[test]
    fn removing_the_last_entry_removes_the_hooks_key() {
        let mut doc = parse(&format!(
            r#"{{"model":"opus","hooks":{{"Stop":[{{"hooks":[{{"command":{cmd}}}]}}]}}}}"#,
            cmd = serde_json::Value::String(installed_command(Agent::Claude))
        ));
        assert!(remove_grouped(&mut doc, Path::new("/x"), Agent::Claude).unwrap());
        assert_eq!(doc, parse(r#"{"model":"opus"}"#));
    }

    /// The same rule the TOML side follows: a file that does not decode
    /// is skipped, and its bytes are exactly where they were.
    #[test]
    fn a_file_that_is_not_utf8_is_skipped_never_coerced() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        let mut bytes = br#"{"token":"abc"#.to_vec();
        bytes.push(0xff);
        bytes.extend_from_slice(br#"def"}"#);
        std::fs::write(&path, &bytes).unwrap();

        match open(&path, crate::write::PRIVATE_MODE).unwrap() {
            Opened::Skip(SkipReason::Unparseable { detail, .. }) => {
                assert!(detail.contains("UTF-8"), "{detail}")
            }
            _ => panic!("a file that is not UTF-8 was opened for editing"),
        }
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
    }

    #[test]
    fn a_hooks_key_that_is_not_an_object_is_a_shape_skip() {
        let mut doc = parse(r#"{"hooks":[1,2,3]}"#);
        assert!(matches!(
            merge_grouped(&mut doc, Path::new("/x"), Agent::Claude, &["Stop"], &ours()),
            Err(SkipReason::UnexpectedShape { .. })
        ));
        assert!(matches!(
            remove_grouped(&mut doc, Path::new("/x"), Agent::Claude),
            Err(SkipReason::UnexpectedShape { .. })
        ));
    }

    #[test]
    fn the_flat_shape_merges_and_prunes_the_same_way() {
        let mut doc = parse(
            r#"{"hooks":{"afterAgentResponse":[{"command":"orca","timeout":10}]},"version":1}"#,
        );
        let entry = handler(&installed_command(Agent::Cursor), 10, None);
        merge_flat(
            &mut doc,
            Path::new("/x"),
            Agent::Cursor,
            &["stop", "afterAgentResponse"],
            &entry,
        )
        .unwrap();

        let after = doc.get("hooks").unwrap().get("afterAgentResponse").unwrap();
        assert_eq!(after.as_array().unwrap().len(), 2);
        assert_eq!(doc.get("version").unwrap(), &Json::Number(1u64.into()));

        assert!(remove_flat(&mut doc, Path::new("/x"), Agent::Cursor).unwrap());
        assert_eq!(
            doc,
            parse(
                r#"{"hooks":{"afterAgentResponse":[{"command":"orca","timeout":10}]},"version":1}"#
            )
        );
    }

    #[test]
    fn locate_reports_the_position_codex_will_count() {
        let mut doc = parse(r#"{"hooks":{"SessionStart":[{"hooks":[{"command":"herdr"}]}]}}"#);
        let entry = handler(&installed_command(Agent::Codex), 10, Some(false));
        merge_grouped(
            &mut doc,
            Path::new("/x"),
            Agent::Codex,
            &["SessionStart"],
            &entry,
        )
        .unwrap();
        assert_eq!(
            locate_grouped(&doc, Agent::Codex, "SessionStart"),
            Some((1, 0))
        );
        assert_eq!(locate_grouped(&doc, Agent::Codex, "Stop"), None);
    }
}
