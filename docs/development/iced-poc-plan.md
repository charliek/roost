# Iced UI and shared Rust engine proof-of-concept plan

Status: POC proposal, not an accepted replacement architecture

Implementation note: the shared-engine extraction, isolated third-target
contract, and Iced walking skeleton are implemented on `poc/iced`. The walking
skeleton has been exercised on macOS and in the Linux shed under X11 and
Wayland with both wgpu and tiny-skia. Product-parity work remains governed by
the acceptance matrices below. The current Iced shell is intentionally still a
walking skeleton: its visual hierarchy and polish differ materially from both
GTK and Swift, and the remaining functional gaps are not accepted merely
because screenshot plumbing exists. This note does not promote the POC to
accepted replacement architecture.

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

The POC enables `wgpu`, `tiny-skia`, `x11`, and `wayland`. Production default is
Iced's `Best`: wgpu on Metal/Vulkan when available, with tiny-skia as the
software fallback. CI runs an explicit software lane via
`ICED_BACKEND=tiny-skia` to remove GPU-driver nondeterminism, plus at least one
wgpu smoke where the runner supports it.

The walking skeleton initially used an Iced `Canvas` program. Cross-renderer
product and external-compositor captures exposed the released tiny-skia canvas
translation defect tracked by [iced#3243](https://github.com/iced-rs/iced/issues/3243):
a canvas below Roost's 220 pt sidebar and 44 pt tab band was rendered at
`(440, 88)` instead of `(220, 44)`. The official fix
([iced commit `76b32d4`](https://github.com/iced-rs/iced/commit/76b32d4906c0023ade37192a16570f9eb100e2b6))
landed after 0.14.0, which is still the latest released Iced version. The POC
therefore exercised its documented rollback instead of adopting an unpinned
Git dependency or vendoring a renderer patch.

The terminal is now one renderer-neutral custom Iced widget that emits core
text and quad primitives directly, not thousands of text widgets. It draws in
absolute `layout.bounds()` coordinates, clips every primitive to the widget
and viewport intersection, and deliberately disables pixel snapping for the
fractional 8.4 pt cell grid. On each redraw it snapshots the active
`roost-vt::RenderState`, resolves theme and cell colors, fills backgrounds,
draws unwrapped shaped glyph clusters at measured cell origins, applies
supported bold/italic styles, draws selection and link affordances, then draws
the cursor. Its persistent widget state owns pointer capture, hover cells, and
multi-click sequencing; unrelated keyboard/window events remain ignored so
the application keyboard subscription still reaches the PTY. The terminal
handle and render walk stay on Iced's update/draw thread. PTY readers emit
owned byte messages; the UI applies `vt_write` and schedules redraw.

The walking skeleton must prove:

- widget glyph metrics are stable for ASCII, wide, and combined characters;
- foreground/background/inverse/style resolution matches GTK;
- cursor shapes and clipping work in both backends;
- resize quantizes pixels to rows/columns and resizes both VT and PTY;
- window screenshots return RGBA bytes that can be encoded as PNG for IPC; and
- keyboard subscriptions and widget pointer events cover terminal input,
  shortcuts, selection, wheel/scrollback, hyperlinks, and mouse reporting.

The custom widget is the exercised renderer rollback. A renderer failure does
not justify depending on GTK or copying its workspace core. A future released
Iced version may permit reconsidering Canvas, but only after the same product
capture and focused origin/clipping tests pass under both backends.

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
| Ubuntu/Wayland/wgpu | yes | targeted | non-clipboard suite under input-less headless Weston | renderer; native clipboard requires a seat/serial |
| Ubuntu/Wayland tiny-skia | yes | targeted | startup and terminal smoke | renderer diagnostics |

The shed is the local Linux authority. Its build script keeps Cargo and Ghostty
outputs in guest-local paths, builds `roost-engine`, GTK, Iced, and `roostctl`,
then provides explicit X11 and Wayland Iced gates. The existing cage/uinput GTK
guard remains unchanged. The shed is stopped after final validation.

## CI and Make changes

Make targets:

- `build-iced`, `run-iced`, `test-iced`;
- `e2e-iced`, `e2e-iced-ci`, `e2e-iced-clipboard`, `smoke-iced`;
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
7. **Visual and interaction convergence**: audit the same seeded workspace in
   Swift, GTK, and Iced; then close measured gaps in window hierarchy, sidebar
   proportions and rows, tab band/pills/badges/close controls, typography,
   terminal padding/cursor/selection, palette placement, resizing, hover,
   focus, and disabled/empty/error states. Treat GTK as the Linux visual
   reference and Swift as a second product reference, allowing only named
   native-toolkit differences. Each polish slice adds focused geometry/color
   assertions and refreshed human-comparison artifacts before it is accepted.
8. **Harness, CI, and shed completion**: target-neutral Iced E2E, coexistence,
   X11/Wayland/macOS matrix, artifacts, Make/docs, final regression gates.
9. **Final review fixes**: architecture/dependency/diff review, no feature scope;
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

### Active mini-plan: renderer-neutral Iced screenshots and visual gate

Scope: implement Iced's `app.screenshot` UI port through the released Iced
0.14 `window::screenshot` task, normalize its renderer-owned RGBA capture to
Roost's requested logical 1x/2x contract, encode PNG in the Iced adapter, and
extend the existing screenshot harness and targeted sidebar pixel assertion to
the third UI. No screenshot, PNG, window, or renderer type enters the engine.

Invariants and interfaces:

- both compiled backends use Iced's public screenshot task; the adapter does
  not reach into `iced_wgpu`, `iced_tiny_skia`, a native window handle, or an
  operating-system capture API;
- the captured physical dimensions and Iced scale factor are normalized to
  `logical renderer surface * requested scale`, so scale 1 and 2 have identical
  wire semantics on Retina macOS, X11, and Wayland (where the renderer surface
  can differ from decorated window metrics). Resampling and PNG encoding
  validate scale factors, dimensions, byte lengths, and allocation arithmetic
  and return explicit errors instead of panicking;
- the app owns a FIFO of screenshot requests and at most one renderer capture
  in flight. Every request sender is consumed exactly once; an early request
  waits for the first window id, and another UI task can delay but never discard
  a queued capture;
- the existing shared IPC dispatcher continues to validate requested scale and
  the 16 MiB framed-response limit. The Iced adapter returns owned PNG bytes and
  dimensions only after renderer access has completed on Iced's event loop;
- `tools/screenshot` accepts `iced` with its isolated profile, binary, socket,
  log, launch, and shutdown paths. The scenario remains target-neutral and
  creates only its throwaway workspace rows; and
- visual automation asserts owned lifecycle colors and common left-edge
  geometry rather than whole-window pixel identity. Failure artifacts include
  the last capture and Iced logs; human comparison artifacts use the same
  seeded GTK and Iced scenario.

Acceptance: unit tests cover 1x/2x normalization at native 1x/2x, malformed
RGBA metadata, and PNG dimensions; `test_sidebar_pixels.py` passes for Iced on
macOS and in the shed with wgpu/tiny-skia under X11/Wayland; it joins the
required Iced Make/Actions suite; `make smoke-iced` produces the five labeled
scenario PNGs and manifest; full Iced has no remaining functional failure;
GTK, Swift, dependency-boundary, and complete local/shed gates remain green.

### Active mini-plan: Iced logical keyboard-focus ownership

Scope: implement `app.active_terminal_focused` for Iced as the adapter's
truthful logical keyboard-route owner, broaden the existing focus contract
from GTK to both Rust UIs, and make those tests part of every required Iced
functional lane. This does not introduce widget focus or presentation state
into `roost-engine`.

Invariants and interfaces:

- Iced's terminal owns keyboard routing only when the command palette is
  closed and the workspace's active tab has a live terminal adapter. A palette
  takes ownership before its input focus task is issued and keeps ownership
  across background project/tab navigation until explicit dismissal or a
  committed activation that intentionally closes it;
- the IPC port returns that adapter state instead of a constant, so both true
  and false transitions are observable without relying on compositor focus.
  One adapter-owned keyboard-route decision is shared by PTY key dispatch and
  the IPC getter so the observable cannot drift into shadow state;
- native toplevel focus and mode-1004 terminal focus reports remain separate:
  `app.set_window_focus` continues to own the latter test seam, and an
  unfocused window does not change which in-app route would receive a key;
- navigation, close-survivor, and core/displayed-tab behavior continue to use
  authoritative workspace operations. Focus ownership never creates a second
  tab-selection state machine; and
- GTK-specific critical-log assertions stay GTK-only even though the common
  logical focus behavior runs on GTK and Iced.

Acceptance: unit tests prove `None` with no live active adapter, `Terminal`
after attachment, and `Palette` precedence; all eight existing focus tests plus
a test-mode window-focus separation test pass on Iced macOS and in the shed
with wgpu/tiny-skia under X11/Wayland. The functional gate still proves the
false palette state and palette ownership during navigation rather than
accepting an always-true implementation; the file joins the required Iced
Make/Actions gate; full Iced, GTK, Swift, dependency-boundary, and local/shed
gates remain green.

### Active mini-plan: Iced palette selection visibility

Scope: remove the remaining Iced-only functional skip by keeping the selected
palette row fully visible with Iced widget-tree operations and reporting the
same `selected_in_view` contract GTK exposes. This is an Iced presentation
adapter concern; palette frames, filtering, ranking, and selection remain in
`roost-ui-model`.

Invariants and interfaces:

- each palette layout revision has stable adapter-local scrollable and selected
  row widget IDs. Opening a frame, changing the query, moving/activating the
  selection, resizing the window, or materially changing the visible dynamic
  frame invalidates the previous geometry. Background-frame refreshes wait
  until that frame becomes current, and unchanged per-tick refreshes do not
  advance the revision;
- a renderer-neutral widget operation first locates the real scroll viewport
  translation and selected-row bounds. Because Iced visits the scrollable
  before its child containers, `Outcome::Chain` then runs a second operation
  to mutate scroll state and a third traversal to read the new translation
  before publishing. It scrolls by only the amount needed to reveal a clipped
  edge. The IPC field stays `None` while geometry is unavailable or stale and
  is `false` only when a freshly measured row is genuinely clipped;
- stale measurement results carry a palette session and layout revision and
  cannot overwrite a newer frame/query/selection. Dismissal clears geometry;
- one adapter helper owns invalidation, revision/ID advancement, measurement
  clearing, and task scheduling for open/present, query, keyboard movement,
  mouse/IPC activation, frame push/pop, async provider success/error, visible
  agent/notification replacement, and resize. A separate measure-only request
  follows `Scrollable::on_scroll`, so manual browsing updates the observable
  without snapping the selection back into view;
- a missing window or widget ID leaves the matching revision pending for the
  next settled tick/window-open event. An empty result set has no selected-row
  ID and remains `None` without retrying indefinitely;
- visibility tasks compose with focus, activation, resize, and screenshot
  tasks instead of replacing them, preserving UI-request arrival order; and
- no Iced widget ID, bounds, callback, or renderer type enters `roost-engine`,
  `roost-ipc`, or `roost-ui-model`.

Acceptance: unit tests pin top and bottom minimal reveal, already-visible
no-op, fractional tolerance, zero-height rejection, bounded missing-geometry
retry, stale session/revision rejection, newer-manual-scroll precedence, and
structural-vs-content-only dynamic-refresh scheduling. The existing
theme-frame functional test runs on GTK and Iced without an Iced skip, proves
the chained operation reaches a fresh visible measurement, shrinks the native
window, and proves resize invalidation restores `selected_in_view == true`;
full Iced
has only the shared OSC #145 and local Bash 3.2 skips; `test_palette.py` joins
the required Iced Make/Actions suite; macOS plus shed
wgpu/tiny-skia X11/Wayland gates pass; complete Rust, GTK, Swift, harness,
dependency-boundary, and CI gates remain green.

Commit boundary: `cc6daac` (logical focus) was pushed and Actions run
`30693454734` completed green before this slice began. Visibility adapter code,
tests, skip removal, Make/Actions changes, and this reviewed plan land together
as the next independently green commit.

### CI follow-up mini-plan: tolerate delayed Iced palette layout

Evidence: commit `aeaf6e6` passed twelve repeated local macOS/wgpu launches and
the pre-push macOS plus shed wgpu/tiny-skia X11/Wayland matrix, but Actions run
`30694544039` timed out waiting for the first theme-row visibility measurement
on macOS/wgpu and Linux Wayland/tiny-skia. Every preceding test passed. The
adapter currently abandons a `Visibility::Missing` result after two 16 ms
ticks, so a widget tree that takes more than roughly 32 ms to materialize can
never resynchronize until an unrelated resize or palette mutation occurs.

Scope: keep the same revisioned locate/reveal/remeasure operation and expand
only its missing-layout recovery policy. A named, bounded retry budget covers
slow debug builds and software renderers; one retry is scheduled per completed
`Missing` result, never as a blocking loop. Palette session/revision changes
reset the budget and stale results remain discarded. Exhaustion leaves
`selected_in_view` unavailable (`None`) and emits one warning containing
session, revision, and retry count; `false` remains reserved for a genuinely
measured but clipped row. A genuine `Visible(false)` answer is not retried.

Invariants: retries never outlive their palette session/revision, manual
scroll still supersedes an older reveal request, no operation runs while state
is locked, no renderer-specific branch or test skip is introduced, and the
normal already-laid-out path still completes in one operation. The retry limit
is expressed as a constant and unit-tested at its boundary so a future timing
change cannot silently restore the two-tick assumption.

Acceptance: unit tests prove first, penultimate, and exhausted `Missing`
transitions plus stale-result rejection; the focused theme/resize E2E passes
repeatedly on macOS/wgpu and in the shed under wgpu/tiny-skia on X11 and
Wayland; Iced format, tests, warnings-denied clippy, dependency checks, the
complete local gate, and GTK theme regression stay green; the fix is committed
and pushed separately, and Actions is green in all four Iced matrix jobs before
the next feature slice begins.

### CI root-cause follow-up: scroll notifications preserve layout identity

Evidence: the expanded missing-geometry budget made the palette test pass in
Actions' Ubuntu/wgpu job but the same run still timed out on macOS/wgpu and
Ubuntu/tiny-skia. Local debug traces show a programmatic reveal immediately
emitting `PaletteScrolled` and advancing revisions (for example revision 62 to
63), while Iced 0.14's `notify_viewport` publishes `on_scroll` when bounds,
content bounds, or offsets change. The callback therefore does not identify a
manual gesture. Treating every notification as structural invalidation can
continually replace row IDs and stale an otherwise successful measurement.

Scope: preserve the palette layout revision and stable row IDs for scroll
offset notifications while advancing a separate measurement generation. The
callback clears the last measurement, resets only the missing-geometry retry
counter for later measure-only scrolls, and queues a measure once the current
structural reveal has succeeded. Layout-driven notifications before that
success preserve the required reveal intent, so delayed geometry cannot
finalize a clipped row.
Content-only refreshes also preserve that intent, and both missing geometry and
persistently clipped reveal results use bounded retry handling. A separate
scheduling budget counts every reveal attempt even when a programmatic scroll
generation-fences its result, preventing an alternating-offset task storm.
Structural frame/query/selection/resize changes continue through the existing
revision-advancing invalidation path. An already-issued reveal keeps valid row
IDs but its pre-scroll result is rejected by the measurement generation; the
queued request remains Reveal until that structural intent succeeds, then later
scrolls queue Measure.

Invariants: programmatic reveal cannot create a revision loop; scrolling
invalidates the observable result, preserves an incomplete structural reveal,
and becomes measure-only after a successful reveal; stale
frame/query/selection results remain fenced by session/revision and stale
viewport results by measurement generation; no renderer branch or test
relaxation is introduced. Unit tests prove scroll measurement clears
visibility, clears retries after structural reveal intent is satisfied,
replaces reveal without touching revision-owned IDs, and
rejects older Visible/Missing results. They also prove a structural reveal
survives an intervening layout notification until stable geometry can reveal
the row. The complete palette suite is repeated
under macOS/wgpu and all shed
renderer/display combinations, then the full local/shed gates, review, push,
and all four Actions Iced jobs must pass before new feature work.

### CI root-cause follow-up: retain pre-window screenshot requests

Evidence: the Ubuntu/wgpu lane failed `app.screenshot` with
`UI dropped reply` in consecutive Actions runs `30696537783` and
`30696820857`, while Ubuntu/tiny-skia, both macOS renderers, and repeated local
plus shed runs passed. In both failures every other Iced functional assertion,
including the native clipboard suite, passed. The adapter currently evaluates
`(self.window_id, self.screenshot_queue.pop_front())` before matching the two
`Some` values. When the IPC server accepts the first capture just before Iced
publishes `WindowOpened`, `pop_front()` still removes and drops the request even
though there is no window ID to schedule. Dropping its oneshot sender produces
the observed engine error immediately; this is an ownership bug, not a timeout
or renderer failure.

Scope: encapsulate pending and in-flight captures in a small adapter-owned
`ScreenshotQueue`. Enqueue retains the IPC reply sender. `start_next` first
requires a native window ID and no active capture, and only then removes the
oldest pending request and returns the Iced screenshot task. A private
window-open preparation step returns both the composed task and an explicit
`retained_resize_scheduled` flag. `window_opened` exposes the task, while
`window_resized` uses the flag—not `UiTask::None`—to decide whether the native
resize event must update terminal geometry. The preparation step orders any
retained native resize before `start_next` (`Resize.then(Screenshot)`), so a
pre-window request starts immediately after the requested native size is
applied; the periodic tick remains a recovery path. Capture completion takes
exactly the active request, sends its encoded result, and starts the next FIFO
entry. No IPC DTO, engine behavior, renderer, or screenshot encoding changes.

Invariants: absence of a window never consumes or closes a reply sender; only
one native capture is active; requests complete in receive order; repeated
open/focus/resize events cannot schedule the active request twice; completing
one request cannot answer another; UI work remains on Iced's event loop and no
engine or terminal lock crosses a native task. App teardown still closes all
remaining senders so callers fail deterministically instead of hanging. The
engine and Iced dependency boundaries remain unchanged.

Tests and acceptance: unit tests prove a pre-window request remains pending
with an open receiver, begins after a window ID appears, and blocks a second
capture until the first is completed; they also prove FIFO completion and
sender closure for both pending and in-flight requests on queue drop. Window
integration tests prove a retained resize is composed before screenshot
startup, a resize event still requests geometry application when screenshot
startup is the only returned task, and repeated open/focus/resize preparation
cannot duplicate an active capture. The existing functional test continues to
exercise stable 1x/2x capture plus two simultaneous IPC clients without a retry
or skip. Repeat the full Iced functional lane under macOS wgpu and tiny-skia
and shed X11/Wayland wgpu/tiny-skia, run `make check` and `make check-iced`,
preserve GTK and Swift regression results, review the full diff, commit and
push separately, and require all Actions jobs—including the Ubuntu/wgpu lane
that reproduced twice—to pass before feature work resumes.

### Native Iced clipboard bridge commit plan

Scope: replace `roost-iced`'s process-local system/selection clipboard shadows
with Iced 0.14's native standard and primary clipboard tasks for the existing
`clipboard.dump`, `clipboard.write`, and OSC 52 UI ports. This commit does not
yet add user copy/paste keybinds, copy-on-select, middle-click paste, image/URI
paste, or multi-click selection; those remain a following presentation/input
slice after the native port itself is proven.

Interfaces and flow: the toolkit-neutral engine continues to emit
`UiRequest::ClipboardDump` / `ClipboardWrite` and `OscAction::ClipboardWrite`
with `ClipboardOp` / `ClipboardTarget`; no toolkit type enters the engine. The
Iced adapter allocates a monotonically increasing request ID for each native
read or write (skipping every active, queued, or pending ID on wrap), retains
read IPC oneshots in adapter-owned pending state, maps task results back through
typed Iced messages, and resolves exactly one pending request. Writes become
explicit `UiTask` effects. `service_ui_requests` folds reads and writes into an
adapter-owned FIFO in `ui_rx` receive order. Only one native effect is active;
its typed completion starts the next operation. This stronger serialization
guarantees that a fire-and-forget write completes before an immediately
following dump begins even when the two requests arrive on different ticks.
Unrelated UI effects still compose through `UiTask::Then` / Iced `Task::chain`.
`apply_osc_actions` likewise returns the queue's next task, which both the live
PTY tick and test-only `TabFeedPtyBytes` path append rather than discard. System
maps to the standard clipboard; selection maps to PRIMARY. Iced's
macOS backend reports PRIMARY unavailable, so reads return empty there, matching
the existing macOS GTK adapter rather than the AppKit-only named selection
pasteboard.

Invariants: all native clipboard access occurs on Iced's event loop; engine and
PTY locks are never held across a toolkit operation; stale/duplicate read
results cannot answer another caller. The adapter inserts each reply sender into
an ID-keyed pending map before scheduling its native read, removes it on the
first result, logs unknown/duplicate IDs without consuming a different request,
ignores a failed reply send after caller cancellation, and drops all remaining
senders with the app. `clipboard-write = deny` filters only OSC 52 before a
native write is scheduled; the test-only IPC clipboard port continues to bypass
that preference, matching GTK. Empty/native-unavailable reads return `Ok(None)`
rather than hanging; the engine remains free of Iced and GTK dependencies.

Tests and acceptance: unit tests cover target mapping, monotonic request IDs,
wrap/collision avoidance, single-consumption/stale-result handling, ordered
write-then-read task composition, and OSC write policy. Pending-state tests
include an out-of-order read result that is rejected while both replies remain
pending, duplicate and unknown results, caller cancellation, and app-drop
cleanup. Native functional coverage includes exact,
no-poll `write(system, A) -> dump == A` and
`write(A) -> write(B) -> dump == B` sequences. Unit coverage asserts OSC
allow/deny task presence and unchanged queue state under deny. Existing
target-neutral `test_selection.py` and `test_osc52.py` are added explicitly to
an Iced native-clipboard Make target, to the default Iced Make gates outside
Wayland, and to the Actions X11 and macOS lists. Linux X11 runs system and
PRIMARY coverage under wgpu and tiny-skia; macOS requires system coverage and
records the accepted Iced PRIMARY parity gap with one narrow selection-target
skip. The complete Iced suite, GTK clipboard regressions,
warnings-denied lint, dependency boundaries, macOS build, and shed wgpu /
tiny-skia X11 clipboard tests must pass. Review the complete diff,
commit as one native-adapter slice, push `poc/iced`, and require green Actions
before beginning user-triggered paste behavior.

Wayland evidence and deferral: Iced 0.14 delegates Wayland clipboard access to
`smithay-clipboard`, whose regular `wl_data_device` path requires a focused
seat and a current input serial. The headless Weston CI backend deliberately has
no input devices, so both native targets are unavailable there; its required
Iced renderer suite therefore remains non-clipboard, with X11 serving as the
documented native-clipboard equivalent. A shed experiment under cage plus real
`/dev/uinput` proved a standard write after a fresh pointer serial, but cage
accepted only one programmatic selection update per serial and did not expose
the PRIMARY protocol. Repeated OSC 52 writes therefore are not yet a reliable
Iced/Wayland capability. Adding a privileged data-control client would be a
separate platform decision (and is not portable to compositors that omit that
extension), so it is deferred rather than hidden behind a passing headless
test. User impact: macOS and X11 native clipboard/OSC work in this slice;
Wayland native writes depend on the compositor accepting Iced's most recent
input serial, and PRIMARY depends on compositor protocol support. The next
clipboard/input slice must resolve or explicitly accept this limitation before
the POC can claim full Wayland clipboard parity.

### Iced text clipboard interaction commit plan

Scope: connect the existing native Iced clipboard queue to real terminal
interactions. The effective toolkit-neutral keybind table (defaults plus
ordered user overrides and `unbind`) supplies Copy and Paste accelerators;
explicit Copy publishes the active terminal selection to the system clipboard
and, where available, PRIMARY, while Paste reads the system clipboard and
sends text to the tab that initiated the read. A completed local left-button
drag applies `copy-on-select` (`off`, PRIMARY-only `true`, or PRIMARY plus
system `clipboard`), and a Linux middle-button press reads PRIMARY and pastes
through the same path. Mouse-reporting gestures retain precedence and never
copy or paste locally. This slice is text-only: image/file-URI paste, URL
hover/open, drag-and-drop, and native double/triple-click timing remain named
later presentation slices.

Interfaces and ownership: `roost-ui-model::keybind` remains authoritative for
accelerator parsing, platform defaults, override order, and action identity.
The Iced adapter maps a native key event to the shared `Accel` value without
introducing a second binding grammar. An already-open palette owns its input;
otherwise the effective table is resolved before any action, including the
currently hard-coded palette openers. This slice dispatches effective
Copy/Paste plus the four already-supported palette actions, so an `unbind` or
a Copy/Paste replacement on a former palette trigger cannot be bypassed by a
legacy check; terminal input owns unmatched or not-yet-supported actions.
`ClipboardQueue` gains an explicit read destination: either an IPC reply or a
paste request carrying
the initiating tab ID. Native completion removes exactly that destination;
the app answers IPC or asks the still-live tab to produce bracketed/unbracketed
paste bytes. Clipboard callbacks never retain a terminal reference. Pointer
messages carry the Canvas's originating tab ID, and handling validates that
origin instead of looking up whichever tab is active at dispatch time. Pointer
handling returns an explicit local-selection completion to the app, which
applies config policy and schedules ordered native writes on Iced's event loop.
No toolkit type enters the engine, `roost-vt`, or shared config/keybind model.

Invariants: palette input keeps precedence and is never captured as a terminal
Copy/Paste action; configured `unbind` and replacement triggers apply before a
key reaches the PTY. Empty/unavailable clipboard text and empty selections are
no-ops. Paste is bound to the initiating tab, not whichever tab happens to be
active when the async read completes, and safely disappears if that tab has
closed; a pointer event for a switched-away or closed tab cannot mutate the new
active terminal. DECSET 2004 wraps non-empty UTF-8 exactly once with
`ESC[200~` and `ESC[201~`; ordinary paste is byte-exact. Explicit Copy orders system and
PRIMARY writes deterministically: the system clipboard is scheduled first, then
best-effort PRIMARY, so Wayland's observed one-update-per-input-serial behavior
cannot sacrifice ordinary Paste for the secondary selection. `copy-on-select`
follows the documented three-state policy with that same order for
`clipboard`; tracking-owned press/motion/release gestures cannot fall through
to selection, copying, or middle paste. All native effects remain FIFO, never
hold workspace/terminal locks across an Iced task, and preserve the existing
IPC/OSC queue semantics and GTK behavior.

Tests and acceptance: pure unit tests cover Iced-key-to-shared-accelerator
mapping (including exact modifiers), default and overridden Copy/Paste
bindings, queue destination scoping/stale completion, initiating-tab paste,
empty/plain/bracketed byte production, copy-on-select policy, pointer-origin
tagging, and a Copy replacement on a former palette trigger. The existing
mouse-reporting functional suite covers tracking precedence; pointer dispatch
uses only the tagged origin and makes a missing/closed origin an explicit
no-op. A self-contained Xvfb/xdotool check
launches the real `roost-iced` binary with isolated paths and verifies an
effective configured Copy key writes the native clipboard, Paste reaches PTY
capture, bracketed Paste is framed, a real drag triggers copy-on-select, and
middle-click reads PRIMARY; the explicit-Copy case sets `copy-on-select=off`
so an earlier drag cannot create a false positive. It is required under both
Iced renderer lanes in Actions and in the shed. A macOS-only adapter test
constructs the Iced Command-key event and follows its resolved Paste request
through queue completion to initiating-tab PTY bytes; the two renderer lanes
continue to require native system clipboard IPC/OSC coverage. The existing
selection, mouse-reporting, X11, macOS, and headless Wayland suites remain
green. The shed real-seat Wayland run must prove an explicit system
Copy-to-Paste round trip and a `copy-on-select=clipboard` drag-to-Paste round
trip. Lack of PRIMARY protocol support may only produce a named
PRIMARY/middle-click accepted limitation after those system paths pass; it is
not allowed to skip or excuse the whole real-seat check. Run formatting,
warnings-denied clippy, dependency checks, full GTK and Swift regressions,
full macOS and shed Iced
matrices, complete diff review, then commit, push, and require green Actions
before starting native multi-click or URL behavior.

Implementation evidence (2026-07-31): the slice added eleven focused Iced
tests (53 total) and passed `make check` plus `make check-iced` on macOS. The
complete fresh target-neutral suites passed with `157 passed, 3 skipped` for
Iced, `151 passed, 9 skipped` for GTK, and `144 passed, 16 skipped` for
Swift/AppKit; every skip printed a narrow capability or pre-existing-issue
reason. Both macOS Iced renderer runs passed the required functional set
(`47 passed, 2 skipped` each). In the shed, the Actions-equivalent Iced set
passed on X11/wgpu (`48 passed, 1 skipped`), X11/tiny-skia (`41 passed,
1 skipped`), Wayland/wgpu (`42 passed`), and Wayland/tiny-skia (`42 passed`);
the focused Iced mouse-tracking suite passed all 10 cases. The X11 real-input
script passed under both renderers, and `tools/shed/shed-test.sh --run` passed
the existing GTK Wayland pointer-drag guard, the Iced X11 clipboard guard, and
the Iced real-seat Wayland clipboard guard. The latter proved explicit
Copy/Paste and drag-copy/Paste before reporting cage's named lack of PRIMARY.
The dependency gates continued to show no GTK/libadwaita/Pango/Cairo or
`roost-linux` edge beneath `roost-iced`, and no GTK or Iced edge beneath
`roost-engine`. Complete diff review requested two fixes—canonical punctuation
accelerators and direct pointer-precedence/origin tests. Both were implemented,
the affected gates above were repeated, and re-review approved the slice with
no remaining findings.

### Iced native multi-click and URL interaction commit plan

Scope: complete the native terminal click behavior that remains after the
clipboard slice. Iced's Canvas adapter will turn consecutive primary-button
presses into deterministic click counts; the existing terminal operation will
expand a double-click to the shared word span and a triple-or-later click to the
line span. Holding the effective `link-modifier` over a regex-detected URL or
OSC 8 hyperlink will underline its full contiguous cell span, show the pointer
cursor, and make a primary click request a platform URL open. This is an Iced
presentation/input slice: URL detection reuses `roost-url`, word expansion
reuses `roost-ui-model`, terminal hyperlink lookup stays in `roost-vt`, and the
toolkit-neutral engine is unchanged.

Interfaces and ownership: `TerminalCanvasState` owns native click timing, while
the originating `TerminalTab` owns its last in-bounds pointer cell so App-level
modifier events can recompute hover without reaching into private widget state.
Canvas publishes cell-bearing press/motion/release events plus a coordinate-free
leave event. Canvas tracks its inside/outside transition because Iced's
`CursorLeft` means leaving the window, while a move from terminal to sidebar is
an out-of-bounds `CursorMoved`; either path publishes passive leave exactly once
before cursor-position conversion and clears hover, last in-bounds cell, and
click sequencing. An already captured gesture is different: one compound
pointer message carries its clamped cell plus `inside = false`, which App
interprets as both hover exit and captured dispatch (Canvas can publish only one
message per update). Tracking and local selection retain ownership and receive
that clamped out-of-bounds motion/release,
while multi-click and URL owners retain ownership but consume motion/release
without mutation. `mouse_interaction` also requires `cursor.is_over(bounds)` so
a cached link cannot leak the hand cursor over the sidebar. Unsupported or changed buttons reset the
click sequence even when they do not map to a terminal button. A primary press
carries a saturated `click_count`; the tracker continues a sequence only when
the tab, button, cell, and a 500 ms window match, and resets on tab replacement,
timeout, another button, or pointer leave. The application resolves
`RoostConfig::link_modifier` through the shared keybind model and passes the
current modifiers to the originating `TerminalTab`. Each tab owns a shared
`roost-url` hover value derived from current terminal state. OSC 8 wins over
regex detection; a contiguous hyperlink-span helper moves to `roost-url` and
GTK migrates to it so the row/URI boundary algorithm is not duplicated between
Rust adapters. Pointer motion updates the originating tab's hover, modifier
changes recompute only the active tab from its app-owned last cell even without
motion, and leave clears hover. The render snapshot contains only the resolved
underline span, never a terminal or toolkit handle. Canvas draws that span and
returns Iced's pointer interaction while it is active.

Click routing is explicit and stable: configured modifier plus a URL has first
claim, including while terminal mouse reporting is enabled; terminal tracking
then owns the complete press/motion/release gesture; local multi-click expansion
then precedes ordinary drag selection. A multi-click-owned gesture suppresses
subsequent motion/release mutation and emits one selection completion, so
copy-on-select runs exactly once. A URL-owned gesture preserves the previous
selection and never triggers copy-on-select. URL ownership latches on the primary
press, emits exactly one `OpenUrl` then, and consumes motion/release even if the
modifier is released or hover clears; launcher failure never replays the gesture.
Whitespace or invalid double-click
expansion falls back to the ordinary local selection anchor rather than leaving
stale gesture ownership. Every pointer action remains tagged with its Canvas tab
ID; switching or closing tabs cannot redirect hover, launch, or selection.

Platform opening remains an Iced UI port. `UiTask::OpenUrl` contains an owned
string and maps in the binary to an asynchronous, argument-safe `open` (macOS)
or `xdg-open` (Linux) child process. The task awaits
`tokio::process::Command::status`, so process waiting is nonblocking and the
child is always reaped; success means a zero launcher exit status, while spawn
and nonzero-exit failures return distinct owned errors. No shell is involved and
the Iced event loop is never blocked. A pure command builder accepts an explicit
platform enum so both hosts are testable on either OS. App tests inject the
explicit open task and synthetic success/error completions; runner tests use a
fake executor and never launch a browser. Completion logs and exposes status
without mutating engine state. The URL string and no borrowed Rust or toolkit
object crosses the task boundary.

Invariants: hover computation never holds workspace or PTY supervisor locks and
never invokes UI code from terminal state access. Link modifier press/release,
pointer motion/leave, terminal output, resize, and tab replacement cannot retain
a stale underline. OSC 8 URI identity controls its span; regex URL columns remain
terminal-cell columns for Unicode/combined rows. Link clicks preserve existing
selection and tracking state, while a failed launcher is visible but does not
replay the click into the PTY or selection. Multi-click counts are local adapter
metadata and do not widen the engine command contract. Iced tabs receive the
effective configured `word-break-chars` at attachment instead of
hard-coding the default, so native and IPC expansion share GTK/Swift semantics.
Swift behavior and both existing native launch adapters remain unchanged;
GTK keeps its interaction contract while consuming the shared link projection;
`roost-iced` gains only the focused `roost-url` dependency and still has no
GTK/libadwaita/Pango/Cairo edge.

Tests and acceptance: pure Canvas tests use injected instants to cover first,
double, triple, timeout, cell/button/tab reset, saturation, and leave behavior.
Terminal tests cover shared word/line expansion from native presses, whitespace
fallback, copy completion exactly once, URL-over-tracking precedence, tracking
over multi-click, URL selection preservation, closed/switched origin routing,
regex and OSC 8 precedence/span boundaries, modifier-only hover recomputation,
configured non-default word-break punctuation, and hover clearing after
leave/output/resize. Captured-gesture tests cover press inside, drag outside,
and release for both local selection and terminal tracking, alongside passive
move-out leave and non-mutating multi-click/URL release. Drawing/state tests cover underline geometry and pointer
interaction. Platform-port tests assert exact macOS/Linux program/argument
construction, absolute-URI validation, and deterministic unsupported-platform,
spawn, and exit-status mapping through a fake runner. Existing
`tab.expand_selection_at`, selection,
copy-on-select, mouse-reporting, and URL fixture tests remain green.

A focused real-input X11 test writes a known URL and word to a real PTY, drives
double/triple clicks with `xdotool`, and verifies selection text plus configured
copy-on-select through IPC/native clipboard. It also holds the configured link
modifier and verifies the effective `app.cursor_shape` diagnostic reports the
link pointer over any OSC 22 shape and restores the OSC shape after modifier
release/leave; browser launch itself stays behind the testable UI task to avoid
CI side effects. Run this under wgpu and tiny-skia. The shed real-seat Wayland lane
repeats multi-click selection and hover input using its existing uinput/cage
path. macOS has no repository-native trusted pointer injector, so this commit
requires both renderer lanes' target-neutral operation suite plus cross-platform
Canvas/App/launcher unit coverage; native macOS multi-click automation is a
named gap with user impact limited to automation evidence, not implementation.
Before commit: format,
warnings-denied clippy, dependency boundaries, full Iced unit/functional and
renderer matrices, complete GTK and Swift regressions, shed X11/Wayland gates,
complete diff review, push `poc/iced`, and require green Actions.

Implementation evidence (2026-08-01): Iced now owns Canvas click sequencing
and captured-gesture state, while `TerminalTab` owns selection/tracking/link
precedence and emits an owned `OpenUrl` task. `roost-vt::RowTextProjection`
preserves complete grapheme text and maps Unicode scalar spans back to terminal
cells, including combining marks, sparse rows, and wide-cell tails; GTK and
Iced both consume that projection for regex links. `roost-url` owns the shared
OSC 8 span/value, and GTK migrated off its duplicate. Both Rust UIs report the
live link cursor override; Iced maps every supported OSC 22 shape to a native
Iced interaction. The launcher accepts only absolute, control-free URIs and
passes one owned argument without a shell. Harness teardown now waits for its
owned Rust child before removing only that target's socket/lock, so a fresh
Iced run leaves no developer-cache runtime artifact.

The exact pre-commit gates passed: `make check` (workspace format/clippy/tests,
118 GTK binary tests, 67 Iced tests, 11 `roost-vt` FFI tests, 7 `roost-url`
tests, 688 Swift Testing tests, 11 XCTest tests, and 14 harness unit tests);
macOS Iced wgpu and tiny-skia functional lanes each passed 47 tests with two
named environment/protocol skips; full fresh GTK passed 151 with nine named
skips and Swift passed 144 with sixteen named skips. In the shed, Iced passed
65 Linux unit tests plus warnings-denied clippy, both full X11 renderer lanes
passed 159 tests with only known issue #145 skipped, both headless Wayland
lanes passed 42 tests, and both X11 and real-seat cage/Wayland renderer lanes
passed Copy/Paste, drag selection, double/triple click, and live Alt-link hover.
Cage's missing PRIMARY protocol remains the explicit Wayland-only limitation.
One combined tiny-skia X11 run timed out while seeding a PTY row after earlier
clipboard phases, and one loaded tiny-skia cage run timed out during the
pre-existing drag phase; isolated same-code reruns and the complete final
matrix both passed, recording these as shed load timing rather than hiding
them as skips.

The plan/code reviewer first found Canvas padding reusing a stale cell,
diagnostic/native cursor divergence, lossy combining/wide URL projection,
wheel events retaining click counts, option-looking OSC 8 launcher input, and
stale `link-modifier` documentation. Review fixes added grid-vs-padding hit
testing, captured clamping, native cursor mapping, the shared lossless
projection in both Rust UIs, wheel reset coverage, absolute-URI validation,
and documentation updates. A second pass found GTK still on its lossy helper;
GTK then migrated to the shared projection with a real-terminal Unicode
regression. The final interaction-diff review closed with no remaining
findings. A later harness-cleanup review found an ownership/TOCTOU race in
naive socket removal; the fix now requires the stored child to still be live
with the answering PID, waits that exact child, acquires the target lock
non-blocking, refuses a replacement server, and verifies path identity before
both unlinks. Fourteen harness unit tests cover normal cleanup, absent
ownership, exited-child PID reuse, changed live ownership, a held replacement
lock, and a replacement server. The final independent review found one
additional chorded-button capture bug and one stale URL-offset comment.
Secondary presses/releases are now consumed without replacing the initiating
drag/tracking owner, with a focused terminal-pointer regression, and the URL contract now
names Unicode-scalar offsets plus cell-aware projection. The hardened cleanup
itself had no further actionable ownership finding after its documented local
audit, unit coverage, and live functional cleanup proofs.

### Renderer-correctness slice result (2026-08-01)

The released-Canvas rollback is exercised and validated. The focused terminal
widget suite passes on macOS with wgpu and tiny-skia and in the Linux shed with
both renderers under X11 and headless Wayland. It checks a distinctive explicit
cell background at the non-zero `(220, terminal_top)` origin, the
`(0, terminal_top)` collapsed
origin, the base background at the widget origin and far edges, and
shape-independent ASCII signal, CJK fallback across the second logical cell,
and a combining-mark ascent relative to the adjacent plain glyph. A separate Xvfb root
capture of the live tiny-skia window agrees with the in-product capture: the
terminal begins at the sidebar edge instead of the historical doubled origin.
Existing PTY, resize, device-reply, screenshot scaling, and queued-client tests
pass in the same lanes. This is a focused correctness gate in the existing
renderer matrix, not a long-running visual-parity CI suite.

### Next phase: measured UI polish and parity convergence

The next planning phase starts from an explicit three-UI gap inventory, not
from the existence of a working Iced terminal. Capture the same seeded
workspace in Swift, GTK, and Iced at fixed window sizes and record, for each
visible component, current geometry, colors, typography, interaction state,
missing behavior, and the intended reference. Prioritize user-visible shell
structure before decorative detail: window/sidebar/tab/terminal hierarchy,
sidebar width and row density, active/hover/agent states, tab chrome and
controls, terminal padding and font metrics, then palette and transient states.

Each resulting commit must pair presentation work with the behavior it exposes
and prove it under both Iced renderers. Acceptance requires focused geometry or
color assertions, refreshed side-by-side artifacts, keyboard and pointer
interaction tests where relevant, and a named explanation for every remaining
GTK/Swift difference. Full-window pixel equality remains inappropriate, but
subjective resemblance alone is also insufficient. The final POC cannot claim
visual or product parity while the inventory contains an unnamed material gap.

The active measured register, baseline method, ordered closure slices, and
first deterministic-capture contract now live in
[`iced-parity-inventory.md`](iced-parity-inventory.md). It records the stock
Iced control styling and missing direct manipulation paths as P0 gaps; the
walking skeleton must not be described as visually or functionally equivalent
until that register is closed or every remaining difference is explicitly
accepted as toolkit-native.

### Chrome-feasibility slice result (2026-08-01)

The stock-purple shell was an adapter styling choice, not an Iced constraint.
The Iced adapter now owns explicit Roost chrome tokens and styles: a `#282828`
sidebar, one 34-point sidebar/tab seam, compact transparent rows, the shared
deep-blue active-project treatment, project rollup stripes and per-tab dots
derived through `roost-ui-model`, a gray active-agent wash, dark compact tab
pills, a notification badge, and an exact-ID active-tab close control. Only
the project/agent body scrolls vertically; only the pill region scrolls
horizontally, leaving collapse, add-tab, notification, header, and footer
controls fixed and reachable.

The read-only metrics contract exposes optional `terminal_top`; old responses
deserialize without it and GTK continues to omit it, while Iced geometry tests
require a finite positive value instead of copying a band constant. Unit tests
pin style colors, stale close IDs, sibling fallback, and last-tab cascade. The
existing X11 real-input gate constrains the window, reaches the final sidebar
row, manually scrolls long tabs to the exact close control, and proves the
surviving PTY remains usable under wgpu and tiny-skia. Human comparison captures
remain the visual judge; no dedicated long-running parity job was added.

This closes the fundamental styling-feasibility risk: released Iced can render
the intended hierarchy, palette, density, states, clipping, and overflow on
macOS and Linux. It does not yet establish release parity. Project creation and
management, tab rename/reorder and hover-close, sidebar resizing, terminal
scrollback/font controls, palette polish, native notifications/drop adapters,
and accessibility/semantic icon refinement remain named implementation work.
Programmatic selection does not yet auto-reveal an offscreen tab pill; manual
horizontal reachability is proven and auto-reveal stays in the tab-manipulation
slice.

### Palette-feasibility slice result (2026-08-01)

The second visual checkpoint also passes. Iced's stock purple controls have
been removed from the command surface: the adapter owns the reference neutral
surface, selection, hover, disabled, input, border, shadow, and scrollbar
styles. Cards are 660 points wide at the reference fixture, size down to their
content, cap at 500 points, and clamp inside a 640-by-360 window. Command rows
show effective platform shortcut hints; fuzzy matches use the shared model's
Unicode-scalar ranges; agent, notification, generic, and provider/disabled rows
retain distinct semantics instead of being flattened into one button label.

The comparison tool now emits five named palette captures for GTK and Iced
(command, query, agents, notifications, provider) under the same hermetic
workspace. These are local/review artifacts rather than a permanent visual
parity CI job. Focus, scrolling, activation, providers, and notifications stay
covered by correctness tests, and Linux real-input coverage checks the
transparent catcher's inside/outside routing so dismissal cannot click through
to application controls.

The evidence supports continuing Iced as a replacement candidate; it does not
claim that Iced is ready to replace GTK or will necessarily be cheaper to
maintain. The project owner accepted the styling-feasibility result and asked
the work to continue without another approval pause. A polished
direct-manipulation path is the next parity and cost-validation milestone,
where text editing, drag/drop feedback, accessibility, and event-routing
complexity can expose costs that static styling cannot.

### Shared terminal-scroll routing commit plan

Scope: close Iced's P0 local-scrollback gap while removing the Rust UIs'
opportunity to diverge on terminal wheel policy. `roost-vt` will own a small
synchronous scroll state machine that accepts normalized row intent and returns
an explicit mouse-report, alternate-screen key, or local-viewport outcome.
It also owns fractional accumulation and the “typing snaps a scrolled viewport
to bottom” state. GTK will migrate from its adapter-local policy and Iced will
route native wheel events through the same API. Font metrics, PageUp/PageDown
application commands, UI scrollbars, and drag/drop remain separate slices.

The normalized value is `history_rows`: positive means toward older history,
negative means toward the live bottom. Conversion and routing are explicit:

| Source/outcome | Conversion or mapping |
|---|---|
| GTK vertical `dy` | `history_rows = -dy`; GTK reports negative for wheel-up and does not distinguish discrete from smooth units |
| Iced `Lines { y }` | `history_rows = y * 3`; one discrete notch matches the existing Swift three-row policy |
| Iced `Pixels { y }` | `history_rows = y / cell_height`; fractional rows accumulate |
| Mouse tracking | positive = button 4, negative = button 5, one report per whole row |
| Untracked alternate screen | positive = Up, negative = Down, one encoded key per whole row |
| Untracked primary screen | `ScrollViewport::Delta(-history_rows)`; libghostty's negative delta moves toward older history |

Invariants: mouse tracking wins over alternate-screen translation; an
untracked alternate screen receives one encoded arrow press per whole row;
an untracked primary screen moves libghostty's viewport with negative deltas
toward older history; modifier-only and Roost-owned shortcuts do not snap the
viewport; the next terminal keystroke does. UI adapters retain native event
units, pointer geometry, modifier conversion, encoders, and PTY writes. No
toolkit, renderer, callback, or session object enters `roost-vt`, and the Swift
implementation can adopt the same explicit outcome model later without an ABI
or struct-layout dependency.

Every live terminal/tab owns an independent scroll-state instance. Direction
changes discard stale fractional momentum. After a local viewport move, the
state reads libghostty's authoritative scrollbar (`offset + len >= total`
means bottom) instead of guessing from the request size; a partial move toward
bottom therefore remains scrolled. Explicit bottom snap clears accumulation
and state. Output, mouse-report, alternate-screen, modifier-only, and
Roost-owned shortcut paths do not clear another tab's state.

Tests and acceptance: pure route/accumulator/direction-change tests and
libghostty viewport/snap tests pin the shared API; GTK unit and functional
regressions stay green after migration; Iced widget/app tests cover line and
pixel wheel normalization, local history, alternate-screen arrows,
mouse-report precedence, and terminal-key snap. The Linux X11 real-input gate
uses physical wheel events under wgpu and tiny-skia to prove visible history,
tracking bytes, alternate-screen bytes, and return-to-bottom behavior. Existing
Wayland real-seat uinput coverage attempts the same axis path; the concrete
cage limitation and equivalent proof are recorded below rather than skipped.
Cross-tab isolation and partial-scroll-down-then-key behavior are explicit
regressions. macOS Iced E2E, Swift/GTK suites, dependency boundaries, and the
complete per-commit gate must remain green before the focused commit is pushed
and Actions verified.

Wayland validation evidence: the shed's standalone relative uinput mouse is
recognized by libinput 1.25 as a pointer and produces a valid
`POINTER_SCROLL_WHEEL` event with legacy and high-resolution detents. Cage's
headless + libinput backend did not forward that hot-plugged axis event to its
Wayland client in four variants (absolute-device axis, separate relative
device, explicit pointer property, and relative-device focus motion), although
the same seat continues to forward motion, buttons, drags, and multi-clicks.
The committed Wayland gate therefore remains strict for the input types cage
can deliver and does not add a skip. The renderer-specific equivalent for
wheel is the complete physical X11 route under both wgpu and tiny-skia, plus
backend-neutral Iced event-normalization tests, shared libghostty route tests,
and the existing Wayland renderer/pointer suite. `inject_wheel.py` remains as a
small reusable diagnostic for a future real-compositor/physical-seat gate.

Validation result: `make check` passed the warnings-denied Rust lint, complete
Rust workspace, GTK tests, 688 Swift tests, and harness unit tests. macOS Iced
E2E passed under both wgpu and tiny-skia (49 passed, one documented Linux-only
PRIMARY-selection skip per renderer). The final shed gate passed GTK's physical
Wayland drag/reorder guard, Iced's X11 real-input guard including all three
wheel routes and key snap, and Iced's physical Wayland clipboard, drag,
multi-click, and link-hover guard. During the audit, the Wayland drag fixture
exposed uinput move/press coalescing; explicit scheduling fences made the
physical gesture deterministic in two focused reruns and the complete shed
gate without weakening its selection assertion.

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
| Iced terminal text misaligns graphemes | fixture screenshots and measured cells in both backends | retain the custom primitive widget and adjust its renderer-neutral shaping/metrics |
| Iced 2x capture on a native 1x display has no public arbitrary-scale render target | normalize the public renderer capture to the required dimensions; pixel/geometry gates run at 1x and smoke artifacts validate 2x | accept nearest-neighbor 2x enlargement for this POC, while Retina captures retain native 2x detail |
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
