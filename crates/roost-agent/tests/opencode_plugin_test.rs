//! The opencode plugin and the opencode adapter, exercised as one path
//! (plan 046 §3.9).
//!
//! opencode is the only agent whose events reach Roost through code we
//! ship in another language, so the seam between the two — what the JS
//! forwards, and what the Rust makes of it — is the part a fixture
//! replay alone cannot cover. This test drives the **shipped** asset
//! under `node`: a stub `$ROOST_AGENT_HOOK` records argv and stdin, a
//! small harness feeds the plugin bus events built from the captured
//! probe, and every recorded stdin is then replayed through
//! [`roost_agent::opencode::opencode_event_to_reports`].
//!
//! It lives here, as a Rust integration test, rather than in a JS test
//! runner: the crate has no JS toolchain, and the assertion that
//! matters is the *Rust* policy applied to *JS-produced* bytes, which
//! only a test that owns both ends can make.
//!
//! The harness never waits between events. The bus hands them over as
//! fast as they happen, so a harness that drained one hook before
//! emitting the next would be supplying the ordering the plugin is
//! supposed to provide, and would pass whether or not it does.
//!
//! `node` is not a build dependency of Roost. Where it is absent the
//! test skips loudly and plan §8's live opencode run is what covers
//! this instead.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use roost_agent::opencode::opencode_event_to_reports;
use roost_ipc::agent::{
    apply_report, validate_report, AgentLifecycle, AgentTabState, Ownership, OwnershipAction,
};
use serde_json::{json, Value};

const TAB: i64 = 7;
const NOW: i64 = 1_757_000_000;

const ROOT: &str = "ses_f91cef768ffeTI8TEd0E4v53Ov";
const CHILD: &str = "ses_child";
const SECOND_ROOT: &str = "ses_second_root";

/// The shipped plugin, copied into the scratch tree verbatim — this
/// test must never drift from what the install engine writes out.
const PLUGIN: &str = include_str!("../assets/opencode/roost-agent-state.js");

/// Records argv and stdin of one `agent-hook` invocation into its own
/// file, named for the sequence number it takes *after* finishing — so
/// the file names are completion order, which is the order Roost would
/// have seen these reports arrive.
///
/// Three behaviours ride on env vars, each off by default: wedge the
/// first invocation, stall one named event, and nothing else.
const STUB: &str = r#"#!/bin/sh
if [ -n "$ROOST_TEST_HANG_DIR" ] && mkdir "$ROOST_TEST_HANG_DIR/hung" 2>/dev/null; then
  printf '%s\n' "$$" > "$ROOST_TEST_PID_FILE"
  exec sleep 30
fi
out=$(mktemp "$ROOST_TEST_STAGE_DIR/rec.XXXXXX")
printf '%s\n' "$*" > "$out"
cat >> "$out"
if [ -n "$ROOST_TEST_SLOW_EVENT" ] && grep -q "\"hook_event_name\":\"$ROOST_TEST_SLOW_EVENT\"" "$out"; then
  sleep "$ROOST_TEST_SLOW_SECONDS"
fi
i=0
while ! mkdir "$ROOST_TEST_SEQ_DIR/$i" 2>/dev/null; do i=$((i + 1)); done
mv "$out" "$ROOST_TEST_RECORD_DIR/$i"
"#;

/// Emits every bus event back to back, then disposes, then reads the
/// records in completion order. Nothing here waits on a hook.
const HARNESS: &str = r#"
import { readFileSync, readdirSync, writeFileSync } from "node:fs";
import { RoostAgentState } from "./roost-agent-state.js";

const dir = process.env.ROOST_TEST_RECORD_DIR;
const steps = JSON.parse(readFileSync(process.env.ROOST_TEST_EVENTS, "utf8"));
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

const hooks = await RoostAgentState({});
if (typeof hooks.event !== "function") throw new Error("no event handler");
if (typeof hooks.dispose !== "function") throw new Error("no dispose handler");

for (const step of steps) {
  await hooks.event({ event: { type: step.type, properties: step.properties } });
}

const started = Date.now();
await hooks.dispose();
const disposeMs = Date.now() - started;

// Long enough for a forward the whitelist should have dropped, or one
// still queued behind the rest, to turn up late.
await sleep(Number(process.env.ROOST_TEST_SETTLE_MS));

// The probe confirmed opencode's TUI inherits Roost's tab env; outside
// a Roost tab the plugin must install no handlers at all.
delete process.env.ROOST_TAB_ID;
const bare = await RoostAgentState({});
if (Object.keys(bare).length !== 0) throw new Error("handlers installed without ROOST_TAB_ID");

const records = readdirSync(dir)
  .map(Number)
  .sort((a, b) => a - b)
  .map((n) => readFileSync(`${dir}/${n}`, "utf8"));
writeFileSync(process.env.ROOST_TEST_ORDERED, JSON.stringify({ records, disposeMs }));
console.log("HARNESS_OK");
"#;

/// True when there is no `node` to drive the plugin with, having said so
/// on stderr first: every test here skips loudly rather than failing on a
/// machine with no JS toolchain.
fn node_is_missing() -> bool {
    let available = Command::new("node")
        .arg("--version")
        .output()
        .is_ok_and(|out| out.status.success());
    if !available {
        eprintln!("skipping: no `node` on PATH (plan §8's live opencode run covers this)");
    }
    !available
}

fn fixture_record(event: &str) -> Value {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/opencode.jsonl");
    let text = fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    for line in text.lines().filter(|line| !line.trim().is_empty()) {
        let record: Value = serde_json::from_str(line).expect("opencode.jsonl parses");
        if record["event"] == json!(event) {
            return record["payload"].clone();
        }
    }
    panic!("opencode.jsonl has no {event}");
}

/// The captured `session.created`, re-pointed at `id` — and, when
/// `parent` is given, made a child session of it. The probe caught only
/// the one root creation, so every other session in these scenarios is
/// derived from that record rather than hand-built.
fn created_as(id: &str, parent: Option<&str>) -> Value {
    let mut record = fixture_record("session.created");
    record["sessionID"] = json!(id);
    record["info"]["id"] = json!(id);
    if let Some(parent) = parent {
        record["info"]["parentID"] = json!(parent);
    }
    record
}

/// The captured `permission.asked`, re-pointed at the child session —
/// the prompt a subagent raises.
fn child_permission() -> Value {
    let mut asked = fixture_record("permission.asked");
    asked["sessionID"] = json!(CHILD);
    asked
}

fn write(path: &Path, contents: &str) {
    fs::write(path, contents).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
}

/// One recorded `agent-hook` invocation: the argv the plugin used and
/// the JSON it wrote to stdin.
struct Recorded {
    argv: String,
    payload: Value,
}

impl Recorded {
    fn event(&self) -> &str {
        self.payload["hook_event_name"]
            .as_str()
            .expect("every forward names its event")
    }

    fn session_id(&self) -> &str {
        self.payload["session_id"].as_str().unwrap_or("")
    }
}

/// One `node` process, and therefore one fresh plugin instance.
#[derive(Default)]
struct Harness {
    /// Name of an event whose hook should take [`Self::slow_seconds`]
    /// to finish, so an unserialized forward would overtake it.
    slow_event: Option<&'static str>,
    slow_seconds: &'static str,
    /// Wedge the *first* hook invocation: it never reads stdin and never
    /// exits, so only the plugin's own kill timeout ends it.
    hang_first: bool,
    /// A hook binary that cannot be spawned at all.
    missing_hook: bool,
    /// How long the harness waits after `dispose` before reading the
    /// records back. Default 300 ms.
    settle_ms: u64,
}

struct Run {
    records: Vec<Recorded>,
    dispose_ms: u64,
    stderr: String,
    /// PIDs the wedged branch of the stub recorded, if any.
    hung: Vec<i32>,
}

impl Harness {
    fn run(self, steps: &Value) -> Run {
        let scratch = tempfile::tempdir().expect("scratch tree");
        let dir = scratch.path();
        let records = dir.join("records");
        let stage = dir.join("stage");
        let seq = dir.join("seq");
        for path in [&records, &stage, &seq] {
            fs::create_dir(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        }
        let stub = dir.join("hook.sh");
        let ordered = dir.join("ordered.json");
        let pids = dir.join("hung.pids");

        write(&dir.join("roost-agent-state.js"), PLUGIN);
        write(&dir.join("package.json"), "{ \"type\": \"module\" }\n");
        write(&dir.join("harness.mjs"), HARNESS);
        write(&dir.join("events.json"), &steps.to_string());
        write(&stub, STUB);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&stub, fs::Permissions::from_mode(0o755)).expect("chmod stub");
        }

        let hook = if self.missing_hook {
            dir.join("no-such-hook")
        } else {
            stub.clone()
        };
        let mut node = Command::new("node");
        node.arg("harness.mjs")
            .current_dir(dir)
            .env("ROOST_AGENT_HOOK", &hook)
            .env("ROOST_TAB_ID", "7")
            .env("ROOST_SOCKET", dir.join("roost.sock"))
            .env("ROOST_TEST_RECORD_DIR", &records)
            .env("ROOST_TEST_STAGE_DIR", &stage)
            .env("ROOST_TEST_SEQ_DIR", &seq)
            .env("ROOST_TEST_EVENTS", dir.join("events.json"))
            .env("ROOST_TEST_ORDERED", &ordered)
            .env(
                "ROOST_TEST_SETTLE_MS",
                if self.settle_ms == 0 {
                    "300".to_string()
                } else {
                    self.settle_ms.to_string()
                },
            );
        if let Some(event) = self.slow_event {
            node.env("ROOST_TEST_SLOW_EVENT", event)
                .env("ROOST_TEST_SLOW_SECONDS", self.slow_seconds);
        }
        if self.hang_first {
            node.env("ROOST_TEST_HANG_DIR", dir)
                .env("ROOST_TEST_PID_FILE", &pids);
        }

        let out = node.output().expect("node runs");
        let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
        assert!(
            out.status.success(),
            "harness failed: {}\n{stderr}",
            String::from_utf8_lossy(&out.stdout),
        );
        assert!(String::from_utf8_lossy(&out.stdout).contains("HARNESS_OK"));

        let read: Value =
            serde_json::from_str(&fs::read_to_string(&ordered).expect("ordered.json"))
                .expect("json");
        let records = read["records"]
            .as_array()
            .expect("records array")
            .iter()
            .map(|record| {
                let record = record.as_str().expect("record is text");
                let (argv, stdin) = record.split_once('\n').expect("argv line then stdin");
                Recorded {
                    argv: argv.to_string(),
                    payload: serde_json::from_str(stdin)
                        .unwrap_or_else(|e| panic!("stdin is not JSON: {stdin}: {e}")),
                }
            })
            .collect();
        let hung = fs::read_to_string(&pids)
            .unwrap_or_default()
            .lines()
            .map(|line| line.trim().parse().expect("a pid"))
            .collect();

        Run {
            records,
            dispose_ms: read["disposeMs"].as_u64().expect("disposeMs"),
            stderr,
            hung,
        }
    }
}

impl Run {
    fn events(&self) -> Vec<&str> {
        self.records.iter().map(Recorded::event).collect()
    }

    fn session_ids(&self) -> Vec<&str> {
        self.records.iter().map(Recorded::session_id).collect()
    }

    /// Replay the recorded bytes through the Rust policy, in the order
    /// the plugin actually delivered them.
    fn replay(&self, from: AgentTabState) -> Vec<AgentTabState> {
        let mut state = from;
        let mut states = Vec::new();
        for record in &self.records {
            for report in opencode_event_to_reports(record.event(), &record.payload, TAB) {
                validate_report(&report).unwrap_or_else(|e| panic!("{}: {e}", record.event()));
                state = apply_report(&state, &report, NOW).state;
            }
            states.push(state.clone());
        }
        states
    }

    fn claims(&self) -> usize {
        self.records
            .iter()
            .flat_map(|record| opencode_event_to_reports(record.event(), &record.payload, TAB))
            .filter(|report| report.ownership_action == OwnershipAction::Claim)
            .count()
    }
}

fn step(event: &str, properties: &Value) -> Value {
    json!({ "type": event, "properties": properties })
}

fn owned_by(session_id: &str, lifecycle: AgentLifecycle) -> AgentTabState {
    AgentTabState {
        lifecycle,
        ownership: Some(Ownership {
            source: "opencode".to_string(),
            session_id: session_id.to_string(),
            ..Ownership::default()
        }),
        ..AgentTabState::default()
    }
}

fn wait_until_gone(pid: i32) -> bool {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let alive = Command::new("sh")
            .arg("-c")
            .arg(format!("kill -0 {pid} 2>/dev/null"))
            .status()
            .expect("kill -0 runs")
            .success();
        if !alive {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// The bus sequence the first scenario replays: the captured root
/// session, the flood event the whitelist must drop, a child session and
/// its permission prompt, then a second root that takes over.
fn steps() -> Value {
    json!([
        step("session.created", &fixture_record("session.created")),
        // 697 of the probe's 862 records are this event.
        step("message.part.delta", &fixture_record("message.part.delta")),
        step("session.created", &created_as(CHILD, Some(ROOT))),
        step("permission.asked", &child_permission()),
        step("session.created", &created_as(SECOND_ROOT, None)),
        // A turn has to actually run before it can idle: `session.idle`
        // is guarded on `["working", "waiting"]`, so without this the
        // claim's `inactive` would veto it.
        step("chat.message", &fixture_record("chat.message")),
        step("session.idle", &fixture_record("session.idle")),
    ])
}

#[test]
fn the_plugin_forwards_what_the_adapter_maps() {
    if node_is_missing() {
        return;
    }

    let run = Harness::default().run(&steps());

    // The flood event is absent because the plugin never spawned for
    // it, and a late record would have shown up in this list too.
    assert_eq!(
        run.events(),
        [
            "session.created",
            "session.created",
            "permission.asked",
            "session.created",
            "chat.message",
            "session.idle",
            "dispose",
        ],
    );

    for record in &run.records {
        assert_eq!(record.argv, "agent-hook opencode");
    }

    // The root rule, on the wire: the child session and its permission
    // prompt both carry the *root's* id, and the second root takes over
    // from there.
    assert_eq!(
        run.session_ids(),
        [
            ROOT,
            ROOT,
            ROOT,
            SECOND_ROOT,
            SECOND_ROOT,
            SECOND_ROOT,
            SECOND_ROOT
        ],
    );
    // …and the child's own id is still in the payload, untouched — the
    // plugin adds, it does not rewrite.
    assert_eq!(run.records[1].payload["sessionID"], json!(CHILD));

    // Now the other half of the seam: the same bytes through the Rust
    // policy. Two claims, not three — the child `session.created` the
    // plugin faithfully forwarded is dropped by the adapter, which is
    // the whole reason the plugin carries no policy of its own.
    assert_eq!(run.claims(), 2);
    let states = run.replay(AgentTabState::default());
    assert_eq!(
        states
            .iter()
            .map(|state| state.ownership.as_ref().map(|o| o.session_id.as_str()))
            .collect::<Vec<_>>(),
        [
            Some(ROOT),
            Some(ROOT),
            Some(ROOT),
            Some(SECOND_ROOT),
            Some(SECOND_ROOT),
            Some(SECOND_ROOT),
            None,
        ],
    );
    assert_eq!(
        states
            .iter()
            .map(|state| state.lifecycle)
            .collect::<Vec<_>>(),
        [
            AgentLifecycle::Inactive,
            AgentLifecycle::Inactive,
            AgentLifecycle::Waiting,
            AgentLifecycle::Inactive,
            AgentLifecycle::Working,
            AgentLifecycle::Finished,
            AgentLifecycle::Inactive,
        ],
    );
}

/// Forwards are serialized, so a slow hook holds up the next one rather
/// than being overtaken by it.
///
/// `apply_report` has no sequence numbers and the wire promises no
/// ordering, so two hooks in flight at once decide the tab's state by
/// who wins the race. Reversed, `permission.replied` lands first and the
/// `permission.asked` behind it leaves the tab stuck `waiting` under a
/// warning banner the user already answered.
#[test]
fn a_slow_forward_is_not_overtaken_by_the_one_behind_it() {
    if node_is_missing() {
        return;
    }

    let created = fixture_record("session.created");
    let asked = fixture_record("permission.asked");
    let replied = fixture_record("permission.replied");
    let run = Harness {
        slow_event: Some("permission.asked"),
        slow_seconds: "0.6",
        ..Harness::default()
    }
    .run(&json!([
        step("session.created", &created),
        step("permission.asked", &asked),
        step("permission.replied", &replied),
    ]));

    assert_eq!(
        run.events(),
        [
            "session.created",
            "permission.asked",
            "permission.replied",
            "dispose"
        ],
    );

    // The symptom, spelled out: replayed in delivery order the tab is
    // released from `working`, never left waiting on a prompt that was
    // already answered.
    assert_eq!(
        run.replay(AgentTabState::default())
            .iter()
            .map(|state| state.lifecycle)
            .collect::<Vec<_>>(),
        [
            AgentLifecycle::Inactive,
            AgentLifecycle::Waiting,
            AgentLifecycle::Working,
            AgentLifecycle::Inactive,
        ],
    );
}

/// `opencode attach`, and a plugin loaded mid-session: the first
/// `session.created` the plugin ever sees is a child.
///
/// Its `parentID` is then the only root there is. Discarded, every later
/// event would be scoped to the child id, and the adapter's report would
/// be rejected against the root that actually owns the tab — no waiting
/// state, no banner, nothing.
#[test]
fn a_first_seen_child_session_adopts_its_parent_as_the_root() {
    if node_is_missing() {
        return;
    }

    let run = Harness::default().run(&json!([
        step("session.created", &created_as(CHILD, Some(ROOT))),
        step("permission.asked", &child_permission()),
    ]));

    assert_eq!(
        run.events(),
        ["session.created", "permission.asked", "dispose"],
    );
    assert_eq!(run.session_ids(), [ROOT, ROOT, ROOT]);
    // The child's `session.created` is still not a claim — a subagent
    // must never evict its parent — so the root has to have been claimed
    // before the plugin loaded, which is exactly the attach case.
    assert_eq!(run.claims(), 0);

    let states = run.replay(owned_by(ROOT, AgentLifecycle::Working));
    assert_eq!(states[1].lifecycle, AgentLifecycle::Waiting);
    assert_eq!(states[2].ownership, None);

    // The control: scoped to the child's own id, every one of those
    // reports is rejected against the root owner and the tab never
    // moves.
    let root_state = owned_by(ROOT, AgentLifecycle::Working);
    for record in &run.records {
        let mut payload = record.payload.clone();
        payload["session_id"] = json!(CHILD);
        for report in opencode_event_to_reports(record.event(), &payload, TAB) {
            let outcome = apply_report(&root_state, &report, NOW);
            assert!(!outcome.accepted, "{}", record.event());
            assert_eq!(outcome.state, root_state);
        }
    }
}

/// A `session.created` whose own session cannot be read is not forwarded
/// at all.
///
/// Forwarded, it would carry the *previous* root's id — the plugin
/// stamps one onto every event — and the adapter reads a
/// `session.created` with a session id as an unconditional `Claim` at
/// lifecycle `inactive`. That stops a live turn's dot, or reinstates an
/// owner a newer session already superseded.
#[test]
fn a_session_created_that_cannot_name_its_session_is_dropped() {
    if node_is_missing() {
        return;
    }

    let run = Harness::default().run(&json!([
        step("session.created", &fixture_record("session.created")),
        step("chat.message", &fixture_record("chat.message")),
        step("session.created", &json!({})),
        step("session.created", &json!({ "sessionID": 12345 })),
        step(
            "session.created",
            &json!({ "sessionID": "", "info": { "id": null } })
        ),
        step("session.idle", &fixture_record("session.idle")),
    ]));

    assert_eq!(
        run.events(),
        ["session.created", "chat.message", "session.idle", "dispose"],
    );
    assert_eq!(run.claims(), 1);
    assert_eq!(run.session_ids(), [ROOT, ROOT, ROOT, ROOT]);

    // The turn still ends on its own `session.idle` — nothing re-claimed
    // the tab back to `inactive` in between, which is what a forwarded
    // stale-id creation would have done.
    assert_eq!(
        run.replay(AgentTabState::default())
            .iter()
            .map(|state| state.lifecycle)
            .collect::<Vec<_>>(),
        [
            AgentLifecycle::Inactive,
            AgentLifecycle::Working,
            AgentLifecycle::Finished,
            AgentLifecycle::Inactive,
        ],
    );
}

/// A hook that never exits is killed, not accumulated.
///
/// `roostctl agent-hook` budgets itself 2 s; a misconfigured
/// `$ROOST_AGENT_HOOK` that reads stdin and never returns would
/// otherwise leave one live process and one open pipe per bus event, for
/// as long as opencode runs.
#[test]
fn a_wedged_hook_is_killed_and_the_queue_recovers() {
    if node_is_missing() {
        return;
    }

    let run = Harness {
        hang_first: true,
        settle_ms: 2000,
        ..Harness::default()
    }
    .run(&json!([
        step("session.created", &fixture_record("session.created")),
        step("chat.message", &fixture_record("chat.message")),
    ]));

    // `dispose` capped its wait instead of blocking on the wedged child
    // — but it did wait, which is what makes a release land at all.
    assert!(
        (1_000..5_000).contains(&run.dispose_ms),
        "dispose took {} ms",
        run.dispose_ms
    );

    // The wedged child is gone once node is, rather than outliving it.
    assert_eq!(run.hung.len(), 1, "the stub wedged exactly once");
    assert!(
        wait_until_gone(run.hung[0]),
        "pid {} survived the plugin",
        run.hung[0]
    );

    // And the forwards queued behind it still went out, in order, once
    // the timeout cleared the jam.
    assert_eq!(run.events(), ["chat.message", "dispose"]);
}

/// A hook binary that cannot be spawned fails on every single event.
/// opencode's log is the user's, and its TUI shares the terminal, so the
/// complaint is made once per plugin instance and then never again.
#[test]
fn an_unspawnable_hook_is_reported_exactly_once() {
    if node_is_missing() {
        return;
    }

    let run = Harness {
        missing_hook: true,
        ..Harness::default()
    }
    .run(&json!([
        step("session.created", &fixture_record("session.created")),
        step("chat.message", &fixture_record("chat.message")),
        step("session.status", &json!({ "status": { "type": "busy" } })),
        step("session.idle", &fixture_record("session.idle")),
    ]));

    assert!(run.records.is_empty());
    assert_eq!(
        run.stderr.matches("roost: cannot run").count(),
        1,
        "stderr was:\n{}",
        run.stderr
    );
}
