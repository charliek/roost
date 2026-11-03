# Roost mobile

The Flutter client for Roost host sessions (plan 089, milestone M5): Android
first, iOS kept possible, the Linux desktop target for development. It is a
client of `roost-session` like Roost-Iced's host tabs, and adds no ops.

## Dev loop

```bash
make get          # flutter pub get
make run-linux    # the app against FakeCore, no Rust and no host needed
make check        # format check + analyze + tests (what CI will run)
make goldens      # regenerate the painter goldens after a deliberate change
make build-apk    # debug APK (needs the Android SDK)
```

`run-linux` uses the scripted `FakeCore`: a connected `devbox` (two
projects, an agent waiting on a permission prompt, a test run streaming
output), a `mac-mini` that needs its host key trusted, and an unreachable
`nas`. Each tab is a pretend shell: type `help`, `colors`, `echo …`. About
five seconds after you open `cargo nextest`, a pretend desktop takes its
size back, to show the "sized by" banner.

Resize the window across 600 and 840 dp to switch between the compact
(stacked screens), medium (host rail) and expanded (sidebar) layouts.

## Layout

| Path | What |
|---|---|
| `lib/core/roost_core.dart`, `models.dart` | `RoostCore` / `TerminalSession`: the Dart mirror of the Rust `RoostApp` / `TerminalHandle` (plan 089 C5) |
| `lib/core/fake/` | `FakeCore`, its pretend shell and screen (frames via the Dart encoder) |
| `lib/terminal/frame.dart`, `frame_encoder.dart` | The C6 v1 frame payload: decoder, plus the encoder the fake and the tests use |
| `lib/terminal/terminal_model.dart` | Per-row cache the frames update (FULL clears it) |
| `lib/terminal/terminal_painter.dart`, `terminal_view.dart` | `CustomPainter` with per-row cached paragraphs; the view that fits the grid, pulls frames once per vsync, pinches and scrolls |
| `lib/terminal/terminal_pane.dart` | One tab on screen: attach/detach, input routing, toolbar, quick replies, compose, extra keys |
| `lib/input/` | Hardware key mapping, the soft-keyboard `TextInputClient`, extra keys, quick replies, compose box |
| `lib/app/`, `lib/hosts/` | Riverpod providers, the three layouts, host and workspace widgets, dialogs |
| `test/fixtures/` | C6 fixtures; every `frame_v1_*.bin` is decoded against its `.json` |

## Plugging in the Rust core

The UI only sees `RoostCore`, through `coreProvider`. The flutter_rust_bridge
milestone adds, without touching the widgets:

- `rust_builder/` (cargokit, from shed-mobile) and the generated
  `lib/src/rust/`;
- `lib/core/frb_core.dart`: `FrbCore implements RoostCore`, a thin adapter
  over the generated `RoostApp`. It converts the generated view types to the
  models in `models.dart`, JSON-encodes `op` params (the Rust side takes
  `params_json` and returns a JSON string), and wraps `TerminalHandle` as a
  `TerminalSession` whose `events()` is the handle's stream;
- in `main.dart`: `await RustLib.init()`, then `roost_init(dataDir,
  deviceLabel)` and `coreProvider.overrideWithValue(FrbCore(app))` when
  `ROOST_FAKE` is 0.

Contract details the Rust side relies on:

- `KeyInput.key` is a W3C `KeyboardEvent.key` name for named keys (`Enter`,
  `Escape`, `Backspace`, `Tab`, `ArrowUp`, `PageDown`, `Home`, `F1`…) and the
  unshifted character for printable ones (`a`, `1`, `/`); `text` is what the
  key types with modifiers applied, null for control combinations.
  `lib/input/key_mapping.dart` is the one place that builds them.
- `takeFrame` returns the next C6 payload, empty when nothing changed; the
  view calls it at most once per vsync, only after a `FrameReady`.
- `scroll(rows)`: negative is up, into history.
- Geometry cell sizes are device pixels (logical × devicePixelRatio).
- Ids in `op` params are string-wrapped, as on the wire (`"tab_id": "3"`).

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
