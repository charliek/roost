# Roost mobile

The Flutter client for Roost host sessions (plan 089, milestone M5): Android
first, iOS kept possible, the Linux desktop target for development. It is a
client of `roost-session` like Roost-Iced's host tabs, and adds no ops.

## Dev loop

```bash
make get                     # flutter pub get
make run-linux               # the app with the Rust core (needs libghostty-vt built)
make run-linux-fake          # the app against FakeCore: no Rust, no host
make check                   # format check + analyze + Dart tests
make rust-check              # clippy + tests for crates/roost-mobile
make frb-gen                 # regenerate the bridge after changing src/api/
make frb-check               # the drift guard: regenerating changes nothing
make test-integration-linux  # the whole stack under Xvfb against a real session
make test-integration-android  # the same on an emulator, over SSH to a dev host
make goldens                 # regenerate the painter goldens after a deliberate change
make build-apk               # debug APK (Android SDK + make mobile-deps)
```

`run-linux` keeps its hosts in `$XDG_DATA_HOME/ai.stridelabs.roost_mobile`.
For a dev session, add a host with target `unix:/path/to/roost.sock`.

`run-linux-fake` uses the scripted `FakeCore`: a connected `devbox` (two
projects, an agent waiting on a permission prompt, a test run streaming
output), a `mac-mini` that needs its host key trusted, and an unreachable
`nas`. Each tab is a pretend shell: type `help`, `colors`, `echo …`. About
five seconds after you open `cargo nextest`, a pretend desktop takes its
size back, to show the "sized by" banner.

Resize the window across 600 and 840 dp to switch between the compact
(stacked screens), medium (host rail) and expanded (sidebar) layouts.

`test-integration-linux` starts its own `roost-session` (built by
`cargo build -p roost-session`, or `ROOST_SESSION_BIN`) with private
directories under `/tmp/rm-*` and `ROOST_TEST_MODE=1`, adds it as a `unix:`
host through the UI, types into its tab through the hardware key path, and
checks the pause/resume lifecycle and teardown leak counts. It writes its
own entrypoint into `build/linux/x64/debug/bundle`; rebuild before running
that bundle by hand.

`test-integration-android` runs the same test file on a booted x86_64
emulator (`ANDROID_DEVICE`, default `emulator-5554`). It starts
`tools/mobile/dev-host.sh --port 2299` (a private session behind `dev_sshd`)
and stops it by PID afterwards; the app adds `dev@10.0.2.2:2299`, trusts the
host key it is shown, and drives the tab with hardware keys, the soft
keyboard's input client, the compose box and a quick reply. Give the
emulator room: the debug APK needs a few hundred MB free on `/data`, and a
phone AVD with `hw.keyboard=no` behaves like a real phone (the soft
keyboard comes up).

## Layout

| Path | What |
|---|---|
| `lib/core/roost_core.dart`, `models.dart` | `RoostCore` / `TerminalSession`: the Dart mirror of the Rust `RoostApp` / `TerminalHandle` (plan 089 C5) |
| `lib/core/frb_core.dart` | `FrbCore`: `RoostCore` over the Rust core through the bridge |
| `lib/core/fake/` | `FakeCore`, its pretend shell and screen (frames via the Dart encoder) |
| `lib/src/rust/` | Generated bridge bindings (`make frb-gen`) |
| `lib/terminal/frame.dart`, `frame_encoder.dart` | The C6 v1 frame payload: decoder, plus the encoder the fake and the tests use |
| `lib/terminal/terminal_model.dart` | Per-row cache the frames update (FULL clears it) |
| `lib/terminal/terminal_painter.dart`, `terminal_view.dart` | `CustomPainter` with per-row cached paragraphs; the view that fits the grid, pulls frames once per vsync, pinches and scrolls |
| `lib/terminal/terminal_pane.dart` | One tab on screen: attach/detach, input routing, toolbar, quick replies, compose, extra keys |
| `lib/input/` | Hardware key mapping, the soft-keyboard `TextInputClient`, extra keys, quick replies, compose box |
| `lib/app/`, `lib/hosts/` | Riverpod providers, the three layouts, host and workspace widgets, dialogs |
| `test/fixtures/` | C6 fixtures (every `frame_v1_*.bin` is decoded against its `.json`) and the key-name contract |
| `integration_test/` | The whole stack against a real `roost-session` |

## The Rust core

`crates/roost-mobile` is the core (plan 089 C5). Its `src/api/` is the
flutter_rust_bridge surface: `BridgeApp` and `BridgeTerminal` wrap
`RoostApp` and `TerminalHandle` as opaque Dart objects, their callback
sinks become Dart streams, and the values that cross are flat structs
(`src/api/types.rs`), so no freezed step is needed. `lib/core/frb_core.dart`
adapts them to `RoostCore`.

`rust_builder/` is the cargokit plugin (`roost_mobile_rust`) that builds the
crate into the app; see its README for the local patches.

Which thread a bridge call runs on:

- `#[frb(sync)]`, on the Dart thread, in call order: key, text, paste,
  scroll, resize, take size, close, leak counts. They are channel sends to
  the owner thread; running them in order keeps typed characters in order.
- The bridge's thread pool (a `Future`): frame pulls and `viewport_text`
  (a round trip to the owner thread, about 1.7 ms median in a debug build),
  `app_paused` (waits up to 2 s for size releases), host registry writes,
  `attach`, `roost_init`.
- The bridge's async runtime: `op`, `send_text`, `clear_notification`.

None of them runs on `RoostApp`'s own runtime, which the waiting calls would
block. Dropping the last `BridgeApp` tears `RoostApp` down on a thread of its
own.

Contract details:

- `KeyInput.key` is a W3C `KeyboardEvent.key` name for named keys (`Enter`,
  `Escape`, `Backspace`, `Tab`, `ArrowUp`, `PageDown`, `Home`, `F1`…) and the
  unshifted character for printable ones (`a`, `1`, `/`); `text` is what the
  key types with modifiers applied, null for control combinations.
  `test/fixtures/key_names.json` lists every named key; a Dart test checks
  the UI sends exactly those and a Rust test checks each one encodes.
- `takeFrame` returns the next C6 payload, empty when nothing changed; the
  view calls it at most once per vsync, only after a `FrameReady`.
- `scroll(rows, col, row)`: positive is up, into history, aimed at a cell.
  Once a local scroll settles (500 ms after its last step) on a tab another
  client sizes, the pane calls `takeSize()`, as ipc.md's ownership rules ask.
- Geometry cell sizes are device pixels (logical × devicePixelRatio).
- Ids in `op` params are string-wrapped, as on the wire (`"tab_id": "3"`).
- SSH targets must name the user (`user@host`).

## Soft keyboard notes

The terminal's input connection uses `TextInputType.multiline` with
`enableSuggestions: false` and `autocorrect: false`. On Android that becomes
`TYPE_TEXT_VARIATION_VISIBLE_PASSWORD | NO_SUGGESTIONS | MULTI_LINE`: Samsung
Keyboard and Gboard then commit characters one at a time with no
composition or prediction, and keep an Enter key. The field always holds a
two-space sentinel so Backspace on "nothing" still produces an edit; every
edit is diffed against it (`diffSoftKeyboard`) and the field is reset. A
newline, the IME action, or an Enter key event all become `Enter`. Hardware
keys are handled in `Focus.onKeyEvent` and never reach the IME. Swipe typing
and voice belong in the compose box, which is a normal text field.
