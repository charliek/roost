# Agent display-name golden fixtures

Canonical `source → display name` corpus for the agents palette row
label (plan 046 §3.7 W7) — `agent_display_name` in
`crates/roost-ui-model/src/agent_palette.rs` (Rust) and
`AgentPalette.agentDisplayName` in `mac/Sources/Roost/AgentPalette.swift`
(Swift). Both ports load this file and assert the same mapping, so the
two UIs cannot drift on spellings like "OpenCode" vs "opencode".

Loaders:

- Rust: `crates/roost-ui-model/tests/agent_display_name_fixtures_test.rs`
  (`cargo test -p roost-ui-model`).
- Swift: `mac/Tests/RoostTests/AgentDisplayNameFixtureTests.swift`
  (`cd mac && swift test`).

Same pattern as [`tests/agent-state-fixtures/`](../agent-state-fixtures/README.md)
and [`tests/word-fixtures/`](../word-fixtures/README.md).

## Format

One JSON object, `cases.json`:

```json
{
  "cases": [
    { "name": "claude", "source": "claude", "expect": "Claude Code" }
  ]
}
```

- `source` is a literal `ownership.source` value, byte for byte.
- `expect` is the exact display name `agent_display_name(source)` must
  return.
- `source` is matched **case-sensitively** against the five adapters'
  own `SOURCE` constants (`crates/roost-agent/src/{claude,codex,
  opencode,grok,cursor}.rs`) — a near-miss casing (`"Claude"`) is an
  unrecognized source, not a fuzzy match, and renders verbatim like any
  other one.
- An unrecognized `source` (including Roost's own internal `manual` /
  `legacy` claims, which this function does not know about — the
  palette's population filter excludes those tabs before this function
  ever sees them) renders **verbatim**: `expect == source`. This is the
  AD-8 guarantee that a sixth, third-party adapter shows up without
  this table being edited.

## Adding a case

Append it to `cases.json`. Every case needs a unique, descriptive
`name`: the loaders report failures by name.
