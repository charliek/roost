# proto/

Single source of truth for the gRPC IPC contract between `roost-core` and
the Mac + Linux UIs. `roost.proto` is consumed by:

* **Rust** — `crates/roost-proto/build.rs` runs `tonic-build` (which
  shells out to `protoc`) at every `cargo build`. Generated bindings
  land in the build target dir; nothing is checked in.
* **Swift (Mac)** — the `GRPCProtobufGenerator` SwiftPM build plugin
  from `grpc-swift-protobuf` (which itself depends on `grpc-swift-2`,
  the v2 line of the package — **not** the legacy `grpc-swift` v1
  URL) regenerates bindings at every `swift build`. The plugin needs
  the `.proto` file to live inside the target's source path, so
  `mac/Sources/Roost/Proto/roost.proto` is a symlink back to
  `proto/roost.proto`. Plugin config sits next to the symlink:
  `mac/Sources/Roost/Proto/grpc-protobuf-generator-config.json`.

Bindings are never checked into VCS — drift between `roost.proto` and a
stale generated file is impossible by construction.

## Schema discipline

* Every change to `roost.proto` lands in `CHANGELOG.md`.
* Pre-1.0: anything goes. Once Phase 5 ships and a real Mac UI depends
  on the schema, evolution is additive only — deprecate fields rather
  than renumber, and bump `roost_proto::PROTOCOL_VERSION` for breaking
  changes.

See [../docs/development/vision.md](../docs/development/vision.md) for
the architectural context.
