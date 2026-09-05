//! Load every `tests/agent-state-fixtures/*.json` and run it against
//! `roost_ipc::agent`. The Swift loader in
//! `mac/Tests/RoostTests/AgentStateFixtureTests.swift` runs the same
//! files, so a divergence between the two ports of the agent state
//! machine surfaces on whichever side regressed.
//!
//! Format documented in `tests/agent-state-fixtures/README.md`.
//! Integration-test form (in `tests/`) because the path walk starts at
//! `CARGO_MANIFEST_DIR` and climbs to the workspace `tests/` directory.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Debug;
use std::fs;
use std::path::PathBuf;

use serde::Deserialize;

use roost_ipc::agent::{
    apply_report, apply_shell_mark, effective, is_live, rank, suppress_raw_osc, AgentLifecycle,
    AgentTabState, AttentionEffect, ShellState, TabAgentReportParams,
};
use roost_ipc::messages::TabState;

fn fixtures_dir() -> PathBuf {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    assert!(p.pop()); // pop "roost-ipc"
    assert!(p.pop()); // pop "crates"
    p.push("tests");
    p.push("agent-state-fixtures");
    p
}

#[derive(Deserialize)]
#[serde(tag = "group", rename_all = "snake_case", deny_unknown_fields)]
enum FixtureFile {
    Derivation {
        cases: Vec<DerivationCase>,
    },
    Rank {
        order_high_to_low: Vec<AgentLifecycle>,
        cases: Vec<RankCase>,
    },
    Transitions {
        cases: Vec<TransitionCase>,
    },
    ShellMarks {
        cases: Vec<ShellMarkCase>,
    },
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ShellMarkCase {
    name: String,
    state: AgentTabState,
    body: String,
    expect: ShellMarkExpect,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ShellMarkExpect {
    /// `false` means the mark is undefined and the state is untouched.
    changed: bool,
    shell: ShellState,
    lifecycle: AgentLifecycle,
    owner_retained: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DerivationCase {
    name: String,
    state: AgentTabState,
    expect: DerivationExpect,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DerivationExpect {
    effective: TabState,
    is_live: bool,
    suppress_raw_osc: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RankCase {
    name: String,
    lifecycle: AgentLifecycle,
    expect: RankExpect,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RankExpect {
    rank: u8,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TransitionCase {
    name: String,
    now: i64,
    current: AgentTabState,
    /// Optional OSC 133 mark applied to `current` **before** the report,
    /// so a case can state a sequence the real world produces — the
    /// prompt-mark failsafe dropping the lifecycle, then a guarded
    /// report landing on what it left behind.
    #[serde(default)]
    shell_mark: Option<String>,
    report: TabAgentReportParams,
    expect: TransitionExpect,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TransitionExpect {
    accepted: bool,
    ownership_changed: bool,
    lifecycle_changed: bool,
    attention: AttentionEffect,
    state: AgentTabState,
    effective: TabState,
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
/// while the others keep the suite green — the whole corpus would then
/// pass while testing nothing on that axis.
#[test]
fn every_group_is_represented_and_non_trivial() {
    // Floors, not targets: set just under the current counts so genuine
    // pruning is allowed but silently gutting a group is not.
    let want: BTreeMap<&str, usize> = [
        ("derivation", 12),
        ("rank", 5),
        ("transitions", 18),
        ("shell_marks", 7),
    ]
    .into_iter()
    .collect();

    let mut found: BTreeMap<&str, usize> = BTreeMap::new();
    for (_, file) in load() {
        let (group, n) = match file {
            FixtureFile::Derivation { cases } => ("derivation", cases.len()),
            FixtureFile::Rank { cases, .. } => ("rank", cases.len()),
            FixtureFile::Transitions { cases } => ("transitions", cases.len()),
            FixtureFile::ShellMarks { cases } => ("shell_marks", cases.len()),
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
fn every_agent_state_fixture_matches_the_rust_implementation() {
    let mut failures: Vec<String> = Vec::new();
    let mut cases = 0usize;

    for (file, parsed) in load() {
        match parsed {
            FixtureFile::Derivation { cases: list } => {
                for case in list {
                    cases += 1;
                    let at = format!("{file}[{}]", case.name);
                    let (f, s, e) = (&mut failures, &case.state, &case.expect);
                    check(f, &at, "is_live", &is_live(s), &e.is_live);
                    check(f, &at, "effective", &effective(s), &e.effective);
                    check(
                        f,
                        &at,
                        "suppress_raw_osc",
                        &suppress_raw_osc(s),
                        &e.suppress_raw_osc,
                    );
                }
            }
            FixtureFile::Rank {
                order_high_to_low,
                cases: list,
            } => {
                for case in list {
                    cases += 1;
                    let at = format!("{file}[{}]", case.name);
                    check(
                        &mut failures,
                        &at,
                        "rank",
                        &rank(case.lifecycle),
                        &case.expect.rank,
                    );
                }
                for pair in order_high_to_low.windows(2) {
                    let (hi, lo) = (pair[0], pair[1]);
                    if rank(hi) <= rank(lo) {
                        failures.push(format!(
                            "{file}[order]: rank({hi:?})={} must exceed rank({lo:?})={}",
                            rank(hi),
                            rank(lo)
                        ));
                    }
                }
            }
            FixtureFile::Transitions { cases: list } => {
                for case in list {
                    cases += 1;
                    let at = format!("{file}[{}]", case.name);
                    let current = match &case.shell_mark {
                        Some(body) => apply_shell_mark(&case.current, body).unwrap_or_else(|| {
                            panic!("{at}: shell_mark {body:?} is not a defined mark")
                        }),
                        None => case.current.clone(),
                    };
                    let out = apply_report(&current, &case.report, case.now);
                    let (f, o, e) = (&mut failures, &out, &case.expect);
                    check(f, &at, "accepted", &o.accepted, &e.accepted);
                    check(
                        f,
                        &at,
                        "ownership_changed",
                        &o.ownership_changed,
                        &e.ownership_changed,
                    );
                    check(
                        f,
                        &at,
                        "lifecycle_changed",
                        &o.lifecycle_changed,
                        &e.lifecycle_changed,
                    );
                    check(f, &at, "attention", &o.attention, &e.attention);
                    check(f, &at, "state", &o.state, &e.state);
                    check(f, &at, "effective", &effective(&o.state), &e.effective);
                    if !case.expect.accepted && out.state != current {
                        failures.push(format!(
                            "{at}: a dropped report must leave state untouched, got {:?}",
                            out.state
                        ));
                    }
                }
            }
            FixtureFile::ShellMarks { cases: list } => {
                for case in list {
                    cases += 1;
                    let at = format!("{file}[{}]", case.name);
                    let got = apply_shell_mark(&case.state, &case.body);
                    let (f, e) = (&mut failures, &case.expect);
                    check(f, &at, "changed", &got.is_some(), &e.changed);
                    let after = got.unwrap_or_else(|| case.state.clone());
                    check(f, &at, "shell", &after.shell, &e.shell);
                    check(f, &at, "lifecycle", &after.lifecycle, &e.lifecycle);
                    check(
                        f,
                        &at,
                        "owner_retained",
                        &after.ownership.is_some(),
                        &e.owner_retained,
                    );
                }
            }
        }
    }

    assert!(cases > 0, "fixtures loaded but contained no cases");
    assert!(
        failures.is_empty(),
        "agent-state fixture failures ({} of {cases} checks):\n{}",
        failures.len(),
        failures.join("\n")
    );
}
