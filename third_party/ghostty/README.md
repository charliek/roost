# third_party/ghostty/

Vendored libghostty-vt build. `build.sh` here is the single source of the pinned Ghostty SHA (currently `6ba144158577d033ccb6ed4b574e4013957c9161`). The static library and headers produced are consumed by both the Rust core / Linux UI (`crates/`) and the Swift Mac UI (`mac/`).

See [../../docs/development/vision.md](../../docs/development/vision.md) for the architecture and design rationale.
