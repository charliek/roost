//! Load `tests/agent-display-name-fixtures/cases.json` and assert
//! agreement with `agent_palette::agent_display_name`. The Swift loader
//! in `mac/Tests/RoostTests/AgentDisplayNameFixtureTests.swift` runs the
//! same file, so a divergence between the two UIs' source→display-name
//! tables surfaces here rather than in a user's agents palette.
//!
//! Format documented in `tests/agent-display-name-fixtures/README.md`.
//! Integration-test form (in `tests/`) because the path walk starts at
//! `CARGO_MANIFEST_DIR` and climbs to the workspace `tests/` directory.

use std::fs;
use std::path::PathBuf;

use serde::Deserialize;

use roost_ui_model::agent_palette::agent_display_name;

fn fixtures_dir() -> PathBuf {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    assert!(p.pop()); // pop "roost-ui-model"
    assert!(p.pop()); // pop "crates"
    p.push("tests");
    p.push("agent-display-name-fixtures");
    p
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Case {
    name: String,
    source: String,
    expect: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CasesFile {
    cases: Vec<Case>,
}

#[test]
fn every_agent_display_name_fixture_matches_the_rust_implementation() {
    let path = fixtures_dir().join("cases.json");
    let raw = fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path:?}: {e}"));
    let file: CasesFile =
        serde_json::from_str(&raw).unwrap_or_else(|e| panic!("parse {path:?}: {e}"));
    assert!(
        !file.cases.is_empty(),
        "fixture file loaded but contained no cases"
    );

    let mut failures = Vec::new();
    for case in &file.cases {
        let got = agent_display_name(&case.source);
        if got != case.expect {
            failures.push(format!(
                "[{}] agent_display_name({:?}) = {:?}, want {:?}",
                case.name, case.source, got, case.expect
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "fixture failures:\n{}",
        failures.join("\n")
    );
}
