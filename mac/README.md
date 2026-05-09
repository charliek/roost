# mac/

Native macOS UI: Swift + AppKit. Talks to `roost-core` via `grpc-swift` v2
over a Unix domain socket. Will link libghostty-vt directly via the C ABI
for in-process VT parse + render in Phase 6/7. Bundled as a notarized
`.app` in Phase 8.

See [../docs/development/vision.md](../docs/development/vision.md) for the
target architecture and phased path.

## Status

| Phase | Done | What landed |
|---|---|---|
| 2 | ✅ | SwiftPM skeleton + grpc-swift v2 deps declared |
| 5 | 🚧 | AppKit window + status panel (this commit). gRPC client + libghostty-vt + cell renderer land in follow-up commits. |

## Run it

From the repo root, in two terminals:

```bash
# Terminal 1 — run the daemon
cargo run -p roost-core
```

```bash
# Terminal 2 — run the Mac UI
cd mac && swift run Roost
```

Today the window shows the resolved socket path and a placeholder
connection status. The next commit wires the gRPC client and replaces the
status with a live `Identify()` round-trip.

## Build

* Requires Xcode 16+ / Swift 6.0+ on macOS 14+.
* `swift build` resolves dependencies and compiles.
* `swift test` runs the smoke tests under `Tests/RoostTests/`.

CI runs both on `macos-latest` via `.github/workflows/refactor.yml`.

> **Time-boxed CI policy:** the `swift-mac` job currently carries
> `continue-on-error: true`. This is a deliberate exception while
> Phases 5–6 land grpc-swift codegen and libghostty-vt FFI on the Swift
> side; both are expected to surface follow-up commits as the toolchain
> stabilises. The flag is removed at the **Phase 6 exit** milestone
> (Mac feature parity with the current Go binary), at which point the
> job becomes required-green-on-every-commit. Tracked in
> [docs/development/vision.md](../docs/development/vision.md) and
> recorded in `.github/workflows/refactor.yml` next to the flag itself.
