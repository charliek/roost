# Test automation & scripting architecture (plan)

Status: **largely implemented** (2026-05). The functional E2E harness
(pytest) is built and headless in CI as [`tools/roosttest/`](../../tools/roosttest/README.md);
the tooling is reorganized into three layers (see
[`tools/README.md`](../../tools/README.md)); and `tab.dump`, `wait`, and
the `palette.*` ops shipped. The plan below is kept as the design
rationale — the "Gap" entries it lists are mostly closed now. The Lua
scripting layer (§ below) remains the open piece. The north star is
canonical in [vision.md](vision.md#the-command-core-north-star); §0 here
is the testing-lens recap.
Audience: Claude (primary) + the maintainer. Targets: this Mac, Macs in
general, the Pop!_OS (COSMIC/Wayland) box, and CI (Linux + macOS runners).

This doc plans two intertwined things the maintainer asked to grow together:

1. **A Lua scripting layer in `roostctl`** that can set up and mutate
   application state (projects, tabs, focus, …) in multi-step actions —
   surfaced to users through the Cmd/Alt+Shift+T launcher, and reused
   wholesale by tests.
2. **Functional, automated tests that exercise the real app on both UIs,
   in CI**, giving confidence that basic flows work on every change.

The thesis: these are the same substrate. A control protocol rich enough
to script the app for a power-user launcher is exactly what an
automated test driver needs. Build the substrate once; let the launcher
and the test suite both stand on it.

---

## 0. North star

Every way to drive Roost — **mouse/clicks, hotkeys, the CLI, and Lua
scripts** — converges on **one core: the workspace operation set**
(open/close/focus tab, create/rename/delete/reorder project, set-state,
notify, dump, … plus a few view ops like screenshot / open-palette).
Each surface is a *thin adapter* onto that core; the UI is a **reaction**
to the core's events, never its own source of truth.

```
  roostctl (CLI) ─┐
  Lua scripts ────┤──▶ IPC handler ──┐
                                      ├─▶  workspace op set  ──emit──▶ events ──▶ UI re-renders
  mouse / clicks ─┐                   │       (THE CORE)
  hotkeys ────────┤──▶ UI dispatch ───┘
```

- **CLI + Lua** are out-of-process → reach the core over the IPC socket
  (the handler is their adapter; Lua sits on top of the same op set).
- **Clicks + hotkeys** are in-process → call the same op set directly
  (their adapter is the UI command / keybind handler).
- A hotkey (`Cmd+Shift+T`), a `roostctl` call, and a Lua script all
  invoke the **same** command — e.g. "run action" or "open tab".

**One contract, two implementations.** There is no shared *codebase*
core — Swift and Rust can't share one. There is one shared **contract** —
the IPC op set in `roost-ipc` — implemented by **Swift `Workspace` +
AppKit** and **Rust `Workspace` + GTK**. "Same interface" means same op
contract + behavioral parity, which the cross-platform E2E suite (below)
exists to enforce. Per platform: identical command surface,
platform-specific guts (`forkpty` vs `portable-pty`, Core Graphics vs
Cairo).

**Two seams** (both firmed up in the IPC refactor on #106):

1. **surfaces → core** (commands in): CLI/Lua via IPC, UI/hotkeys direct.
   *The convergence goal — partially there; every UI/hotkey action should
   route through the op set, not divergent local logic.*
2. **core → UI** (view reach-back: screenshot/dump/activate): GTK's one
   `UiRequest` channel, Mac's one `UiBridge` seam.

**Why this is the north star:** it buys the three things we optimize for
at once —

- **Testability** — tests drive the same op set users do and assert on
  its events/state; no test-only backdoors that drift from reality.
- **Programmability** — the op set *is* the public surface; Lua actions
  and the launcher are first-class clients of it, same as the CLI.
- **Clean architecture** — one place owns each mutation; the UI is a pure
  projection of core state; adding a capability means adding an op + thin
  adapters, not bespoke logic per surface.

Every decision below (and in P2+) is measured against this: *does it
route through the one op set, keep the UI reactive, and stay at parity
across both implementations?*

---

## 1. Goals & non-goals

**Goals**

- Drive the running app deterministically from outside the process and
  assert on the result — terminal **content** (text), workspace **state**
  (tabs/projects/agent-state/notifications), and **rendering** (pixels).
- Run a functional E2E suite **headless in CI** on both UIs.
- A Lua scripting surface in `roostctl` for multi-step state setup /
  mutation, shared by the launcher and tests.
- Zero-to-few runtime dependencies; cross-platform (macOS + Linux);
  legible and extensible by an agent.
- Kill `sleep`-based flakiness: wait on conditions, not wall-clock.

**Non-goals (for now)**

- Pixel-perfect golden-image diffing of the whole window. We assert
  content via text and rendering via *targeted* color/þcell checks.
- Testing the OS-level input encoders (key/mouse → bytes) in CI. That
  stays a **local** smoke (see Tier 2), because uinput/CGEvent injection
  needs privileges / Accessibility and a real compositor.
- Replacing the unit/integration tests. They stay the fast first line.

---

## 2. Current state

| Layer | What exists | Gap |
|---|---|---|
| Unit / integration | `cargo test --workspace` (Rust: IPC, OSC, vt, target picker, persistence) + `swift test` (190 tests: Workspace state machine, IPC dispatch, persistence) | No coverage of the *live* app (PTY, rendering, IPC end-to-end). |
| IPC surface | `roost-ipc`: tab/project CRUD, set-state, notify, focus, send, resize, reorder, screenshot, claude-hook, identify — **now also `tab.dump` (content), `palette.*` (UI-action), and `roostctl wait`**. | Copy/paste + live `events.subscribe` still unimplemented. |
| Event stream | UIs consume an **in-process** event bus. `events.subscribe` over the wire is **stubbed not-implemented on both UIs** (`mac/Sources/Roost/IPCHandlerImpl.swift`, `crates/roost-linux/src/ipc.rs`). | External clients can't wait on events yet (the pytest harness condition-waits via polling instead). |
| Render state | `roost-vt` `RenderState.walk(|cell| …)` yields `Cell { text: String /*grapheme*/, fg, bg }` + cursor; mirrored 1:1 in `mac/Sources/Roost/RenderState.swift`. Both UIs walk it to draw. | **Now exposed over IPC as text via `tab.dump`** (viewport only; scrollback is a follow-up). |
| Tooling | Three layers (see [`tools/README.md`](../../tools/README.md)): `tools/roosttest/` (pytest, IPC, **in CI**), `tools/screenshot/` (bash + roostctl + pngtool, visual), `tools/input/linux/` (uinput/clipboard, real input). | Real-input (Layer 3) + visual (Layer 2) are local-only; a Mac CGEvent injector is still to come. |

This unified design is now realized: the pytest harness (`tools/roosttest/`)
is Tier 1, and the screenshot + input harnesses are reorganized by layer.
The Lua scripting substrate below is the remaining piece.

---

## 3. Principles

- **Robustness lives in the driver + app affordances, not the test
  language.** Flake-resistance comes from (a) waiting on the event
  stream, (b) reading content as text, (c) reproducible rendering, and
  (d) driving via IPC instead of OS input. These are shared no matter
  what language the test cases are written in. (This reframes the
  language question — see §7.)
- **Drive through the control protocol.** The IPC socket is the seam.
  Driving via IPC (not synthetic keystrokes) is deterministic, headless,
  and — critically — needs **no macOS Accessibility (TCC) grant and no
  Wayland pointer mapping**, which is what makes "Mac E2E in CI" tractable.
- **Determinism by construction.** A test mode pins window geometry, font,
  and animations so screenshots and reflow are reproducible across
  machines and DPI.
- **One substrate, two consumers.** The Lua/IPC verbs power both the
  launcher and the tests. Tests can invoke a launcher action and assert
  its effect — the feature tests itself.

---

## 4. Testing tiers

| Tier | What | Where it runs | Speed |
|---|---|---|---|
| **0 — unit/integration** | `cargo test`, `swift test`. Pure logic: state machine, IPC dispatch, OSC, persistence, key-encoder tables. | CI (exists) + local | seconds |
| **1 — functional E2E** | Launch the **real** app; drive via IPC/Lua; assert via `tab dump` (text) + `tab list` (state) + targeted screenshot color checks. Covers: open project/tab, run a command and read its output, state→color, notification + badge, focus switch, session restore, cascade-close, launcher actions. | **CI on both UIs** (Linux xvfb, macOS GUI session) + local | seconds–low minutes |
| **2 — real-input smoke** | OS-level key/pointer injection (`tools/input/linux` uinput; a Mac CGEvent/AppleScript equivalent) exercising the *encoder + gesture* path, verified by screenshot. | **Local only** (Pop!_OS, Mac) | minutes, manual-ish |

Tier 1 is the new center of gravity and the CI confidence-builder. Tier 2
stays local because injecting real input needs privileges/Accessibility
and a live compositor — not worth the CI fragility when Tier 1 already
covers behavior.

---

## 5. App / CLI / IPC refactors (the affordances)

These are the enabling changes. All are additive to the wire protocol.

### 5.1 `tab dump` — terminal content as text  *(highest leverage)*

New IPC op + `roostctl tab dump --tab N [--scrollback] [--json]`.

```jsonc
// request:  {"op":"tab.dump","params":{"tab_id":61,"scrollback":false}}
// response:
{
  "cols": 120, "rows": 30,
  "cursor": {"row": 1, "col": 14, "visible": true},
  "rows_text": ["/private/tmp $ echo hi", "hi", "/private/tmp $", ""]
  // optional --json adds per-cell fg/bg for color assertions
}
```

Implementation: walk the existing `RenderState` (`Cell.text` per cell,
concatenated per row, trailing blanks trimmed) on each UI's main thread —
the same walk both renderers already do. This is the determinism unlock:
tests assert exact text (`assert dump.contains("hi")`) instead of OCR or
pixel-matching. **Low risk; both UIs already have the walk.**

### 5.2 `events.subscribe` over the wire + `roostctl wait` / `events`

Implement the currently-stubbed `events.subscribe` op on **both** UIs:
bridge the in-process event bus to the IPC connection (the GTK side
already has the in-process `events.subscribe()`; the Mac side has the
`@MainActor` event stream `App.swift` consumes — both need a wire fan-out).

Then:

- `roostctl events --follow` — stream events as JSON lines (debugging +
  driver consumption).
- `roostctl wait --tab N --state idle --timeout 5` (and `--tab-count`,
  `--notification`, `--project-count`) — block until satisfied or exit
  non-zero on timeout.

**Fallback if wire-events slip:** `wait` can poll `tab list`/`identify`
on an interval initially; swap to event-driven once subscribe lands. The
*interface* (`roostctl wait …`) is stable either way, so tests don't
churn. Either way, **no test ever calls `sleep`.**

### 5.3 IPC UI-action ops

Expose the actions currently reachable only by keyboard/mouse so tests
(and Lua actions) can trigger them without synthetic input:

- `ui.open_launcher`, `ui.open_palette`, `ui.dismiss_overlay`
- `tab.copy` / `tab.paste` (drive the clipboard path deterministically)
- (later) `ui.select_palette_item`, query overlay state for assertions

Each maps to the same handler the keybind already calls. This is what
lets **Mac E2E avoid TCC** entirely.

### 5.4 Test mode (`ROOST_TEST=1` or `--test-mode`)

Make rendering reproducible: fixed window geometry (e.g. 1200×800 logical),
a bundled fixed monospace font, animations off, and never steal OS focus.
Screenshots then match across machines/DPI; reflow is deterministic.
Optionally normalize the shell prompt for content tests (or tests just set
`PS1` via `tab send`). Gated so it never affects normal runs.

### 5.5 Wire/versioning notes

All ops are additive; bump nothing that breaks existing clients. `tab
dump`'s `--json` cell schema is the one place to design for forward
compat (optional fields). Document each new op in
[`docs/reference/ipc.md`](../reference/ipc.md).

---

## 6. Lua scripting layer

### 6.1 Engine & placement

Embed Lua in **`roostctl`** (Rust) via **`mlua`** (Lua 5.4). Dependency
justification per `CLAUDE.md`: no pure-Rust Lua is production-grade
(`hematita`/`piccolo` are immature); `mlua` is the mature, widely-used
binding. Constraint named, wrapper kept small — the engine only exposes a
curated `roost` table that forwards to the existing IPC client.

**The UIs do not embed Lua.** The Cmd/Alt+Shift+T launcher runs an action
by shelling out to `roostctl run <action.lua>`, which scripts the running
UI back over IPC. One Lua host (the CLI), identical code path for launcher
actions and tests.

```
 launcher (Mac/GTK UI) ──exec──▶ roostctl run action.lua ──IPC──▶ UI workspace
 test runner ───────────────────▶ roostctl run test.lua    ──IPC──▶ UI workspace
```

### 6.2 API surface (sketch)

```lua
-- queries
roost.identify(); roost.projects(); roost.tabs()
local d = roost.dump(tab)            -- {cols,rows,cursor,rows_text=…}
-- mutations
local p = roost.create_project{name="review", cwd="/repo"}
local t = roost.open_tab{project=p.id, cwd="/repo", title="build", cmd="…"}
roost.set_state(t, "running"); roost.focus(t); roost.notify{tab=t, title="…"}
roost.send(t, "echo hi\n"); roost.close_tab(t)
-- synchronization (no sleeps)
roost.wait{tab=t, state="idle", timeout=5}
roost.wait_for(function() return #roost.tabs() == 2 end, 5)
-- rendering (Rust-backed, in-process screenshot)
roost.screenshot{out="/tmp/x.png", scale=2}
roost.pixel(x, y); roost.find_color("#f0a040")   -- locate a UI element
-- assertions
expect(cond, "msg"); expect_eq(a, b); expect_contains(d.rows_text, "hi")
```

The same primitives express a **launcher action** ("spin up my review
layout: project + 3 tabs running these commands") and a **test**
("open a tab, send `echo hi`, wait for prompt, assert dump contains hi").

### 6.3 Launcher integration (the product feature)

Actions are named Lua scripts discovered from config (e.g.
`~/.config/roost/actions/*.lua` and/or a repo-local `.roost/actions/`).
The launcher lists them; selecting one runs `roostctl run` against the
current UI. Built-ins ship in-tree. Config format + discovery to be
specified in the launcher PR.

### 6.4 Trust / safety

Lua actions run arbitrary local code (they can spawn shells via
`tab send`). That's acceptable for **local, user-authored** scripts — same
trust level as a shell rc. We do **not** execute actions from untrusted
sources, and the IPC socket stays user-only (0600, already the case). No
network in the exposed `roost` table.

---

## 7. Test-language decision  *(decided 2026-05-26)*

**Decision: pytest drives the tests; Lua is a scoped user-scripting
surface, not the test mechanism** (see
[vision.md DL-12](vision.md#dl-12-pytest-drives-the-tests-lua-is-a-user-scripting-surface)).
The analysis that led there is kept below; the key insight that made it
low-stakes is that E2E robustness lives in the *affordances*, not the
runner.

**What actually drives E2E robustness** (flake resistance, good failures):

| Robustness factor | Comes from | Language-dependent? |
|---|---|---|
| No sleeps / wait-on-condition | `roostctl wait` + event stream (§5.2) | **No** |
| Deterministic content assertions | `tab dump` (§5.1) | **No** |
| Reproducible rendering | test mode (§5.4) | **No** |
| No TCC/uinput flake | drive via IPC (§5.3) | **No** |
| Clear failure output (expected vs actual) | the runner | **Yes** |
| Fixtures / setup-teardown / parametrize | the runner | **Yes** |
| Reporting (JUnit/HTML), retries, timeouts, parallel | the runner | **Yes** |
| Maintenance burden of the runner itself | the runner | **Yes** |

So: **the flake floor is identical** for Lua or Python — it's set by the
shared affordances. The language only changes *ergonomics and reporting*,
plus *how much harness code we own*.

| Option | Pros | Cons |
|---|---|---|
| **Lua runner** (in `roostctl`) | One language; **zero runtime deps** (just the binary) — ideal for CI + an agent; dogfoods the launcher; same helpers as actions | We hand-roll the runner (discovery, fixtures, JUnit XML, timeouts) — new code we own; thinner ecosystem |
| **pytest** | Mature fixtures/parametrize/reporting/retries; assertion introspection; reuses #103's Python | Python runtime on every box + CI (cheap, but real); a second language; separate from the app |
| **Hybrid** (pytest runner over the shared roostctl/Lua/IPC layer) | pytest ergonomics **and** the Lua launcher; can E2E-test launcher actions; clean role split — **Lua = what the app does, Python = how we assert** | Two languages to keep coherent; most moving parts |

**The decision: pytest as the test runner; Lua scoped to user
scripting.** Tests are pytest over the IPC op set (plus `roostctl` /
shell for the simplest cases) — its fixtures, parametrization over the
2-UI matrix, and reporting cut the harness code we'd otherwise own, and
the flake-killing affordances (`roostctl wait`, `tab dump`) live in the
app, so the runner choice doesn't move the robustness floor. **Lua is
deliberately *not* the test runner.** It is a user-facing scripting
surface — the Cmd+Shift+T launcher and complex user-authored multi-step
actions — added where it earns programmability and not over-invested as
test infrastructure. Both stay thin adapters onto the same op set: a
pytest step and a Lua action invoke identical ops, so neither can drift
from what users actually drive.

Concretely: `pytest` fixtures launch/quit each UI and yield a thin Python
`Roost` client (wraps the socket + `roostctl`); tests assert with plain
`assert`; where a test needs to exercise the launcher path, it runs the
Lua action and asserts the resulting state via the op set.

> **DECISION (2026-05-26):** ☑ **pytest** runner for tests + **scoped
> Lua** for user scripting. Supersedes the earlier "hybrid (pytest +
> heavy Lua)" lean now that programmability + clean-architecture are
> explicit north-star goals; Lua's role narrows to user-facing.

---

## 8. CI design — Linux + Mac E2E

The maintainer chose **both platforms now**. Feasible because Tier-1
drives via IPC + in-process screenshot (no TCC, no compositor capture).

**Linux (GTK):**
- Runner: `ubuntu-latest`. Deps: `libgtk-4-dev libadwaita-1-dev`, the
  ghostty prebuild (reuse the existing gtk CI cache), Python (hybrid) or
  nothing (Lua-only).
- Display: `xvfb-run -a` with `GDK_BACKEND=x11` (the Cairo/Pango
  `GtkDrawingArea` renders fine under Xvfb; in-process `screenshot`
  doesn't need a compositor). Headless Wayland (`weston --backend=headless`
  / `sway --headless`) is a fallback if an X11-only quirk appears.
- Run: build `roost` + `roostctl`, launch under `ROOST_TEST=1`, run the
  Tier-1 suite via `tools/screenshot/launch.sh gtk` → runner.

**macOS:**
- Runner: `macos-latest` (GUI session present; AppKit windows work).
- Build + bundle (or run the unbundled `swift run Roost` — TBD which is
  lighter for tests; the IPC socket comes up either way). Launch under
  `ROOST_TEST=1`; the in-process renderer works unfocused, so no
  screencapture entitlement and **no Accessibility grant** (we never inject
  OS input in Tier 1).
- Risk was app launch/quit hygiene and runner image quirks. **Resolved and
  promoted** (2026-05): `e2e-mac` is now **required** (in `ci-success`). It
  proved stable on `main` (26P/1S, ~8s); the only failures ever seen were a
  real clean-install crash it *correctly caught*. The one cascade mode — a
  crashed instance leaving a held single-instance flock that wedges the next
  launch — is handled by pre-launch hygiene in the harness
  (`tools/roosttest/ui.py` `_mac_cleanup()`: kill any leftover, then unlink
  the stale socket + lock) plus a one-shot launch retry; timeouts scale via
  `ROOST_TEST_TIMEOUT_SCALE=3` on the slower runner. **No pytest reruns** —
  parity with `e2e-gtk`, so a genuine intermittent bug isn't masked.

**Both:**
- Path-filtered like the rest of `ci.yml` (run only when relevant code
  changes). Cache cargo + ghostty. Emit **JUnit XML** (hybrid: pytest
  `--junitxml`; Lua: runner emits it) for GitHub test annotations. Upload
  screenshots + `manifest.md` as artifacts on failure.
- Keep Tier 0 as the fast gate; Tier 1 runs after build.

---

## 9. Determinism strategy

- **Content:** assert via `tab dump` text, not pixels. Normalize the shell
  (set `PS1`, `clear`) or run a fixed `cmd` in the tab so output is stable
  (avoids the `👻`-prompt variability seen in manual testing).
- **Rendering:** Tier-1 pixel checks are *targeted* — "the cell at the
  needs_input pill is amber `#f0a040`" via `find_color`, not whole-window
  diffs. Test mode fixes geometry+font so even those are stable.
- **Timing:** only `roostctl wait` / `wait_for`. No `sleep` in any test.
- **Isolation:** each test creates its own project and cascade-closes it
  (the smoke already does this); a fixture guarantees cleanup even on
  failure. Consider a `state.json` pointed at a temp dir per run so tests
  never touch the dev workspace.

---

## 10. Relationship to #103 / #104

- **#104 `tools/screenshot/`** (bash smoke): keep. Its scenario *shape*
  (create project → states → notify → focus → hook → cascade) becomes the
  first Tier-1 cases. The bash version remains a zero-setup smoke until the
  runner supersedes it.
- **#103 `tools/input/linux/`** (uinput/PNG/clipboard): keep as the **Tier-2**
  real-input layer. Its `pngtool` logic informs `roost.pixel`/`find_color`;
  its uinput injector is the Linux half of the Tier-2 smoke. A Mac CGEvent
  equivalent is the other half (later).
- **Land both now**, resolve the one-line `CLAUDE.md` Troubleshooting
  conflict (both add adjacent bullets — they coexist). The unified harness
  lands separately, on top of §5.

---

## 11. Risks & mitigations

| Risk | Mitigation |
|---|---|
| macOS app won't run cleanly in CI | **Resolved (2026-05):** promoted to required after proving stable. Drives via IPC (no TCC); the harness clears a stale lock/socket before launch and retries the open once; timeouts scale via `ROOST_TEST_TIMEOUT_SCALE`. |
| `events.subscribe` wire work is bigger than hoped | Ship `roostctl wait` polling-backed first; swap to events later behind the same interface. |
| `tab dump` differs subtly Mac vs GTK | Golden the dump format in a cross-UI test (same `cmd`, assert identical `rows_text`); both walk the same `RenderState` shape. |
| Lua (`mlua`) C-dep friction in CI | It builds vendored Lua; cache the cargo artifacts; it only lands in `roostctl`, not the UIs. |
| Two harness entry points confuse future work | This doc + a single `tools/README.md` map (Tier 0/1/2) once the unified harness lands. |
| Screenshot flake across DPI/machines | Test mode pins geometry+font; prefer text assertions; targeted color checks only. |

---

## 12. Phased rollout

Each phase is an independently reviewable PR (or a small stack), gated on
green CI, merged manually per branch policy.

- **P0 — coordination.** Land #103 + #104 (resolve `CLAUDE.md` conflict).
  *Done when:* both merged, `ci-success` green.
- **P1 — content + waiting (the backbone).** `tab dump` (IPC + both UIs +
  `roostctl`); `events.subscribe` over the wire (both UIs) + `roostctl
  wait`/`events`; unit tests for dump + a Rust/Swift test for the wire
  event fan-out. *Done when:* `roostctl tab dump` and `roostctl wait` work
  against both UIs locally; no `sleep` needed to observe a state change.
- **P2 — test mode + Tier-1 harness skeleton.** `ROOST_TEST` (fixed
  geometry/font/no-anim); the runner (per §7 decision) + 3–4 ported
  smoke cases; runs locally on both UIs. *Done when:* `make e2e` (or
  `roostctl test`) is green locally on Mac + GTK.
- **P3 — CI.** ✅ Linux xvfb E2E job (required) + macOS E2E job. JUnit +
  artifact upload. *Done:* Tier-1 runs on PRs touching relevant paths, and
  the macOS job was promoted to **required** (`ci-success`) once stable;
  releases gate on the same `ci-success` via `release.yml`'s `ci-gate`.
- **P4 — Lua engine.** `mlua` in `roostctl`; the `roost` API table;
  `roostctl run <script.lua>`; convert the Tier-1 helpers to use it (or
  the Lua smoke). *Done when:* a Lua action script can set up a
  multi-tab layout end-to-end.
- **P5 — launcher actions.** Wire Cmd/Alt+Shift+T to discover + run Lua
  actions via `roostctl run`; ship a couple of built-in actions; docs.
  *Done when:* selecting a launcher action mutates the live workspace.
- **P6 — Tier-2 real-input smoke + consolidation.** Fold #103 into the
  Tier-2 layer; add the Mac CGEvent injector; write the `tools/README.md`
  tier map; decide #104's fate. *Done when:* a real-keystroke smoke passes
  locally on Pop!_OS and Mac.

P1 is the linchpin — everything downstream leans on `tab dump` + `wait`.

---

## 13. CLAUDE.md updates (written for the agent)

When this lands, `CLAUDE.md` Troubleshooting/Testing should tell an agent,
prescriptively:

- **To verify a change on the live app:** `tools/screenshot/launch.sh <mac|gtk>`,
  then drive with `roostctl` (`tab dump` to read content, `wait` to
  synchronize, `screenshot` to see). Never `sleep`.
- **To run the functional suite:** the one command (`roostctl test …` or
  `pytest tools/roosttest -m e2e --target <mac|gtk>`), and how to read the
  JUnit/artifacts.
- **To add a test:** where cases live, the fixture that gives a clean
  workspace, and the assertion helpers (`dump`, `tab list`, `find_color`).
- **To add a launcher action:** where actions live and the `roost` Lua API.
- **Tier map:** 0 = `cargo/swift test`; 1 = functional E2E (CI, both UIs);
  2 = local real-input (`tools/input/linux` + Mac equivalent).

The guiding rule for these docs: an agent should be able to go from "I
changed X" to "here's the exact command that proves X still works" without
guessing.

---

## Open decisions

1. ~~**Test runner language**~~ — **DECIDED (§7 / DL-12): pytest runner;
   Lua scoped to user scripting.** *Unblocks P2.*
2. ~~**macOS CI launch**~~ — **DECIDED (2026-05): bundle via
   `bundle.sh` + launch through `open`; the macOS E2E job is now
   required.** The harness clears a stale lock/socket before launch and
   retries once; timeouts scale via `ROOST_TEST_TIMEOUT_SCALE`. *Unblocked P3.*
3. **Launcher action discovery** — global (`~/.config/roost/actions/`),
   repo-local (`.roost/actions/`), or both; built-ins in-tree. *Blocks P5.*
4. **Temp-workspace isolation** — point tests at a throwaway `state.json`
   dir, or rely on create/cascade-close hygiene. *Blocks P2.*
