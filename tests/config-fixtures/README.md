# Config parser golden fixtures

Canonical corpus for the `config.conf` value semantics pinned in plan
008 §3.1 — `crates/roost-linux/src/config.rs` (Rust) and
`mac/Sources/Roost/Config.swift` (Swift). Both ports load these files
and assert the same expectations, so drift between the two parsers
surfaces here rather than as a theme that only fails on one platform.

Loaders — the corpus is only a parity guard once **both** read it:

- Rust: `crates/roost-linux/src/config_fixture_tests.rs`
  (`cargo test -p roost-linux`). An in-binary `#[cfg(test)]` module,
  not a `tests/` integration test, because the parser is a private
  binary module.
- Swift: `mac/Tests/RoostTests/ConfigFixtureTests.swift`
  (`swift test --package-path mac`).

Same pattern as [`tests/agent-state-fixtures/`](../agent-state-fixtures/README.md).

## Format

One JSON object per file. The top-level `group` field selects the case
schema (`config_values` is the only group today). Each case:

```json
{
  "name": "double-quoted-theme",
  "content": "theme = \"Dracula\"\n",
  "expect": { "theme": "Dracula", … }
}
```

- `content` is a full config-file body; CRLF files spell `\r\n`
  literally in the JSON string.
- `expect` pins **parser output for the shared key surface** — never
  downstream resolution (theme lookup, font matching). A mismatched
  quote pair like `"dark'` stays verbatim and fails theme resolution
  identically on both platforms; that later layer is out of scope here.
- Every `expect` field is written explicitly in every case:
  - `theme`, `font_family`: string or `null` (= unset).
  - `font_size`: number or `null`.
  - `copy_on_select` ∈ `off | true | clipboard`;
    `clipboard_write` ∈ `allow | deny`.
  - `show_sidebar_agents`: boolean.
  - `word_break_chars`: string, or `null` = the default extra-word-char
    set (`""` means an explicit empty override).
  - `keybinds`: `{trigger, action}` pairs in source order, taken from
    the **raw** (quotes-intact) value.
  - `commands`: `{label, run, title, hold, env}` with `env` as
    `[key, value]` pairs; `providers`: `{label, run, title,
    timeout_secs, limit}` — full objects, so the raw-value path is
    proven, not just labels.
- Platform-only keys (`tab-min-width` / `tab-max-width`,
  `link-modifier`'s application) stay in per-side unit tests; the
  corpus pins the shared surface only.

## Adding a case

Append it to `values.json` and run both loaders. Every case needs a
unique, descriptive `name`: the loaders report failures by name, and
the name is the only thing a reader of a red CI log sees.
