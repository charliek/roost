# crates/

Rust workspace for the Roost UIs and supporting crates:

- `roost-ipc` — JSON IPC wire format + client/server (shared by both UIs and the CLI).
- `roost-cli` — shell-integration CLI; binary is `roostctl`.
- `roost-linux` — gtk4-rs + libadwaita Linux UI; embeds the workspace + PTY supervisor in-process.
- `roost-vt` / `roost-osc` — libghostty-vt FFI wrapper + OSC scanner.

The daemon-era crates (`roost-core`, `roost-proto`, `roost-common`,
`roost-smoke`) were removed in the inline-core refactor; the historical
proto schema lives at [`../docs/archive/roost.proto`](../docs/archive/roost.proto).

See [../docs/development/vision.md](../docs/development/vision.md) for the architecture, principles, and decision log.
