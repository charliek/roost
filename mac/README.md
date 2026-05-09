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
time by the `GRPCProtobufGenerator` SwiftPM build plugin from
`grpc-swift-protobuf` (depending on `grpc-swift-2`, **not** the legacy
`grpc-swift` v1 package). The plugin requires the `.proto` file to live
inside the target source path, so `Sources/Roost/Proto/roost.proto` is
a symlink back to the canonical `proto/roost.proto`. Plugin config is
`Sources/Roost/Proto/grpc-protobuf-generator-config.json`.

No checked-in generated `.swift` files; no separate codegen step in CI.
Drift between schema and Swift bindings is impossible by construction.

## Build

* Requires Xcode 16+ / Swift 6.0+ on macOS 14+.
* `swift build` resolves dependencies and compiles.
* `swift test` runs the smoke tests under `Tests/RoostTests/`.

CI runs both on `macos-latest` via `.github/workflows/refactor.yml`. The
`swift build` and `swift test` steps in the `swift-mac` job are
**required-green** — any breakage fails the workflow. The earlier
time-boxed `continue-on-error` exception was removed in commit b59644d
after a real package-coordinate bug got hidden by it; preventing
similar drift means keeping the baseline Swift surface blocking from
here on. The Phase 6+ Xcode bundling work may add new steps that
warrant their own scoped exceptions when they land, but `swift build`
+ `swift test` will stay required.
