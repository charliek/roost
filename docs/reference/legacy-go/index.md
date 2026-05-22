# Legacy (Go prototype)

The original Roost implementation is a single Go + GTK4 binary built around `libghostty-vt` via cgo. It still ships from `main` while the Rust core + native UIs come up to feature parity on the `feature/rust-port` branch; everything in this section describes that legacy implementation and disappears with the Phase 9 cutover.

Use these docs if you are running the binary built from `main` (Go GTK4) rather than the Rust core + Swift `.app` (macOS) or Rust + gtk4-rs binary (Linux) built from `feature/rust-port`.

| Page | Covers |
|---|---|
| [Installation](installation.md) | Go + Zig + GTK4 prerequisites, `make libghostty`, `make build` |
| [CLI (`roost-cli`)](cli.md) | The Go companion CLI's command surface, env vars, wire format |
| [Architecture](architecture.md) | Go package layout, GTK4 threading contract, OSC routing |
| [Development setup](development-setup.md) | Iterating on the Go module + Zig libghostty-vt step |

The current Roost development path is documented at the top level of the docs site (Getting Started, Reference, Development). The Rust-binary CLI is named `roost-cli-rs` during the transition and renames to `roost-cli` in Phase 9 — see [DL-9](../../development/vision.md#dl-9-new-rust-cli-lands-under-a-transitional-binary-name).
