//! The end-to-end cases, driven against committed fixture home trees.
//!
//! Every fixture under `tests/fixtures/homes/` is copied into a tempdir
//! before anything is applied, so no test can write into the source
//! tree — and no test goes anywhere near a real dotfile, because a
//! [`Home`] is always rooted at that tempdir.
//!
//! The fixtures are the shapes the real files have: a `settings.json`
//! with a `permissions` block and herdr's hook groups already in it, a
//! comment-bearing `config.toml` carrying somebody else's `[hooks.state]`
//! entry, cursor's flat file with a foreign `afterAgentResponse` group,
//! a `settings.json` that is a symlink into a dotfile repo, and files
//! that do not parse or do not have the shape the agent documents.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use roost_agent::Agent;
use tempfile::TempDir;

use crate::codex_hash::{self, Handler};
use crate::command::{
    installed_command, is_roost_command, looks_edited, owned_commands, HOOK_TIMEOUT_SECS,
    INTEGRATION_VERSION,
};
use crate::error::SkipReason;
use crate::home::{Home, ALL_AGENTS};
use crate::json::{Json, Style};
use crate::plan::Guard;
use crate::{claude, codex, cursor, ensure, grok, opencode, state};

fn fixture_root(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/homes")
        .join(name)
}

/// Copy a fixture home into a tempdir, symlinks and all.
///
/// `fs::copy` would follow a symlink and land a regular file where the
/// link was, which is precisely the bug the symlink case exists to catch.
fn copy_tree(src: &Path, dst: &Path) {
    std::fs::create_dir_all(dst).unwrap();
    for entry in std::fs::read_dir(src).unwrap() {
        let entry = entry.unwrap();
        let kind = entry.file_type().unwrap();
        let to = dst.join(entry.file_name());
        if kind.is_symlink() {
            std::os::unix::fs::symlink(std::fs::read_link(entry.path()).unwrap(), &to).unwrap();
        } else if kind.is_dir() {
            copy_tree(&entry.path(), &to);
        } else {
            std::fs::copy(entry.path(), &to).unwrap();
        }
    }
}

fn fixture(name: &str) -> (TempDir, Home) {
    let dir = tempfile::tempdir().unwrap();
    copy_tree(&fixture_root(name), dir.path());
    let home = Home::rooted(dir.path());
    (dir, home)
}

/// Every regular file under `root`, by path relative to it. Symlinks are
/// recorded by their target so a replaced link is a visible difference.
fn snapshot(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    fn walk(root: &Path, dir: &Path, out: &mut BTreeMap<PathBuf, Vec<u8>>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            let rel = path.strip_prefix(root).unwrap().to_path_buf();
            let kind = entry.file_type().unwrap();
            if kind.is_symlink() {
                let target = std::fs::read_link(&path).unwrap();
                out.insert(rel, format!("symlink -> {}", target.display()).into_bytes());
            } else if kind.is_dir() {
                walk(root, &path, out);
            } else {
                out.insert(rel, std::fs::read(&path).unwrap());
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(root, root, &mut out);
    out
}

fn parse_file(path: &Path) -> Json {
    Json::parse(&std::fs::read(path).unwrap()).unwrap()
}

/// Every object anywhere in a JSON document that carries a string
/// `command` — i.e. every hook handler, whatever shape the agent's file
/// uses (Claude's grouped, cursor's flat, grok's whole-file).
fn handlers(value: &Json, out: &mut Vec<Json>) {
    match value {
        Json::Object(entries) => {
            if entries
                .iter()
                .any(|(key, child)| key == "command" && child.as_str().is_some())
            {
                out.push(value.clone());
            } else {
                for (_, child) in entries {
                    handlers(child, out);
                }
            }
        }
        Json::Array(items) => items.iter().for_each(|item| handlers(item, out)),
        _ => {}
    }
}

/// The `command` strings of every handler in `agent`'s JSON files,
/// split into Roost's and everybody else's.
fn commands_on_disk(home: &Home, agent: Agent) -> (Vec<String>, Vec<String>) {
    let mut ours = Vec::new();
    let mut theirs = Vec::new();
    for file in crate::owned_files(home, agent) {
        if file.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let mut found = Vec::new();
        handlers(&parse_file(&file), &mut found);
        for handler in found {
            let command = handler
                .get("command")
                .unwrap()
                .as_str()
                .unwrap()
                .to_string();
            if is_roost_command(agent, &command) {
                ours.push(command);
            } else {
                theirs.push(command);
            }
        }
    }
    (ours, theirs)
}

fn keys(value: &Json) -> Vec<String> {
    value
        .as_object()
        .map(|entries| entries.iter().map(|(k, _)| k.clone()).collect())
        .unwrap_or_default()
}

fn wire_all(home: &Home) -> ensure::Outcome {
    let outcome = ensure::ensure(home, ensure::Mode::Auto, &[], "local", Guard::PERMITTED).unwrap();
    assert!(outcome.is_clean(), "{:?}", outcome.errors);
    outcome
}

/// A guard on the fixtures themselves: the byte-restore claim below only
/// means something if the printer reproduces these files exactly, so a
/// fixture that drifts out of canonical layout fails here with a clear
/// reason rather than as a confusing diff later.
#[test]
fn the_json_fixtures_are_in_the_layout_the_printer_writes() {
    for (name, rel) in [
        ("all", ".claude/settings.json"),
        ("all", ".codex/hooks.json"),
        ("all", ".cursor/hooks.json"),
        ("all", ".grok/settings.json"),
        ("foreign", ".claude/settings.json"),
        ("symlinked", "dotfiles/claude-settings.json"),
    ] {
        let path = fixture_root(name).join(rel);
        let text = std::fs::read_to_string(&path).unwrap();
        let rendered = Json::parse(text.as_bytes())
            .unwrap()
            .render(&Style::detect(&text));
        assert_eq!(rendered, text, "{name}/{rel} is not canonically laid out");
    }
}

/// herdr's `trusted_hash`, as the `all` fixture's `config.toml` spells
/// it.
fn fixture_trusted_hash() -> String {
    std::fs::read_to_string(fixture_root("all").join(".codex/config.toml"))
        .unwrap()
        .lines()
        .find_map(|line| {
            line.trim()
                .strip_prefix("trusted_hash = ")
                .map(|value| value.trim_matches('"').to_string())
        })
        .expect("the fixture carries herdr's trust entry")
}

/// The same guard, for the one fixture value nothing at runtime can
/// check: the `all` home's `[hooks.state]` entry has to be the hash
/// codex would compute for the handler in that home's own
/// `hooks.json`.
///
/// Nothing compares them during a test — the tree is copied into a
/// tempdir, so the key's `/home/fixture` path never matches the
/// `key_source` the code derives — which is exactly why a wrong value
/// can sit here indefinitely while the fixture claims to be a machine
/// whose other tool is already trusted.
#[test]
fn the_codex_fixture_trusts_its_own_handler() {
    let root = fixture_root("all");
    let hooks = parse_file(&root.join(".codex/hooks.json"));
    let entry = hooks
        .get("hooks")
        .and_then(|h| h.get("SessionStart"))
        .and_then(Json::as_array)
        .and_then(|groups| groups.first())
        .and_then(|group| group.get("hooks"))
        .and_then(Json::as_array)
        .and_then(|handlers| handlers.first())
        .expect("the fixture has one SessionStart handler");

    let handler = crate::codex_hash::Handler {
        command: entry
            .get("command")
            .and_then(Json::as_str)
            .unwrap()
            .to_string(),
        timeout_sec: match entry.get("timeout") {
            Some(Json::Number(n)) => n.as_str().parse().ok(),
            _ => None,
        },
        is_async: matches!(entry.get("async"), Some(Json::Bool(true))),
        status_message: None,
        additional_context_limit: None,
    };
    let expected = crate::codex_hash::trusted_hash("SessionStart", None, &handler).unwrap();

    let config = std::fs::read_to_string(root.join(".codex/config.toml")).unwrap();
    assert_eq!(
        fixture_trusted_hash(),
        expected,
        "the fixture's trusted_hash is not the one codex would compute for the \
         handler in its own hooks.json (want {expected}):\n{config}"
    );
}

#[test]
fn ensure_wires_every_present_agent_and_says_which_were_new() {
    let (dir, home) = fixture("all");
    let outcome = wire_all(&home);

    let mut wired = outcome.wired.clone();
    wired.sort_by_key(|a| a.source());
    let mut expected = ALL_AGENTS.to_vec();
    expected.sort_by_key(|a| a.source());
    assert_eq!(wired, expected, "{outcome:?}");
    assert!(outcome.skipped.is_empty(), "{:?}", outcome.skipped);

    // Every agent's file now names Roost's command, and nothing else on
    // the machine got a file it did not ask for.
    for agent in ALL_AGENTS {
        let file = crate::owned_files(&home, agent)[0].clone();
        let text = std::fs::read_to_string(&file).unwrap();
        assert!(
            text.contains("ROOST_AGENT_HOOK"),
            "{}: {text}",
            file.display()
        );
    }

    // And what landed is the exact string, not something like it —
    // ownership is byte equality, so a command that merely resembles
    // this one could never be found again.
    let written = parse_file(&claude::settings_path(&home))
        .get("hooks")
        .unwrap()
        .get("Stop")
        .unwrap()
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|group| {
            group
                .get("hooks")?
                .as_array()?
                .first()?
                .get("command")?
                .as_str()
        })
        .map(str::to_string)
        .collect::<Vec<_>>();
    assert_eq!(
        written,
        vec![
            "bash '/home/fixture/.claude/hooks/herdr-agent-state.sh' stop".to_string(),
            installed_command(Agent::Claude),
        ],
        "herdr's entry moved, or ours is not the exact string"
    );
    drop(dir);
}

/// Every `command` string that lands **on disk** resolves for an agent
/// that interpolates before executing.
///
/// The constant is fenced in `command.rs`, but the constant is not what
/// grok reads — the file is, and each agent's file is composed by a
/// different writer: claude and cursor merge into the user's document,
/// codex merges a grouped one and then hashes it, and grok renders a
/// whole file it owns. A fix applied to the constant while one of those
/// writers kept composing its own variant would put the red rows
/// straight back, and only this test would notice.
///
/// opencode is the deliberate exception: its plugin is JavaScript that
/// reads `process.env` and spawns, so there is no command string to
/// check — the assertion for it is that it has none.
#[test]
fn no_agents_file_carries_a_reference_an_agent_cannot_resolve() {
    let (dir, home) = fixture("all");
    wire_all(&home);

    for agent in ALL_AGENTS.iter().filter(|a| **a != Agent::Opencode) {
        // codex's second file is `config.toml`, which carries hashes
        // rather than commands; `commands_on_disk` reads only the JSON.
        let (ours, _) = commands_on_disk(&home, *agent);
        for command in &ours {
            assert!(
                crate::command::every_reference_has_a_default(command),
                "{}: carries a bare reference: {command}",
                agent.source(),
            );
        }
        // Vacuity guard: a walk that matched nothing would pass above
        // for every agent, including one whose writer had broken.
        assert!(
            !ours.is_empty(),
            "{}: no Roost command found",
            agent.source()
        );
    }

    // opencode's exemption is checked, not assumed: the plugin spawns
    // the hook itself, so the moment its file grows a shell command it
    // belongs in the loop above instead.
    let plugin = std::fs::read_to_string(&crate::owned_files(&home, Agent::Opencode)[0]).unwrap();
    assert!(plugin.contains("process.env"), "{plugin}");
    assert!(!plugin.contains("sh -c"), "opencode grew a shell command");
    drop(dir);
}

/// The upgrade path, end to end. A machine wired by the previous
/// release looks like this right now: the previous command spelling in
/// four JSON files, the previous hashes and the uniform timeout on
/// codex's side, the previous version in the plugin header and in the
/// state record. A plain `ensure` — the one the UIs run at startup —
/// has to bring all of it to the current version, for every agent,
/// without duplicating an entry, touching a foreign one, or leaving a
/// stale trust hash behind for codex to put a dialog on. That is the
/// only way a fix like v3's ever reaches a user.
///
/// Written against "the previous version", not "v2", so the next bump
/// keeps it honest without an edit; the rewind asserts it matched
/// something in every file, so a bump that forgets to keep the old
/// spelling in `owned_commands` fails here rather than orphaning
/// entries in the field.
#[test]
fn ensure_refreshes_an_install_left_by_the_previous_version() {
    let (dir, home) = fixture("all");
    wire_all(&home);
    let previous_version = INTEGRATION_VERSION - 1;
    let escaped = |s: &str| s.replace('"', "\\\"");
    let hash = |command: &str, event: &str| {
        let timeout = codex_hash::normalize_timeout(event, Some(HOOK_TIMEOUT_SECS));
        codex_hash::trusted_hash(event, None, &Handler::roost(command, timeout)).unwrap()
    };
    let per_agent = |home: &Home| -> Vec<(Agent, Vec<String>, Vec<String>)> {
        ALL_AGENTS
            .iter()
            .filter(|a| **a != Agent::Opencode)
            .map(|a| {
                let (ours, theirs) = commands_on_disk(home, *a);
                (*a, ours, theirs)
            })
            .collect()
    };
    let fresh = per_agent(&home);

    // Rewind every file to what the previous release wrote.
    for agent in ALL_AGENTS {
        let now = installed_command(agent);
        let then = owned_commands(agent)[1].clone();
        for file in crate::owned_files(&home, agent) {
            let text = std::fs::read_to_string(&file).unwrap();
            let rewound = match (agent, file.extension().and_then(|e| e.to_str())) {
                (Agent::Opencode, _) => text.replacen(
                    &format!("integration version {INTEGRATION_VERSION}"),
                    &format!("integration version {previous_version}"),
                    1,
                ),
                (Agent::Codex, Some("toml")) => roost_agent::codex::CODEX_HOOK_EVENTS
                    .iter()
                    .fold(text.clone(), |t, event| {
                        t.replace(&hash(&now, event), &hash(&then, event))
                    }),
                (Agent::Codex, _) => {
                    // The previous release also wrote the uniform
                    // timeout — the one codex clamps and warns about.
                    let mut doc = Json::parse(text.as_bytes()).unwrap();
                    set_roost_timeouts(&mut doc, agent, HOOK_TIMEOUT_SECS);
                    doc.render(&Style::detect(&text))
                        .replace(&escaped(&now), &escaped(&then))
                }
                _ => text.replace(&escaped(&now), &escaped(&then)),
            };
            assert_ne!(
                rewound,
                text,
                "{}: the rewind matched nothing in {}",
                agent.source(),
                file.display()
            );
            std::fs::write(&file, rewound).unwrap();
        }
    }
    let (mut record, _) = state::load(&home).unwrap();
    for entry in record.values_mut() {
        entry.integration_version = previous_version;
    }
    state::save(&home, &record).unwrap();

    // The rewind is what an out-of-date machine looks like: wired, on
    // disk, not current — the row `roostctl agent status` prints as
    // "wired@vN, out of date".
    for row in ensure::status(&home).unwrap() {
        assert_eq!(row.wired, Some(previous_version), "{}", row.agent.source());
        assert!(row.entries_on_disk, "{}", row.agent.source());
        assert!(!row.up_to_date, "{}", row.agent.source());
    }

    let outcome = wire_all(&home);
    let mut refreshed: Vec<&str> = outcome.refreshed.iter().map(|a| a.source()).collect();
    refreshed.sort_unstable();
    let mut all: Vec<&str> = ALL_AGENTS.iter().map(|a| a.source()).collect();
    all.sort_unstable();
    assert_eq!(refreshed, all, "every agent is refreshed, none wired anew");
    assert!(outcome.wired.is_empty(), "{:?}", outcome.wired);

    // Every file carries the current spelling exactly as many times as
    // a fresh install writes it — no stale copy, no duplicate — and the
    // foreign entries are untouched.
    for ((agent, ours, theirs), (_, fresh_ours, fresh_theirs)) in
        per_agent(&home).iter().zip(fresh.iter())
    {
        let name = agent.source();
        assert_eq!(ours.len(), fresh_ours.len(), "{name}: entry count moved");
        for command in ours {
            assert_eq!(command, &installed_command(*agent), "{name}");
        }
        assert_eq!(theirs, fresh_theirs, "{name}: a foreign entry changed");
    }

    // codex: the current hashes are in, the previous ones are gone,
    // herdr's is still there, and the two capped events carry the cap.
    let config = std::fs::read_to_string(codex::config_path(&home)).unwrap();
    let now = installed_command(Agent::Codex);
    let then = owned_commands(Agent::Codex)[1].clone();
    for event in roost_agent::codex::CODEX_HOOK_EVENTS {
        assert!(
            config.contains(&hash(&now, event)),
            "{event}: current hash missing"
        );
        assert!(
            !config.contains(&hash(&then, event)),
            "{event}: stale hash kept"
        );
    }
    assert!(config.contains(&fixture_trusted_hash()));
    let hooks = parse_file(&codex::hooks_path(&home))
        .get("hooks")
        .cloned()
        .unwrap();
    for (event, expected) in [
        ("SessionEnd", 3u64),
        ("Interrupt", 3),
        ("Stop", HOOK_TIMEOUT_SECS),
    ] {
        let mut found = Vec::new();
        handlers(hooks.get(event).unwrap(), &mut found);
        let ours: Vec<&Json> = found
            .iter()
            .filter(|h| is_roost_command(Agent::Codex, h.get("command").unwrap().as_str().unwrap()))
            .collect();
        assert_eq!(ours.len(), 1, "{event}");
        assert_eq!(
            ours[0].get("timeout"),
            Some(&Json::Number(expected.into())),
            "{event}"
        );
    }

    // opencode's header and the record both say current.
    let plugin = std::fs::read_to_string(opencode::plugin_path(&home)).unwrap();
    assert!(
        plugin
            .lines()
            .next()
            .unwrap()
            .ends_with(&format!("integration version {INTEGRATION_VERSION}")),
        "{plugin}"
    );
    let (record, _) = state::load(&home).unwrap();
    for agent in ALL_AGENTS {
        assert_eq!(
            state::entry(&record, agent).unwrap().integration_version,
            INTEGRATION_VERSION,
            "{}",
            agent.source()
        );
    }

    // And it is done: a second ensure has nothing left to write.
    for agent in ALL_AGENTS {
        let plan = ensure::plan(agent, &home, ensure::Mode::Auto).unwrap();
        assert!(plan.is_noop(), "{} would edit again", agent.source());
    }
    drop(dir);
}

/// Set the `timeout` of every Roost handler in `value` — the rewind's
/// way of writing what the previous release wrote.
fn set_roost_timeouts(value: &mut Json, agent: Agent, timeout: u64) {
    match value {
        Json::Object(entries) => {
            let ours = entries.iter().any(|(key, child)| {
                key == "command" && child.as_str().is_some_and(|c| is_roost_command(agent, c))
            });
            if ours {
                if let Some((_, slot)) = entries.iter_mut().find(|(key, _)| key == "timeout") {
                    *slot = Json::Number(timeout.into());
                }
            } else {
                for (_, child) in entries.iter_mut() {
                    set_roost_timeouts(child, agent, timeout);
                }
            }
        }
        Json::Array(items) => items
            .iter_mut()
            .for_each(|item| set_roost_timeouts(item, agent, timeout)),
        _ => {}
    }
}

/// The toast list is the record's, not the run's.
///
/// The gap this closes: wire on a machine through one surface that
/// cannot toast (the Mac app, which has no transient status line and so
/// leaves `noticed` false on purpose), then start the UI that can. That
/// second run wires nothing — every agent is already current — so a
/// toast driven by `outcome.wired` would never fire, and `noticed` would
/// stay false forever. `unnoticed` is read off the record, so the second
/// run has something to say and the third does not.
#[test]
fn the_second_ensure_still_has_the_unannounced_agents_to_toast() {
    let (dir, home) = fixture("all");

    let first = wire_all(&home);
    let mut expected = ALL_AGENTS.to_vec();
    expected.sort_by_key(|a| a.source());
    let mut unnoticed = first.unnoticed.clone();
    unnoticed.sort_by_key(|a| a.source());
    assert_eq!(unnoticed, expected, "{first:?}");

    // Nobody said anything. A second ensure wires nothing at all — and
    // still owes the user the sentence.
    let second = ensure::ensure(&home, ensure::Mode::Auto, &[], "local", Guard::PERMITTED).unwrap();
    assert!(second.wired.is_empty(), "{second:?}");
    assert_eq!(second.current.len(), ALL_AGENTS.len(), "{second:?}");
    let mut unnoticed = second.unnoticed.clone();
    unnoticed.sort_by_key(|a| a.source());
    assert_eq!(unnoticed, expected, "{second:?}");

    // Once it has been said, it is never said again — including across
    // the refresh a version bump would drive.
    assert!(state::mark_noticed(&home, &ALL_AGENTS).unwrap());
    let third = ensure::ensure(&home, ensure::Mode::Auto, &[], "local", Guard::PERMITTED).unwrap();
    assert!(third.unnoticed.is_empty(), "{third:?}");
    drop(dir);
}

/// A skipped agent has no record entry, so it is not on the toast list
/// either — the sentence names what Roost actually wired.
#[test]
fn a_skipped_agent_is_never_on_the_toast_list() {
    let (dir, home) = fixture("all");
    let outcome = ensure::ensure(
        &home,
        ensure::Mode::Auto,
        &[Agent::Codex],
        "local",
        Guard::PERMITTED,
    )
    .unwrap();
    assert!(outcome.is_clean(), "{:?}", outcome.errors);
    assert!(!outcome.unnoticed.contains(&Agent::Codex), "{outcome:?}");
    assert!(outcome.unnoticed.contains(&Agent::Claude), "{outcome:?}");
    drop(dir);
}

/// W3's first half: on a home with all five agents present, an install
/// adds Roost's entries and changes nothing else. Comments, key order,
/// foreign hooks and unrelated settings all survive.
#[test]
fn an_install_adds_only_roosts_entries() {
    let (_dir, home) = fixture("all");
    let before_claude = parse_file(&claude::settings_path(&home));
    let before_cursor = parse_file(&cursor::hooks_path(&home));
    let before_codex_toml = std::fs::read_to_string(codex::config_path(&home)).unwrap();

    wire_all(&home);

    // Claude: the top-level key order is untouched, every unrelated
    // value is identical, and herdr's group is still first and still
    // carries its matcher.
    let after = parse_file(&claude::settings_path(&home));
    assert_eq!(keys(&after), keys(&before_claude));
    for key in [
        "permissions",
        "model",
        "statusLine",
        "enabledPlugins",
        "tui",
    ] {
        assert_eq!(after.get(key), before_claude.get(key), "{key} changed");
    }
    let herdr = &after
        .get("hooks")
        .unwrap()
        .get("SessionStart")
        .unwrap()
        .as_array()
        .unwrap()[0];
    assert_eq!(
        herdr,
        &before_claude
            .get("hooks")
            .unwrap()
            .get("SessionStart")
            .unwrap()
            .as_array()
            .unwrap()[0]
    );
    assert_eq!(herdr.get("matcher").unwrap().as_str(), Some("*"));

    // Cursor: the foreign groups keep their contents and their order,
    // and `version` is still the user's.
    let after = parse_file(&cursor::hooks_path(&home));
    assert_eq!(keys(&after), keys(&before_cursor));
    assert_eq!(after.get("version"), before_cursor.get("version"));
    let foreign = before_cursor
        .get("hooks")
        .unwrap()
        .get("beforeShellExecution")
        .unwrap();
    assert_eq!(
        after
            .get("hooks")
            .unwrap()
            .get("beforeShellExecution")
            .unwrap()
            .as_array()
            .unwrap()[0],
        foreign.as_array().unwrap()[0]
    );

    // codex: comments and unrelated tables come through `toml_edit`
    // untouched, and somebody else's trust entry is still there. The
    // hash is read out of the fixture rather than copied here: a second
    // literal is a second thing to keep in step with the fixture's own
    // `hooks.json`, and that is precisely how the fixture's value came
    // to be a hash of a command it does not contain.
    let foreign_trust = fixture_trusted_hash();
    let after = std::fs::read_to_string(codex::config_path(&home)).unwrap();
    for kept in [
        "# Hand-edited, comment-bearing, and none of Roost's business.",
        "# herdr turned this on; Roost must leave it on when it unwires.",
        "js_repl = false",
        foreign_trust.as_str(),
        "[projects.\"/private/tmp\"]",
    ] {
        assert!(after.contains(kept), "lost from config.toml: {kept}");
    }
    assert!(
        after.starts_with(before_codex_toml.split("[hooks.state").next().unwrap()),
        "the untouched prefix of config.toml moved:\n{after}"
    );
}

/// W3's second half. The claim this proves is narrower than "every
/// foreign byte comes back", because that is not what the crate
/// promises: these fixtures are **already in the printer's layout** —
/// `the_json_fixtures_are_in_the_layout_the_printer_writes` is the guard
/// that keeps them that way — and for such a file the semantic
/// guarantee happens to be a byte guarantee. A file that is *not* in
/// that layout is covered by
/// `a_non_canonical_file_is_restored_semantically_and_keeps_its_line_endings`,
/// which is honest about the reformatting an install does once and an
/// uninstall cannot undo.
#[test]
fn an_uninstall_puts_a_canonically_laid_out_file_back_byte_for_byte() {
    let (dir, home) = fixture("all");
    let before = snapshot(dir.path());

    wire_all(&home);
    assert_ne!(snapshot(dir.path()), before, "the install changed nothing");

    let outcome = ensure::uninstall(&home, &ALL_AGENTS, Guard::PERMITTED).unwrap();
    assert!(outcome.is_clean(), "{:?}", outcome.errors);

    let after = snapshot(dir.path());
    // Roost's own record and lock are new files of Roost's, not the
    // user's; everything else must be exactly as it was.
    let restored: BTreeMap<_, _> = after
        .into_iter()
        .filter(|(path, _)| !path.starts_with(".config/roost"))
        .collect();
    for (path, bytes) in &before {
        assert_eq!(
            restored
                .get(path)
                .map(|bytes| String::from_utf8_lossy(bytes)),
            Some(String::from_utf8_lossy(bytes)),
            "{} was not restored",
            path.display()
        );
    }
    let extra: Vec<_> = restored
        .keys()
        .filter(|p| !before.contains_key(*p))
        .collect();
    assert!(extra.is_empty(), "left behind: {extra:?}");
}

/// Only a file Roost *created* may be removed. One the user already had
/// — even an empty one, even a `{}` — is theirs, and an uninstall that
/// deletes it has taken something Roost never gave.
#[test]
fn an_uninstall_never_deletes_a_file_that_was_already_there() {
    for existing in ["", "{}", "{\n}\n", r#"{"hooks":{}}"#] {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::rooted(dir.path());
        std::fs::create_dir_all(home.agent_dir(Agent::Cursor)).unwrap();
        let path = cursor::hooks_path(&home);
        std::fs::write(&path, existing).unwrap();

        ensure::install(&home, &[Agent::Cursor], "local", Guard::PERMITTED).unwrap();
        ensure::uninstall(&home, &[Agent::Cursor], Guard::PERMITTED).unwrap();

        assert!(
            path.exists(),
            "a file the user already had ({existing:?}) was deleted"
        );
    }
}

/// `off` cleans "every file the state record names", which is what makes
/// a record pointing at a `CODEX_HOME` the user has since moved away
/// from still get cleaned instead of orphaned.
#[test]
fn off_cleans_a_recorded_file_at_a_config_dir_that_has_since_moved() {
    let dir = tempfile::tempdir().unwrap();
    let old = dir.path().join("old-codex");
    std::fs::create_dir_all(&old).unwrap();
    let old_env = old.to_string_lossy().into_owned();
    let then = Home::resolve(dir.path(), |key| {
        (key == "CODEX_HOME").then(|| old_env.clone())
    });
    ensure::install(&then, &[Agent::Codex], "local", Guard::PERMITTED).unwrap();
    let stale = codex::hooks_path(&then);
    assert!(stale.exists());

    // The user moves `CODEX_HOME` back to the default and Roost is told
    // to unwire. The record still names the old file.
    let now = Home::rooted(dir.path());
    std::fs::create_dir_all(now.agent_dir(Agent::Codex)).unwrap();
    let outcome = ensure::ensure(&now, ensure::Mode::Off, &[], "local", Guard::PERMITTED).unwrap();
    assert!(outcome.is_clean(), "{:?}", outcome.errors);

    assert!(
        !stale.exists(),
        "the file the record named was orphaned: {}",
        stale.display()
    );
    assert!(state::load(&now).unwrap().0.is_empty());
}

/// A file that is *not* already in the printer's layout still comes back
/// with every value and every key order the user had. What it does not
/// come back with is its original bytes, and that is the guarantee the
/// crate actually makes.
#[test]
fn a_non_canonical_file_is_restored_semantically_and_keeps_its_line_endings() {
    let dir = tempfile::tempdir().unwrap();
    let home = Home::rooted(dir.path());
    std::fs::create_dir_all(home.agent_dir(Agent::Claude)).unwrap();
    let path = claude::settings_path(&home);
    let original = "{\r\n\t\"model\":\"opus\",\r\n\t\"permissions\":{\"defaultMode\":\"a\\u0063cept\"},\r\n\t\"n\":18446744073709551617\r\n}\r\n";
    std::fs::write(&path, original).unwrap();
    let before = parse_file(&path);

    ensure::install(&home, &[Agent::Claude], "local", Guard::PERMITTED).unwrap();
    ensure::uninstall(&home, &[Agent::Claude], Guard::PERMITTED).unwrap();

    let after_text = std::fs::read_to_string(&path).unwrap();
    let after = Json::parse(after_text.as_bytes()).unwrap();
    assert_eq!(keys(&after), keys(&before));
    assert_eq!(after, before, "a value changed across the round trip");
    assert!(
        after_text.contains("\r\n") && !after_text.contains("\n\r"),
        "the file's line endings were rewritten:\n{after_text:?}"
    );
    assert!(
        after_text.contains("18446744073709551617"),
        "the number the user wrote was re-derived from an f64:\n{after_text}"
    );
}

/// The idempotency assertion the plan pins: a second run *plans* nothing.
/// mtimes are a secondary check at best — this is the one that means
/// "Roost will not rewrite your files on every launch".
#[test]
fn a_second_ensure_plans_zero_edits() {
    let (_dir, home) = fixture("all");
    wire_all(&home);

    for agent in ALL_AGENTS {
        let plan = ensure::plan(agent, &home, ensure::Mode::Auto).unwrap();
        assert!(
            plan.is_noop(),
            "{} would edit {:?} again",
            agent.source(),
            plan.edits
                .iter()
                .map(|e| e.path().to_path_buf())
                .collect::<Vec<_>>()
        );
    }

    let second = ensure::ensure(&home, ensure::Mode::Auto, &[], "local", Guard::PERMITTED).unwrap();
    assert!(second.wired.is_empty(), "{:?}", second.wired);
    assert!(second.refreshed.is_empty(), "{:?}", second.refreshed);
    assert_eq!(second.current.len(), ALL_AGENTS.len());
    assert!(!second.wrote, "a no-op ensure wrote something");
}

/// The dotfile-manager case. Replacing the link with a regular file
/// forks the user's tracked tree and nothing ever tells them.
#[test]
fn a_symlinked_settings_file_is_written_through() {
    let (dir, home) = fixture("symlinked");
    let link = claude::settings_path(&home);
    let real = dir.path().join("dotfiles/claude-settings.json");
    assert!(link.symlink_metadata().unwrap().file_type().is_symlink());

    let outcome = ensure::install(&home, &[Agent::Claude], "local", Guard::PERMITTED).unwrap();
    assert!(outcome.is_clean(), "{:?}", outcome.errors);

    assert!(
        link.symlink_metadata().unwrap().file_type().is_symlink(),
        "the symlink was replaced by a regular file"
    );
    assert_eq!(
        parse_file(&real)
            .get("hooks")
            .and_then(|hooks| hooks.get("Stop"))
            .and_then(Json::as_array)
            .map(|groups| groups.len()),
        Some(1),
        "the write did not reach the link's target"
    );

    // And back out through the same link.
    let outcome = ensure::uninstall(&home, &[Agent::Claude], Guard::PERMITTED).unwrap();
    assert!(outcome.is_clean(), "{:?}", outcome.errors);
    assert!(link.symlink_metadata().unwrap().file_type().is_symlink());
    assert_eq!(
        std::fs::read_to_string(&real).unwrap(),
        std::fs::read_to_string(fixture_root("symlinked").join("dotfiles/claude-settings.json"))
            .unwrap()
    );
}

/// A file Roost cannot make sense of is left exactly as found — never
/// coerced into the shape Roost wanted, never rewritten.
#[test]
fn a_malformed_file_is_skipped_with_a_reason() {
    let (dir, home) = fixture("malformed");
    let before = snapshot(dir.path());

    let outcome =
        ensure::ensure(&home, ensure::Mode::Auto, &[], "local", Guard::PERMITTED).unwrap();
    assert!(outcome.is_clean(), "{:?}", outcome.errors);

    let reasons: BTreeMap<&str, &SkipReason> = outcome
        .skipped
        .iter()
        .map(|skip| (skip.agent.source(), &skip.reason))
        .collect();
    assert!(
        matches!(
            reasons.get("claude"),
            Some(SkipReason::UnexpectedShape { .. })
        ),
        "{reasons:?}"
    );
    assert!(
        matches!(reasons.get("cursor"), Some(SkipReason::Unparseable { .. })),
        "{reasons:?}"
    );

    let after: BTreeMap<_, _> = snapshot(dir.path())
        .into_iter()
        .filter(|(path, _)| !path.starts_with(".config/roost"))
        .collect();
    assert_eq!(after, before);

    // An uninstall is just as careful.
    ensure::uninstall(&home, &ALL_AGENTS, Guard::PERMITTED).unwrap();
    let after: BTreeMap<_, _> = snapshot(dir.path())
        .into_iter()
        .filter(|(path, _)| !path.starts_with(".config/roost"))
        .collect();
    assert_eq!(after, before);
}

/// Ownership is exact match. A user's own hook that mentions
/// `$ROOST_AGENT_HOOK` is theirs; so is a Roost entry they have edited,
/// which is reported instead of rewritten.
#[test]
fn a_foreign_entry_and_an_edited_one_both_survive() {
    let (dir, home) = fixture("foreign");
    let before = parse_file(&claude::settings_path(&home));

    // The fixture says what it means to: one hook that merely mentions
    // the variable, and one that is Roost's own string with a byte
    // changed.
    let commands: Vec<String> = before
        .get("hooks")
        .unwrap()
        .as_object()
        .unwrap()
        .iter()
        .flat_map(|(_, groups)| groups.as_array().unwrap().clone())
        .flat_map(|group| group.get("hooks").unwrap().as_array().unwrap().clone())
        .map(|h| h.get("command").unwrap().as_str().unwrap().to_string())
        .collect();
    assert_eq!(commands.len(), 2);
    assert!(commands.iter().all(|c| !is_roost_command(Agent::Claude, c)));
    assert_eq!(
        commands
            .iter()
            .filter(|c| looks_edited(Agent::Claude, c))
            .count(),
        1
    );

    let outcome = ensure::install(&home, &[Agent::Claude], "local", Guard::PERMITTED).unwrap();
    assert!(outcome.is_clean(), "{:?}", outcome.errors);
    assert_eq!(outcome.warnings.len(), 1, "{:?}", outcome.warnings);

    let after = parse_file(&claude::settings_path(&home));
    for (event, index) in [("Stop", 0usize), ("SessionStart", 0)] {
        let group = &after
            .get("hooks")
            .unwrap()
            .get(event)
            .unwrap()
            .as_array()
            .unwrap()[index];
        assert_eq!(
            group,
            &before
                .get("hooks")
                .unwrap()
                .get(event)
                .unwrap()
                .as_array()
                .unwrap()[index],
            "{event} lost the user's entry"
        );
    }

    // And an uninstall takes only what Roost wrote.
    ensure::uninstall(&home, &[Agent::Claude], Guard::PERMITTED).unwrap();
    assert_eq!(
        std::fs::read_to_string(claude::settings_path(&home)).unwrap(),
        std::fs::read_to_string(fixture_root("foreign").join(".claude/settings.json")).unwrap()
    );
    drop(dir);
}

/// `agent install <name>` is an explicit instruction. `agent-hooks =
/// off` is a default. Explicit wins.
#[test]
fn an_explicit_install_wires_even_while_the_mode_is_off() {
    let (_dir, home) = fixture("all");
    let outcome = ensure::install(&home, &[Agent::Codex], "local", Guard::PERMITTED).unwrap();
    assert_eq!(outcome.wired, vec![Agent::Codex]);
    assert!(std::fs::read_to_string(codex::hooks_path(&home))
        .unwrap()
        .contains("agent-hook codex"));

    // `off` is still `off` for everything the user did not ask for —
    // and it does take the explicit one back out, which is what makes
    // `off` mean off.
    let outcome = ensure::ensure(&home, ensure::Mode::Off, &[], "local", Guard::PERMITTED).unwrap();
    assert_eq!(outcome.removed, vec![Agent::Codex]);
    assert!(!std::fs::read_to_string(codex::hooks_path(&home))
        .unwrap()
        .contains("agent-hook codex"));
}

/// `off` on a machine Roost never wired must be inert — no record, no
/// touched file, nothing.
#[test]
fn off_with_nothing_to_remove_writes_nothing() {
    let (dir, home) = fixture("all");
    let before = snapshot(dir.path());

    let outcome = ensure::ensure(&home, ensure::Mode::Off, &[], "local", Guard::PERMITTED).unwrap();
    assert!(outcome.is_clean(), "{:?}", outcome.errors);
    assert!(outcome.removed.is_empty());
    assert!(!outcome.wrote, "off wrote something with nothing to remove");
    // Roost's own lock file is the one thing that appears — taking the
    // lock is how "nothing happened" is established in the first place.
    let after: BTreeMap<_, _> = snapshot(dir.path())
        .into_iter()
        .filter(|(path, _)| !path.starts_with(".config/roost"))
        .collect();
    assert_eq!(after, before);
    assert!(!home.record_path().exists());
}

/// `off` also cleans an agent the record names but the machine no longer
/// has — the record is trusted *and* the agents are rescanned, because
/// either alone leaves a case behind.
#[test]
fn off_cleans_an_agent_the_record_remembers() {
    let (_dir, home) = fixture("all");
    wire_all(&home);
    assert!(state::entry(&state::load(&home).unwrap().0, Agent::Grok).is_some());

    let outcome = ensure::ensure(&home, ensure::Mode::Off, &[], "local", Guard::PERMITTED).unwrap();
    assert!(outcome.is_clean(), "{:?}", outcome.errors);
    assert!(!grok::hooks_path(&home).exists());
    assert!(!opencode::plugin_path(&home).exists());
    assert!(
        state::load(&home).unwrap().0.is_empty(),
        "the record survived off"
    );
}

#[test]
fn the_skip_list_leaves_an_agent_alone_and_says_so() {
    let (_dir, home) = fixture("all");
    let outcome = ensure::ensure(
        &home,
        ensure::Mode::Auto,
        &[Agent::Cursor],
        "local",
        Guard::PERMITTED,
    )
    .unwrap();
    assert!(!outcome.wired.contains(&Agent::Cursor));
    assert!(outcome
        .skipped
        .iter()
        .any(|s| s.agent == Agent::Cursor && matches!(s.reason, SkipReason::SkipList)));
    assert!(!std::fs::read_to_string(cursor::hooks_path(&home))
        .unwrap()
        .contains("ROOST_AGENT_HOOK"));
}

/// Two ensures at once. The lock spans plan **and** apply, so the second
/// sees the first's result rather than a pre-image of it — without that,
/// both would plan an append and the file would end up with Roost's
/// entry twice, or with one of the two writes lost.
#[test]
fn two_concurrent_ensures_leave_exactly_one_entry() {
    let (_dir, home) = fixture("all");
    let a = home.clone();
    let b = home.clone();

    let left = std::thread::spawn(move || {
        ensure::ensure(&a, ensure::Mode::Auto, &[], "local", Guard::PERMITTED)
    });
    let right = std::thread::spawn(move || {
        ensure::ensure(&b, ensure::Mode::Auto, &[], "local", Guard::PERMITTED)
    });
    for outcome in [
        left.join().unwrap().unwrap(),
        right.join().unwrap().unwrap(),
    ] {
        assert!(outcome.is_clean(), "{:?}", outcome.errors);
    }

    let settings = parse_file(&claude::settings_path(&home));
    for event in roost_agent::CLAUDE_HOOK_EVENTS {
        let ours = settings
            .get("hooks")
            .unwrap()
            .get(event)
            .unwrap()
            .as_array()
            .unwrap()
            .iter()
            .filter(|group| {
                group
                    .get("hooks")
                    .and_then(Json::as_array)
                    .is_some_and(|handlers| {
                        handlers.iter().any(|h| {
                            h.get("command")
                                .and_then(Json::as_str)
                                .is_some_and(|c| is_roost_command(Agent::Claude, c))
                        })
                    })
            })
            .count();
        assert_eq!(ours, 1, "{event} carries {ours} Roost entries");
    }
}

/// A user editing the file between the read and the write is the race
/// the lock cannot cover — a hand edit takes no lock. The digest
/// re-check turns it into a reported skip instead of a lost edit.
#[test]
fn a_user_edit_between_plan_and_apply_is_refused_not_clobbered() {
    let (_dir, home) = fixture("all");
    let plan = ensure::plan(Agent::Claude, &home, ensure::Mode::Auto).unwrap();
    assert!(!plan.is_noop());

    let path = claude::settings_path(&home);
    let edited = std::fs::read_to_string(&path)
        .unwrap()
        .replace("\"opus\"", "\"sonnet\"");
    std::fs::write(&path, &edited).unwrap();

    let err = crate::apply(&plan, Guard::PERMITTED).unwrap_err();
    assert!(
        matches!(err, crate::InstallError::ChangedUnderneath { .. }),
        "{err:?}"
    );
    assert_eq!(std::fs::read_to_string(&path).unwrap(), edited);
}

/// The harness jail. `ROOST_TEST_MODE=1` is set on every e2e lane, and
/// a lane that wrote into a real `~/.claude` would be a bug no assertion
/// could take back.
#[test]
fn the_test_mode_refusal_stops_every_verb_before_it_writes() {
    let (dir, home) = fixture("all");
    let before = snapshot(dir.path());
    let jailed = Guard {
        test_mode: true,
        forced: false,
    };

    for result in [
        ensure::ensure(&home, ensure::Mode::Auto, &[], "local", jailed),
        ensure::ensure(&home, ensure::Mode::Off, &[], "local", jailed),
        ensure::install(&home, &ALL_AGENTS, "local", jailed),
        ensure::uninstall(&home, &ALL_AGENTS, jailed),
    ] {
        assert!(matches!(
            result.unwrap_err(),
            crate::InstallError::TestModeRefused
        ));
    }
    assert_eq!(snapshot(dir.path()), before);

    // And the explicit override lets a test that means it through.
    let forced = Guard {
        test_mode: true,
        forced: true,
    };
    assert!(
        ensure::ensure(&home, ensure::Mode::Auto, &[], "local", forced)
            .unwrap()
            .is_clean()
    );
}

/// An agent that is not installed is not a problem to report — it is a
/// named skip, and Roost never creates the directory that would make it
/// look installed.
#[test]
fn an_absent_agent_is_skipped_and_gets_no_directory() {
    let dir = tempfile::tempdir().unwrap();
    let home = Home::rooted(dir.path());
    let outcome =
        ensure::ensure(&home, ensure::Mode::Auto, &[], "local", Guard::PERMITTED).unwrap();

    assert!(outcome.wired.is_empty());
    assert_eq!(outcome.skipped.len(), ALL_AGENTS.len());
    assert!(outcome
        .skipped
        .iter()
        .all(|s| matches!(s.reason, SkipReason::NotPresent)));
    for agent in ALL_AGENTS {
        assert!(!home.agent_dir(agent).exists(), "{}", agent.source());
    }
}

#[test]
fn status_reports_present_wired_and_current_per_agent() {
    let (_dir, home) = fixture("all");
    let before = ensure::status(&home).unwrap();
    assert_eq!(before.len(), ALL_AGENTS.len());
    assert!(before.iter().all(|s| s.present));
    assert!(before.iter().all(|s| s.wired.is_none()));
    assert!(before.iter().all(|s| !s.up_to_date));

    wire_all(&home);
    let after = ensure::status(&home).unwrap();
    assert!(after
        .iter()
        .all(|s| s.wired == Some(crate::INTEGRATION_VERSION)));
    assert!(after.iter().all(|s| s.up_to_date));
    assert!(after.iter().all(|s| !s.noticed), "the toast fired early");

    crate::mark_noticed(&home, &ALL_AGENTS).unwrap();
    assert!(ensure::status(&home).unwrap().iter().all(|s| s.noticed));
}
