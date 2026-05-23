# linux/

Native Linux UI: Rust + gtk4-rs, packaged as an AppImage (Flatpak optional). Links libghostty-vt via `bindgen` for in-process VT parse + render. Cairo + Pango cell renderer. Talks to `roost-core` via `tonic` over a Unix domain socket. Lands in Phase 7.

See [../docs/development/vision.md](../docs/development/vision.md) for the target architecture and phased path. Empty for now — Phase 0 placeholder.
