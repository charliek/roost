# mac/

Native macOS UI: Swift + AppKit, packaged as a notarized `.app` bundle. Links libghostty-vt directly via the C ABI for in-process VT parse + render. Talks to `roost-core` via `grpc-swift` v2 over a Unix domain socket. Xcode project lands here in Phase 2; UI MVP in Phase 5.

See [../docs/development/vision.md](../docs/development/vision.md) for the target architecture and phased path. Empty for now — Phase 0 placeholder.
