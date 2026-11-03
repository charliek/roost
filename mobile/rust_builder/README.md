# roost_mobile_rust

Glue that builds `crates/roost-mobile` into the app with
[cargokit](https://github.com/irondash/cargokit), the way
flutter_rust_bridge's template does. The Dart bindings live in
`../lib/src/rust/` (generated; `make frb-gen`).

Vendored from shed-mobile's `rust_builder/`, with its local patches kept:

- `cargokit/gradle/plugin.gradle`: Gradle 9 removed `Project.exec()`; the
  build task uses the injected `ExecOperations`.
- `cargokit/build_tool/lib/src/builder.dart`: platform builds run the
  toolchain pinned in `rust-toolchain.toml` instead of `stable`. Roost adds
  one more step to that patch: the pin is looked up in the manifest dir and
  its parents, since the workspace's pin is at the repo root.

- `cargokit/build_tool/lib/src/cargo.dart` (Roost): artifacts are named
  after the library cargo builds (`roost-mobile` → `libroost_mobile`), not
  after the package.

Re-apply all three if cargokit is ever re-vendored. Windows is dropped (Roost is
Mac and Linux on the desktop).

Building needs libghostty-vt: `third_party/ghostty/out` for the host, and
`third_party/ghostty/out/<triple>/` for Android (`make mobile-deps`).
