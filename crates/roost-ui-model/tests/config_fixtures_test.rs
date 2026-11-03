//! Load every `tests/config-fixtures/*.json` and run each case through
//! `RoostConfig::parse`, asserting the parsed values the corpus pins.
//!
//! Format documented in `tests/config-fixtures/README.md`. Integration-test
//! form (in `tests/`) because the path walk starts at `CARGO_MANIFEST_DIR`
//! and climbs to the workspace `tests/` directory.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use roost_ui_model::config::{ClipboardWrite, CopyOnSelect, RoostConfig};
use roost_ui_model::word_selection::DEFAULT_EXTRA_WORD_CHARS;

fn fixtures_dir() -> PathBuf {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    assert!(p.pop()); // pop "roost-ui-model"
    assert!(p.pop()); // pop "crates"
    p.push("tests");
    p.push("config-fixtures");
    p
}

#[derive(Deserialize, Debug, PartialEq)]
#[serde(deny_unknown_fields)]
struct Keybind {
    trigger: String,
    action: String,
}

#[derive(Deserialize, Debug, PartialEq)]
#[serde(deny_unknown_fields)]
struct Command {
    label: String,
    run: String,
    title: String,
    hold: bool,
    env: Vec<(String, String)>,
}

#[derive(Deserialize, Debug, PartialEq)]
#[serde(deny_unknown_fields)]
struct Provider {
    label: String,
    run: String,
    title: String,
    timeout_secs: u64,
    limit: usize,
}

/// Every field is required (no `#[serde(default)]`): the README has each
/// case spell every one out, so a case that forgets one is a schema error
/// here rather than a silent default.
#[derive(Deserialize, Debug)]
#[serde(deny_unknown_fields)]
struct Expect {
    theme: Option<String>,
    font_family: Option<String>,
    font_size: Option<f64>,
    copy_on_select: String,
    clipboard_write: String,
    show_sidebar_agents: bool,
    /// `None`: the default extra-word-char set; `""` is an explicit empty.
    word_break_chars: Option<String>,
    keybinds: Vec<Keybind>,
    commands: Vec<Command>,
    providers: Vec<Provider>,
}

#[derive(Deserialize, Debug)]
#[serde(deny_unknown_fields)]
struct Case {
    name: String,
    content: String,
    expect: Expect,
}

/// One file, by its `group`. A file naming a group not listed here fails
/// to load rather than being skipped.
#[derive(Deserialize)]
#[serde(tag = "group", rename_all = "snake_case", deny_unknown_fields)]
enum FixtureFile {
    ConfigValues { cases: Vec<Case> },
}

impl FixtureFile {
    /// The group as the file spells it.
    fn group(&self) -> &'static str {
        match self {
            Self::ConfigValues { .. } => "config_values",
        }
    }

    fn cases(&self) -> &[Case] {
        match self {
            Self::ConfigValues { cases } => cases,
        }
    }
}

fn load(path: &Path) -> FixtureFile {
    let raw = fs::read_to_string(path).unwrap_or_else(|e| panic!("read {path:?}: {e}"));
    serde_json::from_str(&raw).unwrap_or_else(|e| panic!("parse {path:?}: {e}"))
}

/// `(file name, contents)` for every `*.json` in the corpus, sorted by name.
fn load_all() -> Vec<(String, FixtureFile)> {
    let dir = fixtures_dir();
    let mut paths: Vec<PathBuf> = fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("read_dir {dir:?}: {e}"))
        .map(|entry| entry.expect("a directory entry").path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
        .collect();
    paths.sort();
    assert!(!paths.is_empty(), "no fixtures in {dir:?}");
    paths
        .iter()
        .map(|path| {
            let file = path.file_name().unwrap().to_string_lossy().into_owned();
            (file, load(path))
        })
        .collect()
}

fn copy_on_select_name(value: CopyOnSelect) -> &'static str {
    match value {
        CopyOnSelect::Off => "off",
        CopyOnSelect::True => "true",
        CopyOnSelect::Clipboard => "clipboard",
    }
}

fn clipboard_write_name(value: ClipboardWrite) -> &'static str {
    match value {
        ClipboardWrite::Allow => "allow",
        ClipboardWrite::Deny => "deny",
    }
}

/// Every mismatch between `cfg` and `want`, one line each.
fn mismatches(cfg: &RoostConfig, want: &Expect) -> Vec<String> {
    let mut out = Vec::new();
    let mut check = |what: &str, got: String, want: String| {
        if got != want {
            out.push(format!("{what}: got {got}, want {want}"));
        }
    };
    check(
        "theme",
        format!("{:?}", cfg.theme_name),
        format!("{:?}", want.theme),
    );
    check(
        "font_family",
        format!("{:?}", cfg.font_family),
        format!("{:?}", want.font_family),
    );
    check(
        "font_size",
        format!("{:?}", cfg.font_size),
        format!("{:?}", want.font_size),
    );
    check(
        "copy_on_select",
        copy_on_select_name(cfg.copy_on_select).into(),
        want.copy_on_select.clone(),
    );
    check(
        "clipboard_write",
        clipboard_write_name(cfg.clipboard_write).into(),
        want.clipboard_write.clone(),
    );
    check(
        "show_sidebar_agents",
        cfg.show_sidebar_agents.to_string(),
        want.show_sidebar_agents.to_string(),
    );
    check(
        "word_break_chars",
        format!("{:?}", cfg.word_break_chars),
        format!(
            "{:?}",
            want.word_break_chars
                .as_deref()
                .unwrap_or(DEFAULT_EXTRA_WORD_CHARS)
        ),
    );
    let keybinds: Vec<Keybind> = cfg
        .keybinds
        .iter()
        .map(|(trigger, action)| Keybind {
            trigger: trigger.clone(),
            action: action.clone(),
        })
        .collect();
    check(
        "keybinds",
        format!("{keybinds:?}"),
        format!("{:?}", want.keybinds),
    );
    let commands: Vec<Command> = cfg
        .commands
        .iter()
        .map(|c| Command {
            label: c.label.clone(),
            run: c.run.clone(),
            title: c.title.clone(),
            hold: c.hold,
            env: c.env.clone(),
        })
        .collect();
    check(
        "commands",
        format!("{commands:?}"),
        format!("{:?}", want.commands),
    );
    let providers: Vec<Provider> = cfg
        .providers
        .iter()
        .map(|p| Provider {
            label: p.label.clone(),
            run: p.run.clone(),
            title: p.title.clone(),
            timeout_secs: p.timeout_secs,
            limit: p.limit,
        })
        .collect();
    check(
        "providers",
        format!("{providers:?}"),
        format!("{:?}", want.providers),
    );
    out
}

/// Each group must stay present and carry real cases: a floor just under
/// today's count, so pruning a case is allowed and gutting the group is
/// not. The loader reports failures by name, so names must be unique.
#[test]
fn every_config_fixture_group_is_represented_and_non_trivial() {
    const FLOORS: [(&str, usize); 1] = [("config_values", 22)];
    let mut found: BTreeMap<&str, usize> = BTreeMap::new();
    let mut names = BTreeSet::new();
    for (file, loaded) in load_all() {
        *found.entry(loaded.group()).or_default() += loaded.cases().len();
        for case in loaded.cases() {
            assert!(
                names.insert(case.name.clone()),
                "{file}: case name {:?} is not unique",
                case.name
            );
        }
    }
    assert_eq!(
        found.keys().copied().collect::<BTreeSet<_>>(),
        FLOORS.iter().map(|(group, _)| *group).collect(),
        "the corpus's groups"
    );
    for (group, floor) in FLOORS {
        let got = found.get(group).copied().unwrap_or(0);
        assert!(
            got >= floor,
            "fixture group {group} has {got} case(s), below the {floor} floor"
        );
    }
}

#[test]
fn every_config_fixture_matches_the_parser() {
    let mut failures = Vec::new();
    let mut cases = 0;
    for (file, loaded) in load_all() {
        for case in loaded.cases() {
            cases += 1;
            let cfg = RoostConfig::parse(&case.content);
            for mismatch in mismatches(&cfg, &case.expect) {
                failures.push(format!("{file}[{}]: {mismatch}", case.name));
            }
        }
    }
    assert!(cases > 0, "fixtures loaded but contained no cases");
    assert!(
        failures.is_empty(),
        "config fixture failures ({} across {cases} cases):\n{}",
        failures.len(),
        failures.join("\n")
    );
}
