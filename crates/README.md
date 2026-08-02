# crates/

Rust workspace for the Roost UIs and supporting crates:

- `roost-ipc` — JSON IPC wire format + client/server (shared by both UIs and the CLI).
- `roost-agent` — pure agent adapters (Claude Code today): hook event JSON in,
  `tab.agent_report` params out. No I/O, no socket, no clap — the policy is
  unit-testable without a running Roost, and session scoping stays in the
  workspace where the current owner is actually known.
- `roost-cli` — shell-integration CLI; binary is `roostctl`.
- `roost-engine` — toolkit-neutral workspace, persistence, PTY runtime, events, and IPC dispatch.
- `roost-ui-model` — toolkit-neutral config, themes, keybinds, palettes, providers, and projections.
- `roost-linux` — gtk4-rs + libadwaita adapter over the shared Rust engine.
- `roost-vt` / `roost-osc` — libghostty-vt FFI wrapper + OSC scanner.

The daemon-era crates (`roost-core`, `roost-proto`, `roost-common`,
`roost-smoke`) were removed in the inline-core refactor; the historical
proto schema lives at [`../docs/archive/roost.proto`](../docs/archive/roost.proto).

See [../docs/development/vision.md](../docs/development/vision.md) for the architecture, principles, and decision log.
