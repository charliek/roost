# third_party/ghostty/

Vendored libghostty-vt build. `build.sh` here is the single source of the pinned Ghostty SHA (currently `f2d5758f6305867dc36b36293c6165d8152b853e`). The static library and headers produced are consumed by both the Rust core / Linux UI (`crates/`) and the Swift Mac UI (`mac/`).

See [../../docs/development/vision.md](../../docs/development/vision.md) for the architecture and design rationale.
