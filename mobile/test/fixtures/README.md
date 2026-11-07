# Frame payload fixtures (C6 v1)

`test/terminal/frame_test.dart` decodes every `frame_v1_*.bin` here and
compares it with its `.json` sibling, read through the one tolerant reader in
`test/support/frame_fixture.dart`.

- `frame_v1_handmade.*` were assembled by hand from the C6 layout table
  (Python `struct`, not either encoder), so they pin the byte layout
  independently of the Dart encoder the round-trip tests use.
- `frame_v1_basic.*` and `frame_v1_styles.*` come from the Rust encoder's test
  in `crates/roost-mobile`; dropping them here is all it takes to check the
  two sides agree.
