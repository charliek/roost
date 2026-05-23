# Legacy (Go prototype)

The original Roost implementation is a single Go + GTK4 binary built around `libghostty-vt` via cgo. The current Roost has migrated to two native UIs — a Swift `.app` on macOS and a Rust + gtk4-rs binary on Linux — with no daemon; everything in this section describes the retained legacy Go implementation.

Use these docs only if you are running the legacy Go + GTK4 binary rather than the current Swift `Roost.app` (macOS) or the Rust `roost` binary (Linux).

| Page | Covers |
|---|---|
| [Installation](installation.md) | Go + Zig + GTK4 prerequisites, `make libghostty`, `make build` |
| [CLI (`roost-cli`)](cli.md) | The Go companion CLI's command surface, env vars, wire format |
| [Architecture](architecture.md) | Go package layout, GTK4 threading contract, OSC routing |
| [Development setup](development-setup.md) | Iterating on the Go module + Zig libghostty-vt step |

The current Roost development path is documented at the top level of the docs site (Getting Started, Reference, Development). The current CLI is `roostctl` (crate `roost-cli`); see [CLI](../cli.md).
