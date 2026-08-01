# Iced UI and shared Rust engine proof-of-concept plan

Status: POC proposal, not an accepted replacement architecture

Implementation note: the shared-engine extraction, isolated third-target
contract, and Iced walking skeleton are implemented on `poc/iced`. The walking
skeleton has been exercised on macOS and in the Linux shed under X11 and
Wayland with both wgpu and tiny-skia. Product-parity work remains governed by
the acceptance matrices below; this note does not promote the POC to accepted
replacement architecture.

Branch: `poc/iced`

Reviewed against: repository at `3ffa7fa`, Iced `0.14.0`

## Outcome

This branch will prove two related changes without replacing either shipped UI:

1. a third, PTY-backed `roost-iced` UI that runs on Linux and macOS; and
2. a toolkit-neutral Rust engine shared by GTK and Iced, with a narrow public
   API suitable for a future Swift adapter.

The POC preserves the command-core north star: UI actions and IPC requests
become engine commands, workspace state is authoritative, and UIs render
snapshots plus ordered events. PTY bytes and libghostty-vt stay in the UI
process. There is still no daemon.

This proposal evolves DL-4/DL-5 only for the Rust implementations. The Swift
application remains authoritative for the production macOS experience until a
separate architecture decision approves incremental engine adoption. Nothing
in this POC makes that migration implicit.

## Repository audit

### Current Rust ownership

| Area | Current owner | Classification | Destination |
|---|---|---|---|
| Workspace rows, operations, agent derivation, ordered events | `roost-linux/src/daemon/state.rs` | toolkit-neutral command core | `roost-engine::workspace` |
| `state.json` schema, recovery, atomic replace, backup | `roost-linux/src/daemon/store_json.rs` | toolkit-neutral persistence | `roost-engine::persistence` |
| `portable-pty` spawn/write/resize/reap, per-tab broadcasts | `roost-linux/src/daemon/pty.rs` | Rust runtime/platform adapter, not UI | `roost-engine::pty` |
| Workspace + PTY operation facade and OSC application | `roost-linux/src/local_client.rs` | toolkit-neutral application facade | `roost-engine::application` |
| JSON operation dispatch and request/reply UI port | `roost-linux/src/ipc.rs` | core dispatch plus UI boundary | `roost-engine::ipc` |
| Broadcast lag recovery and startup resync | `roost-linux/src/events.rs` | toolkit-neutral runtime bridge | `roost-engine::events` |
| Full-snapshot reconcile planner | `roost-linux/src/reconcile.rs` | toolkit-neutral projection helper | `roost-engine::reconcile` |
| PTY broadcast-to-UI drain and input command channel | `roost-linux/src/tab_session.rs` | toolkit-neutral terminal lifecycle; VT access remains UI-thread-owned | `roost-engine::session` |
| Single-instance lock | `roost-linux/src/single_instance.rs` | process/platform adapter | initially `roost-engine::instance`; may split if API pressure appears |
| Key translation from GDK and keybind dispatch | `key_encoder.rs`, `keybind.rs` | GTK presentation/input adapter | stays GTK; both adapters use `roost-vt` encoder types |
| Mouse gestures, clipboard, URL launching | `mouse_routing.rs`, `clipboard.rs`, `url_launcher.rs` | UI/native ports | stays per UI; neutral request DTOs move to engine |
| VT cell painting, selection geometry, fonts | `terminal_view.rs` | renderer/UI-loop specific | separate GTK and Iced implementations |
| Theme parsing and color resolution | `theme.rs`, `config.rs` | mixed | pure settings/theme DTOs to `roost-ui-model`; font discovery and native settings stay in adapters |
| Keybind, palette, provider, custom-command, word-selection models | corresponding GTK files | already GTK-free despite package ownership | `roost-ui-model` |
| Git metrics, notification inbox, agent palette/rollup | mixed pure model and platform execution/presentation | split by dependency | pure aggregation/snapshot logic to `roost-ui-model`; subprocess/native presentation stays per UI |

The package boundary is currently misleading: more than 6,000 lines of core,
runtime, IPC, and session code live in a crate that unconditionally links GTK,
libadwaita, Pango, and Cairo. Tests for workspace persistence, PTY supervision,
IPC dispatch, resync, and reconcile therefore require the GTK package even when
they do not require a display.

Focused crates already contain the right reusable contracts:

- `roost-ipc`: serializable command/result/event DTOs, framing, server/client,
  profiles, and target selection;
- `roost-vt`: terminal, render-state, key encoder, and mouse encoder wrappers;
- `roost-osc`: streaming OSC scanner and parsed OSC events;
- `roost-agent`: pure hook-to-agent-report adapter; and
- `roost-url`: URL detection and cleanup.

The engine will depend on these crates rather than re-declaring their wire or VT
types.

### Swift duplication and realistic adoption

| Swift implementation | Rust equivalent | Future engine adoption | Native responsibility retained |
|---|---|---|---|
| `Workspace.swift`, `AgentState.swift` | workspace and `roost-ipc::agent` | strong first slice: commands, persistence, snapshots, events, derivation | `@MainActor` projection and AppKit models |
| `IPCHandlerImpl.swift` | engine IPC dispatcher | strong second slice: shared op validation and mutations | view operations marshal through `UiBridge` |
| `OscScanner.swift` | `roost-osc` | small independent slice after an ABI exists | clipboard/notification handling on `MainActor` |
| `LocalClient.swift` | engine application facade | follows workspace/IPC adoption | AppKit-facing convenience wrapper |
| `PtySupervisor.swift` | engine portable-pty supervisor | optional later slice; higher risk because Darwin `forkpty`, signals, and actor isolation are mature | may remain Swift indefinitely |
| `TabSession.swift` | engine session bridge | split adoption: lifecycle DTOs first, actual PTY drain later | libghostty-vt/AppKit access remains `@MainActor` |
| `KeyEncoder.swift`, `MouseEncoder.swift` | `roost-vt` wrappers | feasible but low duplication payoff; platform event mapping remains native | NSEvent/coordinate/gesture mapping |
| `RenderState.swift` | `roost-vt::RenderState` | serialized render snapshots are possible but not an initial ABI slice | Core Graphics drawing, fonts, selection, IME |
| `RoostBackend.swift`, `IPCServer.swift` | engine composition and `roost-ipc` server | dispatch can move; socket accept may remain Swift during transition | process lifecycle and actor hops |

The initial Swift value is eliminating the most failure-prone duplicated state
machine—workspace, persistence, restoration, agent state, and core IPC dispatch—
without placing AppKit, clipboard, notifications, or libghostty-vt callbacks on
Rust worker threads.

## Crate and ownership boundaries

### `roost-engine`

One engine crate is preferred because workspace mutations, persistence, PTY
lifecycle, and IPC dispatch form one application boundary and already share
concrete types. Artificial `workspace`/`runtime` crates would create public
interfaces before the split has demonstrated pressure. Internal modules retain
clear responsibilities and can become crates later without changing UI code.

`roost-engine` owns:

- synchronous workspace commands and invariant checks;
- immutable serializable snapshots and ordered engine events;
- persistence/restoration semantics;
- agent state application and notification-routing decisions;
- portable-pty supervision and terminal session lifecycle;
- OSC routing into workspace commands and UI effects;
- target-neutral IPC dispatch; and
- lag recovery via an explicit full `Resync` event.

It does not own:

- GTK, Iced, AppKit, renderer objects, window handles, fonts, native menus,
  clipboard access, desktop notifications, URL launching, or process-global
  runtimes;
- libghostty-vt terminal handles or render walkers whose access must remain on
  the UI event loop; or
- presentation state that is not part of the common command contract.

The crate depends on `roost-ipc`, `roost-osc`, `portable-pty`, Tokio primitives,
serde, and small platform crates only. It must not depend on `roost-linux`,
`roost-iced`, GTK, libadwaita, Iced, Pango, or Cairo.

### `roost-ui-model`

The audit justifies one separate toolkit-neutral presentation-model crate. These
modules are already pure but do not belong in the future Swift-facing engine:

- configuration parsing and writeback DTOs;
- theme parsing, palette construction, and terminal color resolution inputs;
- keybind actions, triggers, modifier bitsets, and canonical binding maps;
- palette items, fuzzy matching, navigation, and frame stacks;
- command/provider parsing and invocation DTOs;
- word-selection expansion; and
- pure notification inbox, agent rollup/palette, focus, and git-metric models
  where they can be separated from process execution.

`roost-ui-model` may depend on focused data crates such as `roost-ipc`,
`roost-vt` without `ffi`, and serde. It has no windowing, renderer, clipboard,
notification, subprocess runtime, GTK, or Iced dependency. GTK and Iced share
these models immediately; Swift continues its native mirrors until a separate
adoption decision.

### UI adapters

`roost-linux` retains its binary name `roost`, application id, default GTK
profile, command-line contract, and GTK presentation. Imports of extracted
types change from `roost_linux::daemon` to `roost_engine`; behavior does not.

`roost-iced` is a new library and binary package. It owns the Iced application,
widgets, terminal canvas, model-to-style projection, native clipboard and URL
ports, notification adapter, and translation from Iced input events into
`roost-vt` encoder inputs. It imports `roost-engine` and `roost-ui-model`
directly and never imports `roost-linux`.

## Public engine model

### Commands and results

The Rust-facing API starts concrete and explicit:

```text
EngineCommand (serializable command DTO)
  -> Engine::execute(command)
  -> Result<CommandResult, EngineError>
```

The initial variants correspond to the existing `roost-ipc` operation set:
project create/rename/delete/reorder; tab open/close/focus/title/state/reorder;
agent reports; notification clearing; PTY write/resize; and app shutdown.
Where the existing IPC params/result structs already express a command, the
engine reuses them instead of inventing a parallel schema. Rust UI convenience
methods remain thin wrappers around `execute`.

`EngineError` is typed and stable within Rust. The IPC adapter maps it to the
existing kebab-case protocol codes. Errors are returned to the boundary; the
engine never logs and swallows a failed mutation.

### Snapshots

`EngineSnapshot` contains the full ordered project/tab workspace, active
selection, persisted presentation flags that are genuinely shared, and a
monotonic revision. It owns its data and contains no references or toolkit
objects. A UI can replace its projection from any snapshot.

Terminal render snapshots are not part of `EngineSnapshot`. The authoritative
terminal and render-state handles stay in each UI process on its event loop.
Test-mode dump results use existing `roost-ipc` DTOs through the UI port.

### Events and ordering

Every successful synchronous mutation commits under the workspace lock,
increments a revision, and enqueues its complete ordered event batch before
unlocking. Sending is non-blocking and does not invoke UI code. Persistence runs
after the state lock is released and uses the existing commit sequence to
prevent an older write from replacing a newer one.

Each event carries its revision. Consumers that miss events, receive an
out-of-sequence revision, or start after mutations request `snapshot()` and
replace their projection. The runtime subscription converts broadcast lag into
`EngineEvent::Resync { snapshot }`. No callback is invoked while an engine lock
is held.

The documented compound ordering remains stable, notably:

- project deletion: child `TabClosed` events in display order, then
  `ProjectDeleted`, then any active-selection change;
- mutations that change multiple agent projections: state/hook compatibility
  events before the full agent event, matching current tests;
- reorder event only after positions have committed; and
- persistence snapshot sequence follows the same commit order.

### UI port

View-only or native operations cross one narrow channel:

```text
UiRequest DTO + owned one-shot reply
  Engine IPC dispatcher -> UI event-loop adapter -> UiResponse / EngineError
```

Requests include activate, screenshot, window/sidebar metrics, terminal dump,
resolved-cell dump, selection, clipboard, palette, test PTY injection/capture,
mouse routing, and focus simulation. The DTOs use numeric/string/path/buffer
data only. GTK and Iced each drain the same request enum on their event loop.
The engine never receives a toolkit callback and cannot invoke UI code while
holding state.

## Persistence and restoration contract

The engine preserves DL-7:

- `state.json` persists projects, `next_id`, ordered tab layout
  `{title,cwd,position,user_titled}`, active project/tab position, and shared
  persisted view flags;
- live processes, scrollback, terminal modes, notification state, and agent
  ownership are not persisted;
- writes use temporary file + rename + one backup, write-through without fsync
  during normal mutations, and fsync on clean `flush()`;
- `flush()` freezes subsequent teardown-induced persistence;
- corrupt primary state recovers from the backup when valid;
- position repair remains deterministic; and
- restoration returns descriptors that each UI reopens through the normal tab
  command, producing fresh PTYs.

GTK and Iced use the same engine implementation and tests for these semantics.

## Iced integration and renderer decision

Pin released `iced = 0.14.0`; do not use `master` or a Git revision. The
released crate supports macOS, X11, and Wayland, custom canvas widgets, runtime
event subscriptions, window screenshots, and both `wgpu` and `tiny-skia`
backends. Sources consulted:

- <https://docs.rs/iced/0.14.0/iced/application/>
- <https://docs.rs/iced/0.14.0/iced/widget/canvas/>
- <https://docs.rs/iced/0.14.0/iced/window/fn.screenshot.html>
- <https://docs.rs/crate/iced/0.14.0/features>

The POC enables `canvas`, `wgpu`, `tiny-skia`, `x11`, and `wayland`. Production
default is Iced's `Best`: wgpu on Metal/Vulkan when available, with tiny-skia as
the software fallback. CI runs an explicit software lane via
`ICED_BACKEND=tiny-skia` to remove GPU-driver nondeterminism, plus at least one
wgpu smoke where the runner supports it.

The terminal is an Iced `Canvas` program, not thousands of text widgets. On
each redraw it snapshots the active `roost-vt::RenderState`, resolves theme and
cell colors, clips to the terminal bounds, fills backgrounds, draws glyph
clusters at measured cell origins, applies supported bold/italic/underline
styles, draws selection, then draws the cursor. The terminal handle and render
walk stay on Iced's update/draw thread. PTY readers emit owned byte messages;
the UI applies `vt_write` and schedules redraw.

The walking skeleton must prove:

- canvas glyph metrics are stable for ASCII, wide, and combined characters;
- foreground/background/inverse/style resolution matches GTK;
- cursor shapes and clipping work in both backends;
- resize quantizes pixels to rows/columns and resizes both VT and PTY;
- window screenshots return RGBA bytes that can be encoded as PNG for IPC; and
- keyboard subscriptions and canvas pointer events cover terminal input,
  shortcuts, selection, wheel/scrollback, hyperlinks, and mouse reporting.

If canvas text cannot maintain cell alignment for required grapheme cases, the
rollback is a small custom Iced widget that emits backend text/quad primitives;
the engine and third-target work remain valid. A renderer failure does not
justify depending on GTK or copying its workspace core.

## Third target, profiles, and paths

Add `BundleProfileKind::Iced` with:

| Host | Socket/lock | State | Log | App id |
|---|---|---|---|---|
| macOS | `~/Library/Caches/Roost-iced/{roost.sock,roost.lock}` | `~/Library/Application Support/Roost-iced/state.json` | `~/Library/Logs/Roost-iced/roost.log` | `ai.stridelabs.Roost.iced` |
| Linux | `$XDG_RUNTIME_DIR/roost-iced/{roost.sock,roost.lock}` or `/tmp/roost-iced-<uid>/...` | `$XDG_DATA_HOME/roost-iced/state.json` | `$XDG_STATE_HOME/roost-iced/roost.log` | `ai.stridelabs.Roost.iced` |

Unlike today's Mac/Gtk collapse on Linux, Iced remains distinct there so the
POC can coexist with production GTK. `ROOST_STATE_DIR` continues to override
only state. `ROOST_BUNDLE_PROFILE=iced`, `roostctl --target iced`, and explicit
`--socket` use the same precedence rules as existing targets.

The Swift `BundleProfile` mirror gains an `iced` case and path tests even though
the production Swift binary still selects `mac`; this keeps path documentation,
Rust CLI resolution, and cross-language fixtures in lockstep.

Auto-detection probes all three profiles. Zero live sockets reports every
candidate. One selects it. Two or three produce an ambiguity error naming the
live candidates and instructing `--target mac|gtk|iced`. `doctor` uses the same
resolved candidate data instead of hard-coded two-target prose.

## Functional harness and visual strategy

`tools/roosttest` gains a capability record per target rather than target-name
conditionals. Each record defines binary/app launch, socket, state/config
isolation, captured log, shutdown, platform availability, GTK critical scan,
and any narrowly justified native capability. `iced` launches
`target/debug/roost-iced`, captures stdout/stderr, and uses its distinct profile.

All target-neutral tests run against Iced. Tests that exercise GTK-specific
critical logging or AppKit defaults select capabilities explicitly. Mid-test
relaunch reuses the harness state directory. A coexistence test launches GTK
and Iced together on Linux and all three UIs on macOS where a GUI session is
available, then identifies each socket and confirms state isolation.

Screenshot tooling accepts `iced`, seeds the same workspace and deterministic
config, captures GTK and Iced through `app.screenshot`, and records both images.
Stable assertions compare selected geometry and colors:

- sidebar and terminal rectangles and sidebar proportion;
- active row/pill fill and status-dot color;
- terminal padding and default background;
- tab band height and close-control bounds; and
- cursor cell geometry.

Full-window pixel equality is intentionally not an acceptance test because
toolkit glyph rasterization and platform chrome differ.

## Parity matrix

| Capability | GTK reference | Iced acceptance | Test layer |
|---|---|---|---|
| Projects: create/select/rename/reorder/delete | required | required | engine unit + E2E |
| Tabs: open/select/rename/reorder/close/restore | required | required | engine/PTY + E2E |
| PTY shell, command, resize, exit/reap | required | required | integration + E2E |
| Cell colors/styles/cursor/wide/combined/reflow | required | required | VT fixtures + screenshot geometry/color |
| Scrollback | required | required | unit + real input |
| Keyboard encoding and configured shortcuts | required | required | encoder fixtures + real input |
| Selection/copy/paste/URL/mouse reporting | common contract | required | E2E test ops + real input |
| Themes/fonts/color resolution | common contract | required | unit + resolved-cell E2E |
| Agent lifecycle, rollups, native notification | common contract | required | engine + E2E; adapter smoke |
| IPC identify/list/dump/screenshot/test ops/shutdown | common contract | required | target-neutral E2E |
| Palette/sidebar | common contract | required | model tests + E2E + screenshot |
| GTK-only CSS/transitions/native widget flourish | optional | may differ | listed individually in final gaps |

No missing common behavior is hidden by a target-wide skip.

## Platform and backend test matrix

| Host/display/backend | Build | Unit/integration | Functional | Visual/input |
|---|---:|---:|---:|---:|
| macOS/Metal wgpu | yes | yes | full target-neutral suite | launch, screenshot, keyboard/resize/coexistence |
| macOS/tiny-skia | yes | targeted | smoke | screenshot |
| Ubuntu/X11 wgpu or runner fallback | yes | yes | full suite under Xvfb | screenshot + XTEST |
| Ubuntu/X11 tiny-skia | yes | targeted | full deterministic CI lane | screenshot |
| Ubuntu/Wayland/wgpu | yes | targeted | full suite under headless Weston | pointer/clipboard where compositor supports it |
| Ubuntu/Wayland tiny-skia | yes | targeted | startup and terminal smoke | renderer diagnostics |

The shed is the local Linux authority. Its build script keeps Cargo and Ghostty
outputs in guest-local paths, builds `roost-engine`, GTK, Iced, and `roostctl`,
then provides explicit X11 and Wayland Iced gates. The existing cage/uinput GTK
guard remains unchanged. The shed is stopped after final validation.

## CI and Make changes

Make targets:

- `build-iced`, `run-iced`, `test-iced`;
- `e2e-iced`, `e2e-iced-ci`, `smoke-iced`;
- `check-iced` for format/clippy/engine/Iced/dependency boundaries; and
- existing targets keep their names and behavior.

CI push triggers include `poc/iced`. Path filtering gains explicit engine and
Iced outputs. Required work at HEAD includes:

- format and clippy with warnings denied for engine/Iced;
- engine tests on Ubuntu and macOS;
- GTK build/test/E2E regression after extraction;
- Iced build/test on Ubuntu and macOS;
- dependency-boundary scripts using `cargo metadata`/`cargo tree`;
- Iced X11 and Wayland functional lanes and macOS functional lane;
- failure uploads for logs, JUnit, and screenshots;
- existing Swift unit/build/E2E lanes; and
- `ci-success` updated so new required jobs cannot be silently omitted.

## Swift interoperability decision

Decision for this POC: defer unsafe FFI implementation. The Iced application
and engine extraction already exercise the public API directly; adding a static
library, C header generation, Swift build integration, allocator boundary, and
leak instrumentation in the same branch would expand the risk surface without
improving the Iced proof. The engine API must nonetheless support this concrete
boundary without a rewrite.

Proposed ABI v1:

```c
typedef struct roost_engine roost_engine_t;       /* opaque owned handle */
typedef struct { uint8_t *ptr; size_t len; } roost_owned_bytes_t;

uint32_t roost_engine_abi_version(void);
int32_t roost_engine_create(const uint8_t *options_json, size_t len,
                            roost_engine_t **out);
void roost_engine_free(roost_engine_t *engine);
int32_t roost_engine_execute(roost_engine_t *engine,
                             const uint8_t *command_json, size_t len,
                             roost_owned_bytes_t *result_json);
int32_t roost_engine_snapshot(roost_engine_t *engine,
                              roost_owned_bytes_t *snapshot_json);
int32_t roost_engine_poll_event(roost_engine_t *engine,
                                roost_owned_bytes_t *event_json);
int32_t roost_engine_last_error(roost_engine_t *engine,
                                roost_owned_bytes_t *error_json);
void roost_bytes_free(roost_owned_bytes_t bytes);
```

Rules:

- all handles and buffers have explicit create/free ownership; no borrowed
  pointers or Rust layouts cross the boundary;
- command/snapshot/event/error DTOs are versioned JSON using `roost-ipc` shapes
  where practical; ABI status codes are numeric and details are retrieved;
- every exported function catches panics; no unwinding crosses C;
- null, invalid UTF-8/JSON, unknown command, stale handle, and double-free-safe
  wrapper behavior are deterministic and tested;
- v1 uses polling, not callbacks. Swift polls from a task and dispatches owned
  event data to `MainActor`; Rust never calls AppKit or Swift libghostty-vt
  objects from a worker thread;
- a later callback API must document callback thread, reentrancy, cancellation,
  and lifetime before it is added; and
- PTY and UI ports remain opt-in capabilities. Swift can first create an engine
  with workspace/persistence only and retain its native supervisor/rendering.

The eventual smoke test must cover create/free, project creation, snapshot,
ordered event polling, invalid command/error retrieval, and thousands of
repeated create/free cycles under sanitizers or Instruments. That is a separate
reviewed slice, not a hidden requirement of the Iced renderer.

## Milestones and commit sequence

Each commit gets a reviewed mini-plan, complete diff review, full applicable
gate, push, and Actions verification before the next slice.

1. **Plan and audit**: land this proposal only. Rollback: documentation revert.
2. **Engine and shared-model extraction**: create `roost-engine` and
   `roost-ui-model`, move core/runtime and pure UI-model modules with their
   tests, migrate GTK imports, prove behavior and dependency boundaries.
   Preserve the current concrete Workspace/Application methods first, then add
   the command/snapshot/event facade as an additive API in the same green slice.
   Rollback: GTK can be mechanically pointed back before new API consumers land.
3. **Third-target contract**: profiles, path separation, selector/doctor/harness
   capability model, and unit tests. No Iced dependency yet.
4. **Iced walking skeleton**: pinned Iced, native window, real workspace/PTy,
   canvas VT output, keyboard, resize, IPC identify/list/dump, launch smoke. From
   this commit onward `roost-iced` remains launchable.
5. **Terminal interaction parity**: style/cursor/wide/combined/reflow,
   scrollback, selection/clipboard/paste/URL/mouse reporting, themes/fonts.
6. **Product surface parity**: project/sidebar/tab interactions, agent states,
   notifications, palette, persistence/relaunch, screenshot/test ops.
7. **Harness, visuals, CI, and shed**: target-neutral Iced E2E, coexistence,
   X11/Wayland/macOS matrix, artifacts, Make/docs, final regression gates.
8. **Final review fixes**: architecture/dependency/diff review, no feature scope;
   push only after local and shed gates are green.

## Per-commit gate

Before every commit:

1. record scope, invariants, interfaces, tests, and acceptance;
2. apply the plan-review checklist to repository evidence;
3. implement the coherent slice only;
4. run `cargo fmt --all -- --check`, warnings-denied clippy, affected unit and
   integration tests, Swift tests, GTK build/tests, and affected functional
   suites;
5. use the shed for affected Linux/GTK/Iced/X11/Wayland gates, recording any
   same-environment baseline rerun for known flakes;
6. review the full diff for locking, ordering, ownership, error handling,
   toolkit boundaries, and generated files;
7. rerun the complete slice gate;
8. inspect status and diff, create a focused conventional commit, push
   `poc/iced`, and wait for GitHub Actions; and
9. fix failures with additive commits—never rewrite published history.

### Active mini-plan: shared OSC routing and Iced shell-state parity

Scope: introduce a toolkit-neutral, per-terminal OSC router in `roost-engine`,
migrate the GTK drain to it, and use it from Iced for PTY output and test-fed
bytes. The router owns streaming scan state, ordered workspace actions, color
and palette query replies, and explicit clipboard/pointer UI effects. It does
not touch a toolkit, terminal renderer, clipboard, or workspace lock.

Invariants and interfaces:

- one router is owned per live terminal, so split OSC sequences cannot cross
  tabs and action order matches byte order;
- callers provide an owned live-color snapshot and receive an ordered
  `Vec<OscAction>`; no UI callback crosses the engine boundary;
- workspace actions continue through `LocalClient::apply_osc`, while clipboard
  and pointer actions remain narrow UI ports and PTY replies use the tab's
  serial input channel;
- GTK behavior and its OS clipboard/cursor adapters remain unchanged; and
- Iced refreshes render snapshots after injected bytes so a subsequent IPC
  resolved-cell dump cannot observe a stale frame.

Acceptance: engine router unit tests cover split input, event/action ordering,
workspace mapping, and color/palette replies; Iced passes
`test_osc_pipeline.py`, shell/agent OSC state tests, OSC 52's deterministic
test port, and OSC 22 cursor assertions; GTK's focused OSC suites and full
regression gate remain green; dependency-boundary checks remain unchanged.
Mouse encoding, selection geometry, the visible palette, native clipboard
readback, and desktop notification presentation are explicitly outside this
commit and retain their focused failing tests for the next slices.

### Active mini-plan: Iced mouse reporting and terminal focus

Scope: add a per-terminal Iced mouse encoder backed by the existing
`roost-vt`/libghostty protocol implementation, route both Canvas pointer events
and the synthetic IPC port through one cell-coordinate method, and emit
mode-1004 focus reports for both native Iced window events and the synthetic
focus port. Extract the 60 Hz/per-cell motion throttle from the GTK-named module
into `roost-engine::pointer`, with GTK consuming the new owner through a
compatibility re-export so there is only one Rust throttle state machine.

Invariants and interfaces:

- each live Iced terminal owns its `MouseEncoder` and `MotionEmitter`; encoder
  format/mode state is synchronized from that tab's `Terminal` immediately
  before every attempted report;
- the adapter accepts toolkit-neutral `PointerAction`/`PointerButton`, cell
  coordinates, and modifier bits, then maps to libghostty surface coordinates;
  no Iced or GTK type enters the engine or `roost-vt`;
- button motion is never throttled; only mode-1003 motion without a button uses
  the shared 60 Hz/per-cell gate, and the gate commits only after libghostty
  emits non-empty bytes;
- native Canvas events and `tab.dispatch_mouse_event` call the same Iced method,
  while native window focus and `app.set_window_focus` call the same focus
  method; mode-disabled events remain silent; and
- pointer bytes go through `TabSession::send_input`, preserving capture order
  and avoiding UI callbacks or workspace locks.

Acceptance: shared throttle unit tests remain deterministic; all ten
`test_mouse_tracking.py` cases pass against Iced (including the already-green
OSC 22 cases); a focused native-event unit test covers cell mapping/button
state; GTK's mouse-routing unit/E2E suite and full regression suite remain
green; macOS plus shed X11/Wayland renderer lanes pass; dependency boundaries
remain unchanged. Selection anchoring, copy/paste, URL hit-testing, local
scrollback, and platform cursor presentation are separate later slices—the
Canvas forwards mouse-tracking protocol now but deliberately does not pretend
those local interactions are complete.

### Active mini-plan: shared terminal selection and Iced drag selection

Scope: move renderer-independent terminal selection state, screen/viewport
coordinate conversion, visible highlight spans, and selected-text extraction
from the GTK view into `roost-vt::TerminalSelection`. Migrate GTK to that type,
then use the same type from Iced for selection IPC, word/line expansion via the
existing `roost-ui-model::word_selection` policy, visible Canvas highlights,
and native left-button drag selection when terminal mouse reporting is off.

Invariants and interfaces:

- selection endpoints are stored as libghostty screen coordinates so output and
  scrollback never retarget a selection to unrelated viewport text;
- `TerminalSelection` exposes explicit set/begin/update/clear, snapshot, visible
  spans, row text, and selected text operations; it owns no GTK, Iced, clipboard,
  callback, or renderer object;
- text/render walks accept the caller-owned `Terminal` and `RenderState`, return
  explicit `roost-vt::Result`, and never retain borrowed grid references;
- GTK retains its existing gesture precedence, copy-on-select ports, paint
  colors, and public behavior while no longer owning the selection state
  machine; `roost-ui-model::word_selection` remains the single word policy;
- Iced pointer reporting retains precedence while a tracking press is active;
  otherwise left press/motion/release mutate only the active tab's selection;
  tab switches cannot leak gesture or selection state; and
- selection paint is an adapter concern: the model returns normalized visible
  cell spans and each renderer chooses its own color/alpha.

Acceptance: shared model tests cover normalization, committed single cells,
multi-row column ranges, screen-stable coordinates, clipping, extraction, and
clear/update failure; Iced passes all `test_selection.py` selection assertions
and `test_word_selection.py`; native Canvas tests cover local drag messages and
tracking precedence; GTK selection/word tests and full regressions stay green;
macOS and shed X11/Wayland focused gates pass; dependency boundaries remain
unchanged. Native CLIPBOARD/PRIMARY read/write tasks, copy/paste shortcuts,
middle-click paste, URL hit-testing, and double-click timing in the Iced Canvas
are the next clipboard/input slice. Existing deterministic clipboard shadows
remain explicit test ports in this commit and are not described as OS-native.

### Active mini-plan: Iced command palette and launcher adapter

Scope: add one tab-independent Iced palette session backed by
`roost-ui-model::palette::PaletteState`. The visible overlay, native keyboard
and pointer actions, `palette.open/state/query/activate/dismiss`, bundled-theme
and font drill-ins, and configured command launcher all read and mutate that
same session. Launcher confirmation uses `LocalClient::open_tab`, preserving
the engine-owned workspace/PTY operation path.

Invariants and interfaces:

- Iced owns only presentation/session adaptation; fuzzy matching, query and
  selection transitions, frames, command ids, launcher items, and shell argv
  construction remain toolkit-neutral `roost-ui-model` APIs;
- palette input has priority over terminal input while open, dismiss/confirm
  resolves exactly once, and IPC plus native buttons call the same methods;
- command activation dispatches through existing App/workspace operations;
  unsupported rename dialogs do not gain shadow state or false-success paths;
- theme preview applies to every live `TerminalTab`, refreshes libghostty
  colors, emits DEC 2031 reports through `TabSession`, and dismissal restores
  the theme that was active when the palette opened;
- launcher commands come from the same `ROOST_CONFIG` parser as GTK, inherit
  the active workspace cwd, and launch through the engine supervisor;
- the engine's native cwd fallback uses `/proc` on Linux and libproc on
  macOS, matching the existing Swift behavior without moving process
  inspection into either UI adapter; and
- no palette callback, Iced widget, or renderer object enters `roost-engine`,
  `roost-ui-model`, or a workspace lock.

Acceptance: Iced unit tests cover closed/open snapshots, query ranking,
theme drill-in/preview restoration, and launcher id resolution; the common
`test_palette.py`, `test_launcher.py`, and mode-2031 theme-switch assertion
pass against Iced; native overlay actions exercise the same session methods;
GTK/Swift regressions stay green; macOS and shed X11/Wayland renderer lanes
pass; dependency boundaries remain unchanged. Agent palettes and git metrics,
notification inbox commands, provider-driven `palette.present`, rename
dialogs, and persistence of an explicitly confirmed theme/font are named
follow-up slices rather than parallel state hidden in this commit.

Hosted-CI review follow-up: the IPC socket can answer during the short interval
before Iced publishes its first `WindowOpened` event. An early `window.resize`
must update logical terminal geometry immediately and retain one pending native
resize for delivery when the window id arrives; startup readiness must not
depend on renderer event timing. The walking-skeleton resize/device-reply test
is the regression gate on both Linux renderer lanes and macOS.

### Active mini-plan: shared agent switcher and git metrics

Scope: make the Iced palette consume the existing toolkit-neutral agent-row
projection and move the asynchronous git-metrics service out of the GTK package
into `roost-engine`. GTK keeps its current presentation and main-context hop;
Iced adds a visible agent row, direct `palette.open kind="agents"`, the command
palette drill-in, live authoritative refresh, and tab activation through the
workspace operation path.

Invariants and interfaces:

- `roost-ui-model::agent_palette` remains the only owner of population,
  lifecycle derivation, row text, ordering, elapsed time, and row-id parsing;
- `roost-engine::git_metrics` owns process execution, timeouts, concurrency,
  parsing, deduplication, and session cache data without GTK, Iced, renderer, or
  callback dependencies; GTK imports it from the engine rather than retaining a
  compatibility copy;
- each open Iced palette receives a monotonically increasing session id; git
  work returns through an owned channel, and results are ingested only when the
  current session matches, so dismiss/reopen cannot flash or apply stale values;
- every Iced tick resyncs the open agents frame from `Workspace::snapshot`,
  preserving query and selected row by id through `PaletteState::update_items`;
  a slow UI therefore recovers without relying on a complete delta stream;
- non-repository, missing-cwd, timeout, and task failures become the explicit
  em-dash value after logging; a pending row stays `None`, never a false zero;
- activating `agent:<tab-id>` focuses through `Workspace::focus_tab` and closes
  the palette; the empty sentinel is a successful no-op that remains open, and
  a row whose tab vanished returns the existing not-found contract without a
  crash; and
- Iced renders the serialized agent payload (project, name, lifecycle/status,
  elapsed time, and metrics) without creating a second agent state machine.

Acceptance: all agent-palette model and extracted git-metrics unit tests pass;
GTK compiles against `roost-engine::git_metrics` and its full regression suite
stays green; every target-neutral case in `test_agent_palette.py` passes against
Iced, including live lifecycle/project refresh, effective lifecycle, activation,
command drill-in, real hook routing, same-root metrics reuse, and non-repo error
resolution; macOS plus shed X11/Wayland focused gates pass; dependency trees
remain toolkit-clean. Sidebar agent rows, native notification presentation,
notification/provider palettes, and sidebar collapse/reveal behavior are
separate slices and do not gain shadow state here.

### Active mini-plan: Iced sidebar state and agent rows

Scope: make the Iced sidebar a live projection of the authoritative workspace,
including per-project agent rows, active-row styling, collapse/reveal, the
`show-sidebar-agents` toggle, and `app.sidebar_dump`. The existing engine
`sidebar_collapsed` field remains the persisted owner; the existing shared
config parser/writer remains the owner of the agent-row visibility preference.

Invariants and interfaces:

- project and tab membership, ordering, focus, lifecycle, and row text come
  from `Workspace::snapshot` plus `roost-ui-model::agent_palette::sidebar_agents`;
  Iced owns only widget composition and a last-rendered projection cache;
- `app.sidebar_dump` combines fresh snapshot membership with the same
  last-rendered rows the Iced view consumes, includes every project (including
  one created before its first UI reconcile), and marks at most the one
  engine-active tab;
- `toggle_sidebar` calls `Workspace::set_sidebar_collapsed`, so persistence and
  restoration stay engine-owned; collapsing changes terminal geometry and
  `window.metrics` immediately without a second boolean cache;
- `toggle_sidebar_agents` mutates the loaded `RoostConfig` value and writes it
  through `roost-ui-model::config::set_key`; a failed write is visible in the
  status area and leaves the deliberate live-session value intact;
- project, tab, and agent buttons dispatch the existing workspace focus
  operations; a successful cross-project agent jump reveals the sidebar, while
  a vanished tab cannot alter collapse state; and
- lifecycle colors use the shipped GTK/AppKit constants; no agent ranking,
  state derivation, persistence, or event callback is duplicated in Iced.

Acceptance: all `test_sidebar_agents.py` and
`test_sidebar_collapse_persistence.py` cases pass against Iced; focused agent
palette tests remain green; unit tests pin collapsed width and lifecycle color
constants; GTK and Mac functional regressions stay green; macOS and shed
X11/Wayland sidebar/agent gates pass; dependency boundaries remain unchanged.
Pixel capture and geometry/color assertions remain explicitly assigned to the
following screenshot/visual slice because `app.screenshot` is still an honest
unsupported Iced UI port in this commit. Notifications and provider palettes
also remain separate coherent slices.

### Active mini-plan: shared notification inbox and Iced triage

Scope: make GTK and Iced consume one toolkit-neutral notification palette
projection, then connect Iced to the engine's ordered notification events and
authoritative snapshot. Iced gains the inbox drill-in, unread jump, clear-all,
per-tab attention marker, and a native inbox button. Desktop banner delivery
remains a separate native UI port rather than entering the engine or shared
model.

Invariants and interfaces:

- `Workspace` remains the sole owner of focus suppression and the
  `has_notification` bit; the Iced adapter never re-derives policy from window
  or tab state after the event;
- `roost-ui-model::notification_inbox` owns deduplication, cap, ordering, row
  ids, frame construction, command rows, and active-project-first unread
  selection; GTK migrates its matching helpers onto those APIs in the same
  commit;
- Iced subscribes before hydration and drains `WorkspaceEvent` without calling
  UI code from a runtime worker. `NotificationFired` supplies the ephemeral
  body while `TabNotification(false)` and close/delete edges remove rows;
- each Iced tick reconciles membership to a full workspace snapshot. A lagged
  consumer drops stale rows and deterministically reconstructs missing pending
  rows without bodies, because notification content is intentionally not
  persisted;
- tab, agent, notification, and unread navigation all focus through the
  workspace, clear the authoritative bit, and reveal the sidebar only after a
  successful focus; and
- clearing iterates the inbox and sends one workspace clear per row. Row
  removal follows the resulting false-edge rather than a competing local
  mutation.

Acceptance: the shared-model tests cover dynamic command count, empty and
populated frames, row parsing, newest/active-project unread selection, and cap;
all five `test_notifications.py` cases pass against Iced; existing palette,
agent, sidebar, GTK, and Swift regressions stay green; macOS plus shed
X11/wgpu and Wayland/tiny-skia focused gates pass; dependency boundaries remain
toolkit-clean. Native OS notification banners are deferred because they require
a platform-specific Iced notification port and are not needed to validate the
shared state/event architecture; the visible inbox, markers, and triage actions
remain fully usable.

### Active mini-plan: shared provider runtime and deferred palettes

Scope: extract provider subprocess execution from GTK into a generic bounded
process service in `roost-engine`, migrate GTK to it, and implement Iced's
provider list/activate drill-ins plus the blocking `palette.present` IPC port.
Provider parsing, invocation DTO construction, output parsing, and palette-row
projection remain in `roost-ui-model`.

Invariants and interfaces:

- `roost-engine::process` accepts owned argv/env/stdin/cwd/timeout data and
  returns owned stdout or a typed error; it has no provider, palette, GTK, or
  Iced dependency and always uses kill-on-drop for timeout/cancellation;
- GTK and Iced build the same provider invocation through
  `roost-ui-model::provider`, call the shared process service off their UI
  loops, and parse the phase-specific output through the same model functions;
- every Iced provider request carries the current palette session and a
  monotonically increasing request generation. Dismiss/reopen, a newer run,
  or shutdown makes late results inert;
- provider frames preserve the model's actionable flag, row limit, error row,
  placeholder, environment, stdin JSON, active-tab context, sibling
  `roostctl`, and list-versus-activate semantics;
- `palette.present` stores one owned reply sender in the Iced adapter. Pick,
  dismiss, or replacement takes it exactly once, and no workspace/UI lock is
  held while replying; and
- the custom and agent default shortcuts open their existing frames without
  leaking the keystroke into the PTY. Unsupported rename or screenshot ports
  are unaffected.

Acceptance: engine process tests cover stdout/stdin/env/cwd, nonzero exit,
timeout, empty argv, and sibling executable resolution; GTK's provider and full
functional suites remain green; all four `test_provider.py` cases pass against
Iced on macOS, shed X11/wgpu, and shed Wayland/tiny-skia; the provider suite is
added to the required Iced Make/Actions gate; full Iced has no functional
failure other than the explicitly separate screenshot pixel port; Swift and
dependency-boundary gates remain green.

## Objective acceptance criteria

- `poc/iced` HEAD is pushed with green required Actions and no PR or package.
- `roost-engine` is used by GTK and Iced and has no UI/renderer dependencies;
  `roost-ui-model` contains shared non-rendering UI semantics and no toolkit.
- Cargo metadata proves Iced has no GTK/libadwaita/Pango/Cairo dependency and
  neither UI depends on the other.
- GTK retains its binary/launch behavior and existing Rust/functional tests.
- `roost-iced` launches on macOS, X11, and Wayland, uses a real PTY and
  libghostty-vt terminal, accepts input, resizes, and implements the common UI
  and IPC contract.
- Mac, GTK, and Iced profiles have distinct socket/lock/state/log locations and
  coexist on macOS; GTK and Iced coexist on Linux.
- `roostctl --target iced`, harness relaunch/persistence, logs, startup error
  gate, screenshots, and target-neutral E2E pass.
- required terminal rendering/input/clipboard/mouse/agent behavior has focused
  tests; any optional GTK-only difference is named with user impact.
- existing Swift build/test/E2E and GTK build/test/E2E are green.
- the shed validates engine, GTK regression, Iced X11/Wayland, relevant input,
  and artifact isolation, then is stopped.
- documentation gives exact build/run/test commands, paths, prerequisites,
  renderer requirements/risks, coexistence instructions, and the deferred FFI
  ownership/threading/test design.

## Risks and rollback points

| Risk | Mitigation | Rollback |
|---|---|---|
| Mechanical extraction changes GTK behavior | move code/tests first, preserve concrete API, types, and event order before layering the new facade; run full GTK suite | revert the extraction commit before Iced consumes it |
| Workspace lock emits callbacks or persistence reorders writes | non-blocking event enqueue under lock; callbacks/UI/persistence after unlock; revision tests | retain current sender/sequence design |
| Slow UI diverges | revisioned events plus mandatory full snapshot resync | fall back to resync on every ambiguous transition |
| Iced canvas text misaligns graphemes | fixture screenshots and measured cells in both backends | custom primitive widget behind same adapter |
| GPU unavailable in CI or older Linux | tiny-skia deterministic lane and wgpu smoke | make software backend CI default, keep runtime selection |
| X11/Wayland behavior differs | explicit lanes and shed runs, no implicit backend | document a narrowly scoped renderer limitation only with evidence |
| Target generalization regresses existing CLI | table-driven three-target tests and precedence fixtures | keep explicit `--socket` escape hatch while fixing selector |
| FFI pressure distorts Rust API | owned serializable DTOs and opaque future ABI, but no unsafe export now | separate branch/ADR for FFI proof |
| Scope exceeds one safe commit | vertical slices, launchability gate, per-commit CI | stop at last green useful commit only if an external blocker is documented |

## Deferred behavior policy

Only presentation flourishes outside the common product contract may be
deferred. Every deferral must identify the exact GTK behavior, reason, affected
platform/backend, user-visible impact, and a focused test or issue. Broad
`iced` skips, swallowed UI-port failures, static terminal demos, and dependency
shortcuts through `roost-linux` are not acceptable deferrals.
