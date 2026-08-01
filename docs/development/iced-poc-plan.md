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
