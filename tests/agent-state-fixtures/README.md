# Agent state golden fixtures

Canonical corpus for the agent state machine defined in plan 002 §3.2 /
§3.3 — `crates/roost-ipc/src/agent.rs` (Rust) and
`mac/Sources/Roost/AgentState.swift` (Swift). Both ports load these
files and assert the same expectations, so drift between the two UIs
surfaces here rather than in a user's sidebar.

Loaders — the corpus is only a parity guard once **both** read it:

- Rust: `crates/roost-ipc/tests/agent_state_fixtures.rs`
  (`cargo test -p roost-ipc`).
- Swift: `mac/Tests/RoostTests/AgentStateFixtureTests.swift`
  (`swift test --package-path mac`), added with the Swift port of the
  state machine.

Same pattern as [`tests/word-fixtures/`](../word-fixtures/README.md) and
[`tests/url-fixtures/`](../url-fixtures/README.md), with one adaptation:
the derivation is wire-shape logic owned by `roost-ipc`, so the Rust
loader lives in that crate rather than in a consumer crate.
[`tests/ipc-vectors/`](../ipc-vectors/README.md) pins the wire *shape*
of the ops; this directory pins the *behavior* behind them.

## Format

One JSON object per file. The top-level `group` field selects the case
schema, so a group may be split across several files and the loader
dispatches on the field, not the filename. Enum values are plain
snake_case strings — no language-specific encodings — and every expected
value is written out explicitly rather than derived.

### `group: "derivation"`

`effective()` / `is_live()` / `suppress_raw_osc()` over the three axes.

```json
{
  "name": "live-owner-failed-projects-to-needs-input",
  "state": { "shell": "unknown", "lifecycle": "failed", "ownership": { … } },
  "expect": { "effective": "needs_input", "is_live": true, "suppress_raw_osc": false }
}
```

- `shell` ∈ `unknown | at_prompt | foreground_process`.
- `lifecycle` ∈ `inactive | working | waiting | finished | failed`.
- `ownership` is `null` or an object (fields below).
- `effective` is a legacy `TabState`: `none | running | needs_input |
  idle`. It is a **closed four-value enum** — `failed` deliberately
  projects onto `needs_input`, and the fixtures pin that.

### `group: "rank"`

```json
{
  "order_high_to_low": ["failed", "waiting", "working", "finished", "inactive"],
  "cases": [ { "name": "failed-is-4", "lifecycle": "failed", "expect": { "rank": 4 } } ]
}
```

The loader asserts each case's exact rank **and** that `rank()` is
strictly decreasing across `order_high_to_low`.

### `group: "transitions"`

`apply_report(current, report, now)`.

```json
{
  "name": "claim-supersedes-a-live-owner",
  "now": 1700001000,
  "current": { "shell": …, "lifecycle": …, "ownership": … },
  "report":  { "tab_id": "3", "source": "manual", … },
  "expect": {
    "accepted": true,
    "ownership_changed": true,
    "lifecycle_changed": true,
    "attention": { "kind": "unchanged" },
    "state": { … },
    "effective": "running"
  }
}
```

- `report` is a literal `tab.agent_report` params object. Required:
  `tab_id` (string-wrapped int64), `source`, `ownership_action`
  (`claim | preserve | release`). Optional, with defaults: `session_id`
  `""`, `lifecycle` **omitted = unchanged**, `lifecycle_if` omitted =
  unconditional, `attention` `preserve`, `severity` `info`,
  `title`/`body`/`detail` `""`, `metadata` `{}`. The op is
  `deny_unknown_fields`, so a fixture may not carry stray keys — in
  particular `last_event_at` is server-stamped from `now` and is not a
  caller-supplied field.
- `shell_mark` (optional, on the **case**, not the report) is an OSC 133
  mark applied to `current` before the report, so a case can state a
  sequence rather than a single step — the prompt-mark failsafe dropping
  the lifecycle, then a `lifecycle_if`-guarded report landing on what it
  left behind. `expect.state` is measured against the post-mark state,
  and an undefined mark is a fixture error rather than a no-op.
- `report.lifecycle_if` gates the `lifecycle` patch and any
  `attention: "set"` on the **current** lifecycle: outside the set both
  are dropped, while `detail`/`metadata` still merge and
  `attention: "clear"` still applies. A vetoed report is still
  `accepted: true` — it matched the owner — and a `release` cannot be
  vetoed at all.
- `expect.attention` is the effect the caller should apply:
  `{"kind":"set","title":…,"body":…,"severity":…}`, `{"kind":"clear"}`,
  or `{"kind":"unchanged"}`.
- `expect.ownership_changed` tracks owner **identity/presence** only. A
  refreshed `last_event_at` or merged metadata is not an ownership
  change — otherwise every accepted report would look like one.
- `accepted: false` means the report was dropped on an ownership
  mismatch; `expect.state` then equals `current` byte for byte.
- Ownership identity is the **pair** `(source, session_id)`. The corpus
  pins both halves independently — a mismatch in either one alone must
  be rejected — so an implementation that compared only `session_id`
  fails here rather than in production.

### `group: "shell_marks"`

`apply_shell_mark(state, body)` — the OSC 133 vocabulary.

```json
{
  "name": "D-drops-lifecycle-but-keeps-the-owner-as-a-label",
  "state": { "shell": …, "lifecycle": …, "ownership": … },
  "body": "D",
  "expect": {
    "changed": true,
    "shell": "at_prompt",
    "lifecycle": "inactive",
    "owner_retained": true
  }
}
```

- `body` is the raw mark payload, so `"D;127"` must behave as `"D"`.
- `changed: false` means the mark is undefined (`""`, `"Z"`) and the
  state is returned untouched.
- `C` writes only the shell axis — an owning agent's lifecycle survives
  a foreground process starting.
- `A`/`B`/`D` also drop the lifecycle to `inactive` while retaining
  ownership. This is the failsafe that stops a killed agent from muting
  a tab forever, so `owner_retained: true` alongside
  `lifecycle: "inactive"` is the point, not an oversight.

### The `ownership` object

```json
{
  "source": "claude",
  "session_id": "s1",
  "last_event_at": 1700000000,
  "detail": "permission_prompt",
  "metadata": { "model": "claude-opus-5" }
}
```

Identity is the **pair** `(source, session_id)`. Ownership is live iff
it is present with a non-empty `source` — there is no TTL (AD-3), so no
fixture may assume one.

## Adding a case

Append it to the file for its group and run both loaders. Every case
needs a unique, descriptive `name`: the loaders report failures by name,
and the name is the only thing a reader of a red CI log sees.
