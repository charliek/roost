//! Load every `tests/config-fixtures/*.json` and run it against
//! `crate::config`. The Swift loader in
//! `mac/Tests/RoostTests/ConfigFixtureTests.swift` runs the same files,
//! so a divergence between the two config parsers surfaces on whichever
//! side regressed.
//!
//! Format documented in `tests/config-fixtures/README.md`. In-binary
//! `#[cfg(test)]` module rather than a `tests/` integration test because
//! the parser is a private binary module (not a `roost_linux` library
//! export), so an external test can't see it.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Debug;
use std::fs;
use std::path::PathBuf;

use serde::Deserialize;

use crate::config::{ClipboardWrite, CopyOnSelect, RoostConfig};

fn fixtures_dir() -> PathBuf {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    assert!(p.pop()); // pop "roost-linux"
    assert!(p.pop()); // pop "crates"
    p.push("tests");
    p.push("config-fixtures");
    p
}

#[derive(Deserialize)]
#[serde(tag = "group", rename_all = "snake_case", deny_unknown_fields)]
enum FixtureFile {
    ConfigValues { cases: Vec<ValueCase> },
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ValueCase {
    name: String,
    content: String,
    expect: ValueExpect,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ValueExpect {
    theme: Option<String>,
    font_family: Option<String>,
    font_size: Option<f64>,
    copy_on_select: String,
    clipboard_write: String,
    show_sidebar_agents: bool,
    /// `None` = the default extra-word-char set; `""` = explicit empty.
    word_break_chars: Option<String>,
    keybinds: Vec<KeybindExpect>,
    commands: Vec<CommandExpect>,
    providers: Vec<ProviderExpect>,
}

#[derive(Debug, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
struct KeybindExpect {
    trigger: String,
    action: String,
}

#[derive(Debug, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
struct CommandExpect {
    label: String,
    run: String,
    title: String,
    hold: bool,
    env: Vec<(String, String)>,
}

#[derive(Debug, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProviderExpect {
    label: String,
    run: String,
    title: String,
    timeout_secs: u64,
    limit: usize,
}

fn copy_on_select_name(v: CopyOnSelect) -> &'static str {
    match v {
        CopyOnSelect::Off => "off",
        CopyOnSelect::True => "true",
        CopyOnSelect::Clipboard => "clipboard",
    }
}

fn clipboard_write_name(v: ClipboardWrite) -> &'static str {
    match v {
        ClipboardWrite::Allow => "allow",
        ClipboardWrite::Deny => "deny",
    }
}

fn load() -> Vec<(String, FixtureFile)> {
    let dir = fixtures_dir();
    let mut paths: Vec<PathBuf> = fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("read_dir {dir:?}: {e}"))
        .filter_map(|e| {
            let p = e.ok()?.path();
            (p.extension().and_then(|s| s.to_str()) == Some("json")).then_some(p)
        })
        .collect();
    paths.sort();
    assert!(
        !paths.is_empty(),
        "no fixtures in {dir:?} — did the repo layout change?"
    );
    paths
        .into_iter()
        .map(|path| {
            let name = path
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("?")
                .to_string();
            let raw = fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path:?}: {e}"));
            let parsed: FixtureFile =
                serde_json::from_str(&raw).unwrap_or_else(|e| panic!("parse {name}: {e}"));
            (name, parsed)
        })
        .collect()
}

/// Accumulate rather than assert, so one run reports every mismatch in
/// the corpus instead of only the first.
fn check<T: PartialEq + Debug>(
    failures: &mut Vec<String>,
    at: &str,
    what: &str,
    got: &T,
    want: &T,
) {
    if got != want {
        failures.push(format!("{at}: {what} got {got:?}, want {want:?}"));
    }
}

/// Every group must stay present AND carry real cases. Checking only
/// presence (or only a global case total) lets one group be emptied
/// while the others keep the suite green.
#[test]
fn every_group_is_represented_and_non_trivial() {
    // Floors, not targets: set just under the current counts so genuine
    // pruning is allowed but silently gutting a group is not.
    let want: BTreeMap<&str, usize> = [("config_values", 22)].into_iter().collect();

    let mut found: BTreeMap<&str, usize> = BTreeMap::new();
    for (_, file) in load() {
        let (group, n) = match file {
            FixtureFile::ConfigValues { cases } => ("config_values", cases.len()),
        };
        *found.entry(group).or_default() += n;
    }

    let groups: BTreeSet<&str> = found.keys().copied().collect();
    let expected: BTreeSet<&str> = want.keys().copied().collect();
    assert_eq!(groups, expected, "missing fixture group(s)");
    for (group, floor) in want {
        let got = found[group];
        assert!(
            got >= floor,
            "fixture group {group} has {got} case(s), below the {floor} floor"
        );
    }
}

#[test]
fn every_config_fixture_matches_the_rust_parser() {
    let mut failures: Vec<String> = Vec::new();
    let mut cases = 0usize;

    for (file, parsed) in load() {
        let FixtureFile::ConfigValues { cases: list } = parsed;
        for case in list {
            cases += 1;
            let at = format!("{file}[{}]", case.name);
            let cfg = RoostConfig::parse(&case.content);
            let (f, e) = (&mut failures, &case.expect);
            check(f, &at, "theme", &cfg.theme_name, &e.theme);
            check(f, &at, "font_family", &cfg.font_family, &e.font_family);
            check(f, &at, "font_size", &cfg.font_size, &e.font_size);
            check(
                f,
                &at,
                "copy_on_select",
                &copy_on_select_name(cfg.copy_on_select),
                &e.copy_on_select.as_str(),
            );
            check(
                f,
                &at,
                "clipboard_write",
                &clipboard_write_name(cfg.clipboard_write),
                &e.clipboard_write.as_str(),
            );
            check(
                f,
                &at,
                "show_sidebar_agents",
                &cfg.show_sidebar_agents,
                &e.show_sidebar_agents,
            );
            let want_wbc = e
                .word_break_chars
                .clone()
                .unwrap_or_else(|| RoostConfig::default().word_break_chars);
            check(f, &at, "word_break_chars", &cfg.word_break_chars, &want_wbc);
            let got_keybinds: Vec<KeybindExpect> = cfg
                .keybinds
                .iter()
                .map(|(t, a)| KeybindExpect {
                    trigger: t.clone(),
                    action: a.clone(),
                })
                .collect();
            check(f, &at, "keybinds", &got_keybinds, &e.keybinds);
            let got_commands: Vec<CommandExpect> = cfg
                .commands
                .iter()
                .map(|c| CommandExpect {
                    label: c.label.clone(),
                    run: c.run.clone(),
                    title: c.title.clone(),
                    hold: c.hold,
                    env: c.env.clone(),
                })
                .collect();
            check(f, &at, "commands", &got_commands, &e.commands);
            let got_providers: Vec<ProviderExpect> = cfg
                .providers
                .iter()
                .map(|p| ProviderExpect {
                    label: p.label.clone(),
                    run: p.run.clone(),
                    title: p.title.clone(),
                    timeout_secs: p.timeout_secs,
                    limit: p.limit,
                })
                .collect();
            check(f, &at, "providers", &got_providers, &e.providers);
        }
    }

    assert!(cases > 0, "fixtures loaded but contained no cases");
    assert!(
        failures.is_empty(),
        "config fixture failures ({} of {cases} cases):\n{}",
        failures.len(),
        failures.join("\n")
    );
}
