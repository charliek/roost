# crates/

Rust workspace for the Roost UIs and supporting crates:

- `roost-ipc` — JSON IPC wire format + client/server (shared by both UIs and the CLI).
- `roost-cli` — shell-integration CLI; binary is `roostctl`.
- `roost-linux` — gtk4-rs + libadwaita Linux UI; embeds the workspace + PTY supervisor in-process post-M3.
- `roost-vt` / `roost-osc` — libghostty-vt FFI wrapper + OSC scanner.
- `roost-core`, `roost-proto`, `roost-common`, `roost-smoke` — legacy gRPC daemon and its supports; scheduled for deletion in M7. They remain in the workspace until then so `cargo build` keeps passing on every commit during the refactor.

See [../docs/development/vision.md](../docs/development/vision.md) for the target architecture and phased path.
