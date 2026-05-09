# third_party/ghostty/

Vendored libghostty-vt build for the new architecture. `build.sh` here pins the same Ghostty SHA as `build/build.sh` (currently `c74f6d56d1feef473033057bc0ff7e3f00cf6421`); both pins move in lockstep until the Phase 9 cutover, after which only this one survives. The static library and headers produced are consumed by both the Rust core / Linux UI (`crates/`) and the Swift Mac UI (`mac/`).

> **Why `third_party/`, not `vendor/`?** Go's tooling treats a top-level `vendor/` directory as the vendored module cache; an unrelated tree under `vendor/` confuses `go build` / `go vet`. Until the Phase 9 cutover removes the Go module, vendored Ghostty source must live elsewhere.

See [../../docs/development/vision.md](../../docs/development/vision.md) for the target architecture and phased path.
