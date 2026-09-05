//! codex — two files, and the reason the rollback exists.
//!
//! `~/.codex/hooks.json` carries the handlers; `~/.codex/config.toml`
//! carries `[features] hooks = true` and, per handler, the
//! `trusted_hash` that stops codex asking the user to review a hook
//! change it can already account for. The two only make sense together:
//! handlers without their hashes is a review dialog on the next launch,
//! which is the one outcome this design exists to avoid. So `hooks.json`
//! is written first and rolled back if `config.toml` cannot follow
//! ([`crate::plan::apply`]).
//!
//! `config.toml` goes through `toml_edit` rather than a `serde`
//! round-trip: it is the user's own file, comment-bearing and
//! hand-edited, and a value round-trip drops every comment in it.
//! Outside the tables Roost touches the bytes are unchanged.
//!
//! **All handlers are synchronous.** An `async` handler is a detached
//! process, and a `PostToolUse` landing after `Stop` would flip a
//! finished tab back to working — `apply_report` has no sequence
//! numbers and the wire makes no ordering promise. The clamp still
//! matters even so, because codex *hashes* the declared timeout: Roost's
//! uniform `timeout: 10` hashes as `3` on `SessionEnd` and `Interrupt`.

use std::collections::BTreeMap;
use std::path::PathBuf;

use roost_agent::{Agent, CODEX_HOOK_EVENTS};
use toml_edit::{value, DocumentMut, Item, Table};

use crate::codex_hash::{self, Handler};
use crate::command::{owned_commands, HOOK_TIMEOUT_SECS};
use crate::error::{InstallError, SkipReason};
use crate::home::Home;
use crate::json::Json;
use crate::jsonedit::{self, Opened};
use crate::plan::{FileEdit, InstallPlan, Intent};
use crate::state::{Prior, PriorFlag};
use crate::write::{self, PRIVATE_MODE};

const AGENT: Agent = Agent::Codex;

pub fn hooks_path(home: &Home) -> PathBuf {
    home.agent_dir(AGENT).join("hooks.json")
}

pub fn config_path(home: &Home) -> PathBuf {
    home.agent_dir(AGENT).join("config.toml")
}

/// The `[hooks.state]` key prefix codex will use for our `hooks.json`.
///
/// codex builds it from its own `codex_home`, which is canonicalized
/// upstream **unless** `allow_symlinked_codex_home = true`, in which
/// case it keeps the alias the environment handed it
/// (`config/src/codex_home_symlink.rs`). The flag lives in the same
/// `config.toml` this module is already reading, so resolving the same
/// way codex will is a lookup, not a guess — and getting it wrong costs
/// a key codex never looks up, which is a review dialog on every launch.
///
/// It is the one part of this integration that is *not*
/// host-independent, which is why doctor compares expected against
/// present rather than assuming.
fn key_source(home: &Home, allow_symlinked: bool) -> String {
    let path = hooks_path(home);
    if allow_symlinked {
        return path.to_string_lossy().into_owned();
    }
    write::resolve_target(&path).to_string_lossy().into_owned()
}

/// `allow_symlinked_codex_home` as the user's `config.toml` sets it.
fn allow_symlinked(config: &DocumentMut) -> bool {
    config
        .get("allow_symlinked_codex_home")
        .and_then(Item::as_bool)
        .unwrap_or(false)
}

/// What `[features] hooks` says before Roost touches it — the pre-image
/// an uninstall needs to put the user's own switch back.
pub fn observed_features_flag(home: &Home) -> PriorFlag {
    let Ok(image) = write::read_image(&config_path(home), PRIVATE_MODE) else {
        return PriorFlag::Absent;
    };
    let Ok(config) = open_config(&image) else {
        return PriorFlag::Absent;
    };
    match config
        .get("features")
        .and_then(Item::as_table_like)
        .and_then(|features| features.get("hooks"))
        .and_then(Item::as_bool)
    {
        Some(true) => PriorFlag::True,
        Some(false) => PriorFlag::False,
        None => PriorFlag::Absent,
    }
}

/// The hashes a `trusted_hash` may carry and still be one Roost wrote.
///
/// Two sources, and the second is the one that matters. The canonical
/// set — every integration version's command in the shape this module
/// writes — covers a `hooks.json` nobody has touched. But the hash
/// `plan_install` *stores* comes off [`on_disk_identity`], which carries
/// the group's `matcher` and the handler as the file spells it. A
/// `matcher` the user added to Roost's group therefore lands in the
/// stored hash, and a set built only from the canonical shape would not
/// recognise Roost's own key — orphaning it in the user's `config.toml`
/// on every uninstall. So the identity on disk is a candidate too,
/// whenever its command is one of ours.
fn our_hashes(event: &str, on_disk: Option<&(Option<String>, Handler)>) -> Vec<String> {
    let mut hashes: Vec<String> = owned_commands(AGENT)
        .iter()
        .filter_map(|command| {
            codex_hash::trusted_hash(event, None, &Handler::roost(command, HOOK_TIMEOUT_SECS))
        })
        .collect();
    if let Some((matcher, handler)) = on_disk {
        if let Some(hash) = codex_hash::trusted_hash(event, matcher.as_deref(), handler) {
            hashes.push(hash);
        }
    }
    hashes
}

/// The Roost-owned handler each event carries in `doc` right now, as the
/// file spells it — the identity codex hashes, and therefore the
/// identity an already-stored `trusted_hash` was computed from.
///
/// Read **before** a removal, never after: `remove_grouped` takes the
/// handler out, and the matcher it was hashed with goes with it.
fn owned_on_disk(doc: &Json) -> BTreeMap<&'static str, (Option<String>, Handler)> {
    let mut found = BTreeMap::new();
    for event in CODEX_HOOK_EVENTS {
        // `locate_grouped` matches on the command, so anything it finds
        // is already one of ours.
        let Some((group, handler)) = jsonedit::locate_grouped(doc, AGENT, event) else {
            continue;
        };
        if let Some(identity) = on_disk_identity(doc, event, group, handler) {
            found.insert(event, identity);
        }
    }
    found
}

/// The identity codex will hash for the handler at
/// `hooks.<event>[group].hooks[handler]`: the group's `matcher` and the
/// handler object **as the file actually spells it**.
///
/// Reading the file rather than re-deriving the canonical entry is the
/// whole point. A `timeout` edited from 10 to 20 leaves the command
/// byte-identical, so the handler is still Roost's — and codex hashes
/// the 20. Anything that re-derived the expected hash from
/// [`crate::command::installed_command`] would compare two hashes of the
/// *same* canonical handler and report a match through the review dialog
/// codex is about to show. The same goes for a v1 command still on disk
/// under a v2 hash, and for a `matcher` the user added to our group.
fn on_disk_identity(
    doc: &Json,
    event: &str,
    group: usize,
    handler: usize,
) -> Option<(Option<String>, Handler)> {
    let group = doc.get("hooks")?.get(event)?.as_array()?.get(group)?;
    let entry = group.get("hooks")?.as_array()?.get(handler)?;
    let matcher = group
        .get("matcher")
        .and_then(Json::as_str)
        .map(str::to_string);
    Some((
        matcher,
        Handler {
            command: entry.get("command")?.as_str()?.to_string(),
            timeout_sec: number(entry.get("timeout")),
            // Absent is `false`, which is also what codex's own
            // deserialization defaults it to.
            is_async: matches!(entry.get("async"), Some(Json::Bool(true))),
            status_message: entry
                .get("statusMessage")
                .and_then(Json::as_str)
                .map(str::to_string),
            additional_context_limit: number(entry.get("additionalContextLimit")),
        },
    ))
}

/// A JSON number as the file spelled it. [`crate::json::Number`] keeps
/// the token rather than a parsed value (so a write cannot restyle it),
/// so reading one back is a parse.
fn number<T: std::str::FromStr>(value: Option<&Json>) -> Option<T> {
    match value? {
        Json::Number(n) => n.as_str().parse().ok(),
        _ => None,
    }
}

/// One codex hook event's trust comparison: what codex would compute for
/// the handler Roost has installed right now, against what
/// `config.toml`'s `[hooks.state]` currently records — doctor's
/// `agent.codex.trust` read, not something `plan`/`apply` consult (they
/// always rewrite the expected hash; this is the diagnosis of what
/// happens if nobody does).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrustEntry {
    pub event: &'static str,
    pub expected: String,
    /// `None` when `config.toml` carries no state entry at this key at
    /// all — wired in `hooks.json` but never trusted, which reads to
    /// codex exactly like a hash mismatch: a review dialog.
    pub present: Option<String>,
}

/// One entry per hook event codex has a Roost handler wired for right
/// now (empty when it has none — nothing to trust yet, which is
/// different from a handler that is trusted incorrectly).
///
/// `Err` only when `hooks.json` / `config.toml` cannot be read or
/// parsed at all — the same [`SkipReason`]-shaped findings
/// [`plan_install`] reports for the same failures.
pub fn trust_entries(home: &Home) -> Result<Vec<TrustEntry>, SkipReason> {
    let hooks_path = hooks_path(home);
    let config_path = config_path(home);

    let config_image =
        write::read_image(&config_path, PRIVATE_MODE).map_err(|e| SkipReason::Unparseable {
            path: config_path.clone(),
            detail: e.to_string(),
        })?;
    let config = open_config(&config_image)?;
    let key_source = key_source(home, allow_symlinked(&config));

    let hooks_doc = match jsonedit::open(&hooks_path, PRIVATE_MODE) {
        Ok(Opened::Ready(file)) => file.doc.clone(),
        Ok(Opened::Skip(reason)) => return Err(reason),
        Err(e) => {
            return Err(SkipReason::Unparseable {
                path: hooks_path.clone(),
                detail: e.to_string(),
            })
        }
    };

    let state = config
        .get("hooks")
        .and_then(Item::as_table_like)
        .and_then(|hooks| hooks.get("state"))
        .and_then(Item::as_table_like);

    let mut entries = Vec::new();
    for event in CODEX_HOOK_EVENTS {
        let Some((group, handler)) = jsonedit::locate_grouped(&hooks_doc, AGENT, event) else {
            continue;
        };
        let Some((matcher, on_disk)) = on_disk_identity(&hooks_doc, event, group, handler) else {
            continue;
        };
        let (Some(key), Some(expected)) = (
            codex_hash::state_key(&key_source, event, group, handler),
            codex_hash::trusted_hash(event, matcher.as_deref(), &on_disk),
        ) else {
            continue;
        };
        let present = state
            .and_then(|s| s.get(key.as_str()))
            .and_then(Item::as_table_like)
            .and_then(|t| t.get("trusted_hash"))
            .and_then(Item::as_str)
            .map(str::to_string);
        entries.push(TrustEntry {
            event,
            expected,
            present,
        });
    }
    Ok(entries)
}

/// Parse `config.toml`, treating an absent file as an empty document.
///
/// Bytes that are not UTF-8 are a skip, not a lossy decode: substituting
/// U+FFFD produces a document that parses perfectly and a write that
/// persists the substitution over whatever the user's byte was.
fn open_config(image: &write::Image) -> Result<DocumentMut, SkipReason> {
    let text = match image.text() {
        Ok(None) => return Ok(DocumentMut::new()),
        Ok(Some(text)) => text,
        Err(e) => {
            return Err(SkipReason::Unparseable {
                path: image.path.clone(),
                detail: format!("not valid UTF-8 ({e})"),
            })
        }
    };
    text.parse::<DocumentMut>()
        .map_err(|e| SkipReason::Unparseable {
            path: image.path.clone(),
            detail: e.to_string(),
        })
}

fn table_mut<'a>(
    parent: &'a mut Table,
    key: &str,
    implicit: bool,
    path: &std::path::Path,
) -> Result<&'a mut Table, SkipReason> {
    let item = parent.entry(key).or_insert_with(|| {
        let mut fresh = Table::new();
        fresh.set_implicit(implicit);
        Item::Table(fresh)
    });
    item.as_table_mut()
        .ok_or_else(|| SkipReason::UnexpectedShape {
            path: path.to_path_buf(),
            detail: format!("`{key}` is present but is not a table"),
        })
}

/// Every `[hooks.state]` key that is one of ours: same `hooks.json`, an
/// event we install, and a `trusted_hash` we computed.
///
/// The hash does not depend on the handler's position, so this finds
/// stale keys left by a group the user inserted ahead of ours as well as
/// current ones — which is what lets `ensure` rewrite them instead of
/// leaving codex asking about a hook it already trusts under a different
/// index.
fn owned_state_keys(
    state: &Table,
    key_source: &str,
    on_disk: &BTreeMap<&'static str, (Option<String>, Handler)>,
) -> Vec<String> {
    state
        .iter()
        .filter_map(|(key, item)| {
            let (path, label, _, _) = codex_hash::split_state_key(key)?;
            if path != key_source {
                return None;
            }
            let event = codex_hash::event_for_label(label)?;
            let present = item.get("trusted_hash")?.as_str()?;
            our_hashes(event, on_disk.get(event))
                .iter()
                .any(|ours| ours == present)
                .then(|| key.to_string())
        })
        .collect()
}

/// Is there any hook group left in `hooks.json` after Roost's removal?
fn has_other_hooks(doc: &Json) -> bool {
    doc.get("hooks")
        .and_then(Json::as_object)
        .is_some_and(|events| !events.is_empty())
}

pub fn plan_install(home: &Home) -> Result<InstallPlan, InstallError> {
    let hooks_path = hooks_path(home);
    let config_path = config_path(home);

    // `config.toml` is read first because it is what says how codex
    // spells the path in the trust keys.
    let config_image = write::read_image(&config_path, PRIVATE_MODE)?;
    let mut config = match open_config(&config_image) {
        Ok(config) => config,
        Err(reason) => return Ok(InstallPlan::skip(AGENT, Intent::Install, reason)),
    };
    let key_source = key_source(home, allow_symlinked(&config));

    let mut hooks_file = match jsonedit::open(&hooks_path, PRIVATE_MODE)? {
        Opened::Skip(reason) => return Ok(InstallPlan::skip(AGENT, Intent::Install, reason)),
        Opened::Ready(file) => file,
    };
    let command = crate::command::installed_command(AGENT);
    let entry = jsonedit::handler(&command, HOOK_TIMEOUT_SECS, Some(false));
    let warnings = match jsonedit::merge_grouped(
        &mut hooks_file.doc,
        &hooks_path,
        AGENT,
        &CODEX_HOOK_EVENTS,
        &entry,
    ) {
        Ok(warnings) => warnings,
        Err(reason) => return Ok(InstallPlan::skip(AGENT, Intent::Install, reason)),
    };

    // The trust keys are index-based, so they are computed from the
    // *final* document — including when the merge changed nothing but a
    // group the user inserted moved ours along. The hash is read off
    // that document too: `merge_grouped` refreshes an older entry **in
    // place**, so a `matcher` the user put on our group survives the
    // merge, and codex hashes it. Deriving the hash from the canonical
    // handler alone would write a hash for a group shape that is not the
    // one on disk — a review dialog no `ensure` could ever clear.
    let mut trust: Vec<(String, String)> = Vec::new();
    for event in CODEX_HOOK_EVENTS {
        let Some((group, handler)) = jsonedit::locate_grouped(&hooks_file.doc, AGENT, event) else {
            continue;
        };
        let Some((matcher, on_disk)) = on_disk_identity(&hooks_file.doc, event, group, handler)
        else {
            continue;
        };
        let (Some(key), Some(hash)) = (
            codex_hash::state_key(&key_source, event, group, handler),
            codex_hash::trusted_hash(event, matcher.as_deref(), &on_disk),
        ) else {
            continue;
        };
        trust.push((key, hash));
    }

    let before = config.to_string();

    let on_disk = owned_on_disk(&hooks_file.doc);
    if let Err(reason) = wire_config(&mut config, &config_path, &key_source, &trust, &on_disk) {
        return Ok(InstallPlan::skip(AGENT, Intent::Install, reason));
    }

    let mut edits: Vec<FileEdit> = hooks_file.finish().into_iter().collect();
    let after = config.to_string();
    if after != before {
        edits.push(FileEdit {
            image: config_image,
            after: Some(after.into_bytes()),
        });
    }

    Ok(InstallPlan {
        agent: AGENT,
        intent: Intent::Install,
        edits,
        skipped: None,
        warnings,
        files: vec![hooks_path, config_path],
    })
}

/// Set `features.hooks` without disturbing the key's own decoration.
///
/// `Table::insert` over an occupied key resets it, and that decoration
/// is where `toml_edit` keeps the comment the user wrote above the line.
/// Replacing the `Item` behind an existing key leaves the key — and the
/// comment — where they are.
fn set_flag(features: &mut Table, on: bool) {
    match features.get_mut("hooks") {
        Some(item) => *item = value(on),
        None => {
            features.insert("hooks", value(on));
        }
    }
}

fn wire_config(
    config: &mut DocumentMut,
    path: &std::path::Path,
    key_source: &str,
    trust: &[(String, String)],
    on_disk: &BTreeMap<&'static str, (Option<String>, Handler)>,
) -> Result<(), SkipReason> {
    // Written only when it is not already what we want. `Table::insert`
    // over an occupied key resets the key's decoration, which is where
    // `toml_edit` keeps the comment above it — so a blind write of a
    // flag that is already `true` would silently eat the user's note.
    let features = table_mut(config, "features", false, path)?;
    if features.get("hooks").and_then(Item::as_bool) != Some(true) {
        set_flag(features, true);
    }

    let hooks = table_mut(config, "hooks", true, path)?;
    let state = table_mut(hooks, "state", true, path)?;

    // Drop our stale keys first: a handler that moved index leaves one
    // behind, and codex would keep asking about the entry it no longer
    // matches.
    let wanted: Vec<&str> = trust.iter().map(|(key, _)| key.as_str()).collect();
    for stale in owned_state_keys(state, key_source, on_disk) {
        if !wanted.contains(&stale.as_str()) {
            state.remove(&stale);
        }
    }
    for (key, hash) in trust {
        let entry = table_mut(state, key.as_str(), false, path)?;
        if entry.get("trusted_hash").and_then(Item::as_str) != Some(hash.as_str()) {
            entry.insert("trusted_hash", value(hash.as_str()));
        }
    }
    Ok(())
}

pub fn plan_uninstall(home: &Home, prior: &Prior) -> Result<InstallPlan, InstallError> {
    let hooks_path = hooks_path(home);
    let config_path = config_path(home);

    let config_image = write::read_image(&config_path, PRIVATE_MODE)?;
    let mut config = match open_config(&config_image) {
        Ok(config) => config,
        Err(reason) => return Ok(InstallPlan::skip(AGENT, Intent::Uninstall, reason)),
    };
    let key_source = key_source(home, allow_symlinked(&config));

    let mut hooks_file = match jsonedit::open(&hooks_path, PRIVATE_MODE)? {
        Opened::Skip(reason) => return Ok(InstallPlan::skip(AGENT, Intent::Uninstall, reason)),
        Opened::Ready(file) => file,
    };
    // Before the removal: the matcher a user put on Roost's group is
    // part of the hash `config.toml` is carrying, and `remove_grouped`
    // takes the handler that identifies the group away with it.
    let on_disk = owned_on_disk(&hooks_file.doc);
    if hooks_file.existed() {
        if let Err(reason) = jsonedit::remove_grouped(&mut hooks_file.doc, &hooks_path, AGENT) {
            return Ok(InstallPlan::skip(AGENT, Intent::Uninstall, reason));
        }
    }
    let others_remain = has_other_hooks(&hooks_file.doc);

    let mut edits: Vec<FileEdit> = hooks_file
        .finish_after_removal(prior.created(&hooks_path))
        .into_iter()
        .collect();

    if config_image.exists() {
        let before = config.to_string();
        if let Err(reason) = unwire_config(
            &mut config,
            &config_path,
            &key_source,
            &on_disk,
            others_remain,
            prior.codex_features_hooks,
        ) {
            return Ok(InstallPlan::skip(AGENT, Intent::Uninstall, reason));
        }
        let after = config.to_string();
        if after != before {
            // A `config.toml` Roost created and has just emptied goes
            // away; one the user already had stays, empty or not —
            // deleting a file Roost never made is not an uninstall, it
            // is a loss.
            let gone = after.trim().is_empty() && prior.created(&config_path);
            let bytes = (!gone).then(|| after.into_bytes());
            edits.push(FileEdit {
                image: config_image,
                after: bytes,
            });
        }
    }

    Ok(InstallPlan {
        agent: AGENT,
        intent: Intent::Uninstall,
        edits,
        skipped: None,
        warnings: Vec::new(),
        files: Vec::new(),
    })
}

fn unwire_config(
    config: &mut DocumentMut,
    path: &std::path::Path,
    key_source: &str,
    on_disk: &BTreeMap<&'static str, (Option<String>, Handler)>,
    others_remain: bool,
    prior_flag: Option<PriorFlag>,
) -> Result<(), SkipReason> {
    let mut removed_state = false;
    let mut hooks_emptied = false;
    if config.get("hooks").is_some() {
        let hooks = table_mut(config, "hooks", true, path)?;
        if hooks.get("state").is_some() {
            let state_emptied = {
                let state = table_mut(hooks, "state", true, path)?;
                for key in owned_state_keys(state, key_source, on_disk) {
                    state.remove(&key);
                    removed_state = true;
                }
                removed_state && state.is_empty()
            };
            if state_emptied {
                hooks.remove("state");
            }
        }
        hooks_emptied = removed_state && hooks.is_empty();
    }
    if hooks_emptied {
        config.remove("hooks");
    }

    // `[features] hooks = true` is a switch, not an entry, and it may
    // well be the user's own. Two rules, both conservative:
    //
    // * it stays on while any other tool's hooks are still registered —
    //   turning it off there would break them;
    // * otherwise it goes back to exactly what the state record says it
    //   was before Roost first set it. `None` — a record lost with
    //   `~/.config/roost`, or a machine Roost never recorded — means
    //   Roost does not know it put the flag there, so the flag stays. A
    //   `hooks = true` left behind runs nothing; a `hooks = true` taken
    //   away breaks whatever the user had.
    if others_remain || config.get("features").is_none() {
        return Ok(());
    }
    let restore = match prior_flag {
        None | Some(PriorFlag::True) => return Ok(()),
        Some(prior) => prior,
    };
    let features_emptied = {
        let features = table_mut(config, "features", false, path)?;
        if features.get("hooks").is_none() {
            false
        } else if restore == PriorFlag::False {
            set_flag(features, false);
            false
        } else {
            features.remove("hooks");
            features.is_empty()
        }
    };
    if features_emptied {
        config.remove("features");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::installed_command;
    use crate::plan::{apply, Guard};

    fn home_with_codex() -> (tempfile::TempDir, Home) {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::rooted(dir.path());
        std::fs::create_dir_all(home.agent_dir(AGENT)).unwrap();
        (dir, home)
    }

    /// Through `ensure`, not `apply`, because the state record is half
    /// of what an uninstall reads: which files Roost created, and what
    /// `[features] hooks` said before it got here.
    fn wire(home: &Home) {
        crate::ensure::install(home, &[AGENT], "local", Guard::PERMITTED).unwrap();
    }

    fn unwire(home: &Home) {
        crate::ensure::uninstall(home, &[AGENT], Guard::PERMITTED).unwrap();
    }

    fn uninstall_plan(home: &Home) -> InstallPlan {
        plan_uninstall(home, &crate::state::prior(home, AGENT).unwrap()).unwrap()
    }

    #[test]
    fn a_fresh_install_writes_both_files_and_trusts_every_event() {
        let (_dir, home) = home_with_codex();
        let plan = plan_install(&home).unwrap();
        assert_eq!(plan.edits.len(), 2, "hooks.json and config.toml");
        apply(&plan, Guard::PERMITTED).unwrap();

        let hooks = std::fs::read_to_string(hooks_path(&home)).unwrap();
        let hooks = Json::parse(hooks.as_bytes()).unwrap();
        for event in CODEX_HOOK_EVENTS {
            assert!(hooks.get("hooks").unwrap().get(event).is_some(), "{event}");
        }

        let config = std::fs::read_to_string(config_path(&home)).unwrap();
        assert!(config.contains("[features]"), "{config}");
        assert!(config.contains("hooks = true"), "{config}");
        let key_source = key_source(&home, false);
        for event in CODEX_HOOK_EVENTS {
            let key = codex_hash::state_key(&key_source, event, 0, 0).unwrap();
            assert!(config.contains(&key), "missing {key} in:\n{config}");
        }
        // The hash a real codex computes, not one Roost invented: the
        // SessionEnd clamp shows up here or the dialog comes back.
        let expected = codex_hash::trusted_hash(
            "SessionEnd",
            None,
            &Handler::roost(installed_command(AGENT), HOOK_TIMEOUT_SECS),
        )
        .unwrap();
        assert!(config.contains(&expected), "{config}");

        assert!(plan_install(&home).unwrap().is_noop(), "not idempotent");
    }

    /// The comment-bearing, hand-edited file the `toml_edit` dependency
    /// was taken on for.
    #[test]
    fn an_existing_config_keeps_its_comments_and_layout() {
        let (_dir, home) = home_with_codex();
        let original = "# my codex config\nmodel = \"gpt-6\"\n\n[notice]\n# keep me\nfast = true\n\n[features]\njs_repl = false\n";
        std::fs::write(config_path(&home), original).unwrap();

        wire(&home);
        let after = std::fs::read_to_string(config_path(&home)).unwrap();

        assert!(
            after.starts_with("# my codex config\nmodel = \"gpt-6\"\n"),
            "{after}"
        );
        assert!(after.contains("# keep me"), "{after}");
        assert!(after.contains("js_repl = false"), "{after}");
        assert!(after.contains("hooks = true"), "{after}");

        // Uninstall puts every one of those bytes back.
        unwire(&home);
        assert_eq!(
            std::fs::read_to_string(config_path(&home)).unwrap(),
            original
        );
    }

    /// The flag is codex-wide. Another tool's hooks keep it on.
    #[test]
    fn the_features_flag_survives_an_uninstall_when_other_hooks_remain() {
        let (_dir, home) = home_with_codex();
        std::fs::write(
            hooks_path(&home),
            "{\n  \"hooks\": {\n    \"SessionStart\": [\n      {\n        \"hooks\": [\n          {\n            \"command\": \"herdr\"\n          }\n        ]\n      }\n    ]\n  }\n}\n",
        )
        .unwrap();
        wire(&home);
        unwire(&home);

        let config = std::fs::read_to_string(config_path(&home)).unwrap();
        assert!(config.contains("hooks = true"), "{config}");
        let hooks = std::fs::read_to_string(hooks_path(&home)).unwrap();
        assert!(hooks.contains("herdr"), "{hooks}");
        assert!(!hooks.contains("ROOST_AGENT_HOOK"), "{hooks}");
    }

    /// A group inserted ahead of ours moves our index. The stale key has
    /// to go, or codex keeps asking about a hook it already trusts.
    #[test]
    fn a_reordered_group_rewrites_the_trust_key_and_drops_the_stale_one() {
        let (_dir, home) = home_with_codex();
        apply(&plan_install(&home).unwrap(), Guard::PERMITTED).unwrap();
        let key_source = key_source(&home, false);
        let at_zero = codex_hash::state_key(&key_source, "SessionStart", 0, 0).unwrap();
        let at_one = codex_hash::state_key(&key_source, "SessionStart", 1, 0).unwrap();
        assert!(std::fs::read_to_string(config_path(&home))
            .unwrap()
            .contains(&at_zero));

        // The user adds their own SessionStart group in front of ours.
        let text = std::fs::read_to_string(hooks_path(&home)).unwrap();
        let mut doc = Json::parse(text.as_bytes()).unwrap();
        let groups = doc
            .get_mut("hooks")
            .unwrap()
            .get_mut("SessionStart")
            .unwrap()
            .as_array_mut()
            .unwrap();
        groups.insert(
            0,
            Json::parse(br#"{"hooks":[{"type":"command","command":"mine"}]}"#).unwrap(),
        );
        std::fs::write(
            hooks_path(&home),
            doc.render(&crate::json::Style::default()),
        )
        .unwrap();

        let plan = plan_install(&home).unwrap();
        assert!(!plan.is_noop(), "the moved key was not noticed");
        apply(&plan, Guard::PERMITTED).unwrap();

        let config = std::fs::read_to_string(config_path(&home)).unwrap();
        assert!(config.contains(&at_one), "{config}");
        assert!(!config.contains(&at_zero), "stale key survived:\n{config}");
    }

    /// Both files Roost created go away again, rather than leaving an
    /// empty `config.toml` and a `{}` `hooks.json` behind.
    #[test]
    fn an_uninstall_removes_files_roost_created_outright() {
        let (_dir, home) = home_with_codex();
        wire(&home);
        assert!(hooks_path(&home).exists() && config_path(&home).exists());

        unwire(&home);
        assert!(!hooks_path(&home).exists(), "an empty hooks.json survived");
        assert!(
            !config_path(&home).exists(),
            "an empty config.toml survived"
        );
        assert!(uninstall_plan(&home).is_noop());
    }

    /// codex canonicalizes `CODEX_HOME` before it builds the key, so a
    /// symlinked config directory has to resolve the same way here — a
    /// key on the link path is one codex never looks up, which is a
    /// review dialog on every launch.
    #[test]
    fn the_trust_key_carries_the_resolved_path_not_the_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("dotfiles/codex");
        std::fs::create_dir_all(&real).unwrap();
        std::os::unix::fs::symlink(&real, dir.path().join(".codex")).unwrap();

        let home = Home::rooted(dir.path());
        apply(&plan_install(&home).unwrap(), Guard::PERMITTED).unwrap();

        let config = std::fs::read_to_string(config_path(&home)).unwrap();
        let resolved = std::fs::canonicalize(&real).unwrap();
        assert!(
            config.contains(&format!(
                "{}/hooks.json:session_start:0:0",
                resolved.display()
            )),
            "{config}"
        );
    }

    /// A byte that is not UTF-8 is not something to guess at. Lossy
    /// decoding turns it into U+FFFD, which parses as perfectly good
    /// TOML — and the next write persists the substitution over the
    /// user's token.
    #[test]
    fn a_config_that_is_not_utf8_is_skipped_never_coerced() {
        let (_dir, home) = home_with_codex();
        let mut bytes = b"api_token = \"abc".to_vec();
        bytes.push(0xff);
        bytes.extend_from_slice(b"def\"\n");
        std::fs::write(config_path(&home), &bytes).unwrap();

        let plan = plan_install(&home).unwrap();
        assert!(
            matches!(plan.skipped, Some(SkipReason::Unparseable { .. })),
            "{:?}",
            plan.skipped
        );
        assert!(plan.edits.is_empty());
        assert_eq!(std::fs::read(config_path(&home)).unwrap(), bytes);

        let plan = uninstall_plan(&home);
        assert!(
            matches!(plan.skipped, Some(SkipReason::Unparseable { .. })),
            "{:?}",
            plan.skipped
        );
        assert_eq!(std::fs::read(config_path(&home)).unwrap(), bytes);
    }

    /// `[features] hooks` is the user's switch when the user set it.
    /// Roost may only put back what Roost changed.
    #[test]
    fn a_features_flag_the_user_already_had_survives_an_uninstall() {
        let (_dir, home) = home_with_codex();
        let original = "[features]\nhooks = true\n";
        std::fs::write(config_path(&home), original).unwrap();

        crate::ensure::install(&home, &[AGENT], "local", Guard::PERMITTED).unwrap();
        crate::ensure::uninstall(&home, &[AGENT], Guard::PERMITTED).unwrap();

        assert_eq!(
            std::fs::read_to_string(config_path(&home)).unwrap(),
            original
        );
    }

    /// Worse than losing it: the user turned the feature *off*, Roost
    /// turned it on, and an uninstall has to put the `false` back rather
    /// than delete the line.
    #[test]
    fn a_features_flag_the_user_set_to_false_is_restored_not_dropped() {
        let (_dir, home) = home_with_codex();
        let original = "[features]\nhooks = false\n";
        std::fs::write(config_path(&home), original).unwrap();

        crate::ensure::install(&home, &[AGENT], "local", Guard::PERMITTED).unwrap();
        assert!(std::fs::read_to_string(config_path(&home))
            .unwrap()
            .contains("hooks = true"));

        crate::ensure::uninstall(&home, &[AGENT], Guard::PERMITTED).unwrap();
        assert_eq!(
            std::fs::read_to_string(config_path(&home)).unwrap(),
            original
        );
    }

    /// codex keeps the symlinked `CODEX_HOME` alias when the user asks
    /// it to, so a key on the resolved path is one codex never looks
    /// up — a review dialog on every launch.
    #[test]
    fn allow_symlinked_codex_home_keeps_the_alias_in_the_key() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("dotfiles/codex");
        std::fs::create_dir_all(&real).unwrap();
        let link = dir.path().join(".codex");
        std::os::unix::fs::symlink(&real, &link).unwrap();

        let home = Home::rooted(dir.path());
        std::fs::write(
            config_path(&home),
            "allow_symlinked_codex_home = true\n[features]\nhooks = true\n",
        )
        .unwrap();
        apply(&plan_install(&home).unwrap(), Guard::PERMITTED).unwrap();

        let config = std::fs::read_to_string(config_path(&home)).unwrap();
        assert!(
            config.contains(&format!("{}/hooks.json:session_start:0:0", link.display())),
            "the key carries the resolved path, not the alias:\n{config}"
        );
    }

    /// A user's own trust entry for a hook Roost never wrote is not
    /// ours, whatever it is keyed on.
    #[test]
    fn a_foreign_trust_entry_is_left_alone() {
        let (_dir, home) = home_with_codex();
        let key_source = key_source(&home, false);
        let foreign = codex_hash::state_key(&key_source, "SessionStart", 4, 0).unwrap();
        std::fs::write(
            config_path(&home),
            format!("[hooks.state.\"{foreign}\"]\ntrusted_hash = \"sha256:deadbeef\"\n"),
        )
        .unwrap();

        wire(&home);
        unwire(&home);

        let config = std::fs::read_to_string(config_path(&home)).unwrap();
        assert!(config.contains("sha256:deadbeef"), "{config}");
    }

    /// Freshly wired: every trust entry's `present` matches its
    /// `expected` — the state doctor's `agent.codex.trust` reads as
    /// clean.
    #[test]
    fn trust_entries_match_right_after_a_fresh_install() {
        let (_dir, home) = home_with_codex();
        wire(&home);

        let entries = trust_entries(&home).unwrap();
        assert_eq!(entries.len(), CODEX_HOOK_EVENTS.len(), "{entries:?}");
        for entry in &entries {
            assert_eq!(
                entry.present.as_deref(),
                Some(entry.expected.as_str()),
                "{entry:?}"
            );
        }
    }

    /// A hand-edited (or stale) `trusted_hash` shows up as drift: present
    /// disagrees with expected, without touching anything on disk.
    #[test]
    fn trust_entries_reports_drift_against_a_hand_edited_hash() {
        let (_dir, home) = home_with_codex();
        wire(&home);
        let key_source = key_source(&home, false);
        let key = codex_hash::state_key(&key_source, "SessionStart", 0, 0).unwrap();

        let mut config = std::fs::read_to_string(config_path(&home)).unwrap();
        let needle = format!("[hooks.state.\"{key}\"]\ntrusted_hash = ");
        let at = config.find(&needle).unwrap() + needle.len();
        let rest_start = config[at..].find('\n').unwrap() + at;
        config.replace_range(at..rest_start, "\"sha256:deadbeef\"");
        std::fs::write(config_path(&home), &config).unwrap();

        let entries = trust_entries(&home).unwrap();
        let drifted = entries.iter().find(|e| e.event == "SessionStart").unwrap();
        assert_eq!(drifted.present.as_deref(), Some("sha256:deadbeef"));
        assert_ne!(drifted.present.as_deref(), Some(drifted.expected.as_str()));
    }

    /// Nothing wired yet: no group matches any event, so there is
    /// nothing to compare — an empty list, not an error.
    #[test]
    fn trust_entries_is_empty_before_anything_is_wired() {
        let (_dir, home) = home_with_codex();
        assert!(trust_entries(&home).unwrap().is_empty());
    }

    fn read_hooks(home: &Home) -> Json {
        let text = std::fs::read_to_string(hooks_path(home)).unwrap();
        Json::parse(text.as_bytes()).unwrap()
    }

    fn write_hooks(home: &Home, doc: &Json) {
        std::fs::write(hooks_path(home), doc.render(&crate::json::Style::default())).unwrap();
    }

    /// The one handler Roost owns for `event`, for a test to edit.
    fn our_handler<'a>(doc: &'a mut Json, event: &str) -> &'a mut Json {
        doc.get_mut("hooks")
            .unwrap()
            .get_mut(event)
            .unwrap()
            .as_array_mut()
            .unwrap()[0]
            .get_mut("hooks")
            .unwrap()
            .as_array_mut()
            .unwrap()
            .get_mut(0)
            .unwrap()
    }

    /// The regression the fabricated-input tests could not reach: doctor
    /// has to hash the handler that is **on disk**. An edited `timeout`
    /// leaves the command byte-identical — so the entry is still ours,
    /// and `locate_grouped` still finds it — while codex hashes the new
    /// timeout and opens the review dialog. Re-deriving the expected
    /// hash from the canonical command compares two hashes of the same
    /// timeout-10 handler and reports `ok` straight through that dialog.
    #[test]
    fn an_edited_timeout_reads_as_trust_drift() {
        let (_dir, home) = home_with_codex();
        wire(&home);
        assert!(trust_entries(&home)
            .unwrap()
            .iter()
            .all(|e| e.present.as_deref() == Some(e.expected.as_str())));

        let mut doc = read_hooks(&home);
        our_handler(&mut doc, "Stop").insert("timeout", Json::Number(20u64.into()));
        write_hooks(&home, &doc);

        let entries = trust_entries(&home).unwrap();
        let stop = entries.iter().find(|e| e.event == "Stop").unwrap();
        assert_ne!(
            stop.present.as_deref(),
            Some(stop.expected.as_str()),
            "an edited timeout must read as drift: {stop:?}"
        );
        // Only that event. The rest of the file is untouched, and a
        // check that painted the whole agent red would be as useless as
        // one that painted it green.
        for entry in entries.iter().filter(|e| e.event != "Stop") {
            assert_eq!(
                entry.present.as_deref(),
                Some(entry.expected.as_str()),
                "{entry:?}"
            );
        }
    }

    /// The other half of the same defect: a handler flipped to `async`,
    /// or given a `statusMessage`, is still ours by command — and codex
    /// hashes both fields.
    #[test]
    fn an_edited_async_or_status_message_reads_as_trust_drift() {
        for (key, value) in [
            ("async", Json::Bool(true)),
            ("statusMessage", Json::String("mine".into())),
        ] {
            let (_dir, home) = home_with_codex();
            wire(&home);
            let mut doc = read_hooks(&home);
            our_handler(&mut doc, "SessionStart").insert(key, value);
            write_hooks(&home, &doc);

            let entries = trust_entries(&home).unwrap();
            let entry = entries.iter().find(|e| e.event == "SessionStart").unwrap();
            assert_ne!(
                entry.present.as_deref(),
                Some(entry.expected.as_str()),
                "{key}: {entry:?}"
            );
        }
    }

    /// A v1 command still on disk under the hash a v1 install wrote is
    /// **not** drift: codex hashes what is there, matches, and asks
    /// nothing. Reporting drift here would send the user to `agent
    /// ensure` for a dialog that was never going to appear — the
    /// out-of-date wiring is `agent.codex.wired`'s to report, not this
    /// check's.
    #[test]
    fn a_v1_command_under_its_own_v1_hash_is_not_drift() {
        let (_dir, home) = home_with_codex();
        wire(&home);
        let v1 = owned_commands(AGENT).last().unwrap().clone();
        assert_ne!(v1, installed_command(AGENT));

        // Rewrite both sides the way an older Roost left them: the v1
        // command, and the hash that command hashes to.
        let mut doc = read_hooks(&home);
        for event in CODEX_HOOK_EVENTS {
            our_handler(&mut doc, event).insert("command", Json::String(v1.clone()));
        }
        write_hooks(&home, &doc);

        let key_source = key_source(&home, false);
        let mut config = std::fs::read_to_string(config_path(&home)).unwrap();
        for event in CODEX_HOOK_EVENTS {
            let key = codex_hash::state_key(&key_source, event, 0, 0).unwrap();
            let old = codex_hash::trusted_hash(
                event,
                None,
                &Handler::roost(installed_command(AGENT), HOOK_TIMEOUT_SECS),
            )
            .unwrap();
            let new =
                codex_hash::trusted_hash(event, None, &Handler::roost(&v1, HOOK_TIMEOUT_SECS))
                    .unwrap();
            config = config.replace(&old, &new);
            assert!(config.contains(&key), "{key}");
        }
        std::fs::write(config_path(&home), &config).unwrap();

        for entry in trust_entries(&home).unwrap() {
            assert_eq!(
                entry.present.as_deref(),
                Some(entry.expected.as_str()),
                "{entry:?}"
            );
        }
    }

    /// And the inverse, which is the shape the review named: a v1
    /// command left on disk while `config.toml` carries the *current*
    /// hash. codex hashes the v1 command, misses, and opens the dialog —
    /// so doctor has to call it drift.
    #[test]
    fn a_v1_command_under_a_current_hash_is_drift() {
        let (_dir, home) = home_with_codex();
        wire(&home);
        let v1 = owned_commands(AGENT).last().unwrap().clone();

        let mut doc = read_hooks(&home);
        our_handler(&mut doc, "Stop").insert("command", Json::String(v1));
        write_hooks(&home, &doc);

        let entries = trust_entries(&home).unwrap();
        let stop = entries.iter().find(|e| e.event == "Stop").unwrap();
        assert_ne!(stop.present.as_deref(), Some(stop.expected.as_str()));
    }

    /// A `matcher` the user put on our group survives `merge_grouped`
    /// (it refreshes the handler in place), and codex hashes it on every
    /// event that keeps one. So the hash the install *writes* has to
    /// come off the merged document too — otherwise `ensure` writes a
    /// hash for a group shape that is not on disk, and no number of
    /// `ensure` runs ever clears the dialog.
    #[test]
    fn a_user_matcher_on_our_group_is_hashed_by_install_and_by_doctor() {
        let (_dir, home) = home_with_codex();
        wire(&home);

        let mut doc = read_hooks(&home);
        doc.get_mut("hooks")
            .unwrap()
            .get_mut("SessionStart")
            .unwrap()
            .as_array_mut()
            .unwrap()[0]
            .insert("matcher", Json::String("*".into()));
        write_hooks(&home, &doc);

        // Before a re-ensure the stored hash is the matcher-less one.
        let entries = trust_entries(&home).unwrap();
        let entry = entries.iter().find(|e| e.event == "SessionStart").unwrap();
        assert_ne!(entry.present.as_deref(), Some(entry.expected.as_str()));

        // And `ensure` is the remedy it claims to be.
        wire(&home);
        for entry in trust_entries(&home).unwrap() {
            assert_eq!(
                entry.present.as_deref(),
                Some(entry.expected.as_str()),
                "{entry:?}"
            );
        }
        assert!(std::fs::read_to_string(hooks_path(&home))
            .unwrap()
            .contains("\"matcher\""));
    }

    /// The same `matcher`, followed all the way to an uninstall.
    ///
    /// The hash `ensure` stores comes off the merged document, so it
    /// carries the matcher. Recognising ownership by the *canonical*
    /// hash alone would fail to match Roost's own key here and leave it
    /// in the user's `config.toml` forever — a dead entry pointing at a
    /// handler that no longer exists. (`hooks.json` deliberately keeps
    /// the annotated group; that is the user's note, not our leftover.)
    #[test]
    fn a_matcher_on_our_group_does_not_orphan_the_trust_key() {
        let (_dir, home) = home_with_codex();
        wire(&home);

        let mut doc = read_hooks(&home);
        doc.get_mut("hooks")
            .unwrap()
            .get_mut("SessionStart")
            .unwrap()
            .as_array_mut()
            .unwrap()[0]
            .insert("matcher", Json::String("*".into()));
        write_hooks(&home, &doc);
        wire(&home);

        unwire(&home);
        let left = std::fs::read_to_string(config_path(&home)).unwrap_or_default();
        assert!(
            !left.contains("trusted_hash"),
            "an orphaned trust key survived the uninstall:\n{left}"
        );
    }
}
