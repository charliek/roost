# Config parser golden fixtures

Canonical corpus for the `config.conf` value semantics pinned in plan
008 §3.1: what `RoostConfig::parse`
(`crates/roost-ui-model/src/config.rs`) makes of each config body.

Loader:

- Rust: `crates/roost-ui-model/tests/config_fixtures_test.rs`
  (`cargo test -p roost-ui-model`).

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
- `expect` pins **parser output for the keys below** — never
  downstream resolution (theme lookup, font matching). A mismatched
  quote pair like `"dark'` stays verbatim and fails theme resolution
  later; that layer is out of scope here.
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
- Keys outside this surface (`link-modifier`, `local-backend`,
  `agent-hooks`, the `macos-*` keys, …) stay in `config.rs`'s own unit
  tests.

## Adding a case

Append it to `values.json` and run the loader. Every case needs a
unique, descriptive `name`: the loader reports failures by name, and
the name is the only thing a reader of a red CI log sees.
