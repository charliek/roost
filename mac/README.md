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
| 5.1 | ✅ | AppKit window + status panel |
| 5.2 | ✅ | grpc-swift v2 codegen via SwiftPM build plugin + live `Identify()` round-trip from the UI |
| 5.3 | 🚧 | libghostty-vt FFI from Swift (next) |
| 5.4 | 🚧 | Cell renderer (Core Graphics first) |
| 5.5 | 🚧 | `StreamPty` consumed + keystrokes routed |

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

You should see a window come up immediately with the daemon's actual
**pid + version + protocol version** printed in the status panel within
a second or two of launch (the gRPC `Identify()` round-trip happens
asynchronously after the window appears). If the daemon isn't running
the panel turns red and shows the failure reason + a hint to start it.

## Codegen

Swift bindings for `proto/roost.proto` are generated at `swift build`
time by the `GRPCSwiftProtobufGenerator` SwiftPM build plugin from
`grpc-swift-protobuf`. The plugin requires the `.proto` file to live
inside the target source path, so `Sources/Roost/Proto/roost.proto` is
a symlink back to the canonical `proto/roost.proto`. Plugin config is
`Sources/Roost/Proto/grpc-swift-proto-generator-config.json`.

No checked-in generated `.swift` files; no separate codegen step in CI.
Drift between schema and Swift bindings is impossible by construction.

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
