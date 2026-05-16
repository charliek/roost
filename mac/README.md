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

## Prerequisites

Both the daemon and the Mac UI require `protoc` (the protobuf compiler)
because the gRPC bindings are generated at build time:

```bash
brew install protobuf
```

The Mac UI additionally links libghostty-vt from the vendored Ghostty
build at `third_party/ghostty/out/lib/libghostty-vt.a`. Build it once
before running `swift build`:

```bash
mise install                        # gets zig 0.15.2 if not already installed
./third_party/ghostty/build.sh      # clones Ghostty at the pinned SHA, builds the static lib
```

Subsequent `swift build` runs are fast — the artifacts are cached and
the script no-ops on the next invocation. Re-run the script after a
Ghostty SHA bump (which happens in lockstep on `build/build.sh` and
`third_party/ghostty/build.sh`).

## Run it

From the repo root, in two terminals:

```bash
# Terminal 1 — run the daemon
cargo run -p roost-core
```

```bash
# Terminal 2 — run the Mac UI
cd mac && PROTOC_PATH="$(which protoc)" swift run Roost
```

`PROTOC_PATH` is required because `GRPCProtobufGenerator`'s
`deriveProtocPath` only checks (a) the plugin config's `protocPath`
field, (b) the `PROTOC_PATH` env var, and (c) SwiftPM's `tool(named:)`
registry. System binaries from Homebrew aren't registered as SwiftPM
tools, so the env var is the cleanest portable way to point it at
your local protoc. CI sets the same env var.

If you'd rather not type `PROTOC_PATH` every time, set it in your
shell profile (`export PROTOC_PATH=$(which protoc)`) or pin it in
the plugin config `Sources/Roost/Proto/grpc-swift-proto-generator-config.json`
under the `protocPath` field — note that pinning makes the config
host-specific, which is why we don't do it in the repo by default.

You should see a window come up immediately with the daemon's actual
**pid + version + protocol version** printed in the status panel within
a second or two of launch (the gRPC `Identify()` round-trip happens
asynchronously after the window appears). If the daemon isn't running
the panel turns red and shows the failure reason + a hint to start it.

## Bundle as `Roost.app` (Phase 6a M7)

`swift run` is fine for hot-iteration development but not for Finder /
Dock / Spotlight integration. `mac/scripts/bundle.sh` wraps the
SwiftPM output into a real `.app` bundle.

```bash
./mac/scripts/bundle.sh             # release build -> mac/build/Roost.app
./mac/scripts/bundle.sh debug       # debug build (same layout)
ROOST_VERSION=0.2.0 ./mac/scripts/bundle.sh
open mac/build/Roost.app
```

The script:

* Runs `swift build -c <config> --product Roost`.
* Assembles `mac/build/Roost.app/Contents/{MacOS, Resources, Info.plist, PkgInfo}`.
* Substitutes `@VERSION@` in `mac/Resources/Info.plist.template` with
  `$ROOST_VERSION` (defaults to `0.1.0`).
* Copies the SwiftPM resource bundles (`Roost_Roost.bundle` carries
  the embedded themes; `swift-crypto_*` bundles ship with gRPC SSL
  deps) into `Contents/Resources/`.
* Includes `mac/Resources/AppIcon.icns` if present (M7 doesn't
  ship a real icon — drop one in and rerun).

**Out of scope** (Phase 8 follow-ups, intentional): code-signing with a
Developer ID certificate, notarization via `notarytool`, DMG creation,
Sparkle auto-update feed.

The first launch from Finder will hit the macOS "downloaded from
internet" dialog because the bundle isn't signed; click Open. Inside
a CI run or for installer flows, that dialog is bypassed by
`xattr -dr com.apple.quarantine mac/build/Roost.app`.

## Codegen

Swift bindings for `proto/roost.proto` are generated at `swift build`
time by the `GRPCProtobufGenerator` SwiftPM build plugin from
`grpc-swift-protobuf` (depending on `grpc-swift-2`, **not** the legacy
`grpc-swift` v1 package). The plugin requires the `.proto` file to live
inside the target source path, so `Sources/Roost/Proto/roost.proto` is
a symlink back to the canonical `proto/roost.proto`. Plugin config is
`Sources/Roost/Proto/grpc-swift-proto-generator-config.json` (the
plugin's expected config filename — note the `-swift-` infix; the
SwiftPM target is `GRPCProtobufGenerator` but its config file uses
the older name as a stable convention).

No checked-in generated `.swift` files; no separate codegen step in CI.
Drift between schema and Swift bindings is impossible by construction.

## Build

* Requires Xcode 16+ / Swift 6.0+ on macOS 15+.
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
