# Test automation

How the [north star](vision.md#the-command-core-north-star) gets
verified: tests drive the **same workspace operation set** users and
`roostctl` drive, and assert on its events and state. No test-only
backdoors that can drift from what ships.

Audience: Claude (primary) + the maintainer. Targets: dev Macs, the
Pop!_OS (COSMIC/Wayland) box, and CI (Linux + macOS runners).

## The three layers

[`tools/README.md`](https://github.com/charliek/roost/blob/main/tools/README.md)
is the source of truth for this map; the summary:

| Layer | Dir | Drives via | Verifies | Where it runs |
|---|---|---|---|---|
| **1 — functional** | [`roosttest/`](https://github.com/charliek/roost/blob/main/tools/roosttest/README.md) | JSON IPC (a thin Python client) | the op set — `tab.dump` / `tab.list` / `palette.*` / `identify`: behavior + content as text | **CI, both UIs** + local |
| **2 — visual** | [`screenshot/`](https://github.com/charliek/roost/blob/main/tools/screenshot/README.md) | `roostctl` + `roostctl screenshot` | pixels: colors, badges, cursor, reflow, which tab/sidebar is shown | local |
| **3 — real input** | [`input/linux/`](https://github.com/charliek/roost/blob/main/tools/input/linux/README.md) | OS key/pointer injection (uinput) | the *real* key-encoder + mouse-gesture + clipboard path | local + one soft CI lane |

Reach for the highest layer that can answer the question. Layer 1 is
where most new coverage lands: it needs no compositor, no macOS
Accessibility (TCC) grant, and no Wayland pointer mapping, which is
exactly what makes "Mac E2E in CI" tractable at all. `cargo test` /
`swift test` remain the fast first line below all three;
[`tools/perf/`](https://github.com/charliek/roost/blob/main/tools/perf/README.md)
measures *cost* on a sibling axis rather than correctness.

## Principles

- **Robustness lives in the driver + app affordances, not the test
  language.** Flake resistance comes from waiting on conditions, reading
  content as text, reproducible rendering, and driving via IPC instead
  of OS input — none of which depend on what the cases are written in.
- **Drive through the control protocol.** The IPC socket is the seam.
  Every UI action reachable only by keyboard or mouse gets an op
  (`palette.*`, `tab.copy`/`tab.paste`, `screenshot`) so a test can
  trigger it without synthetic input.
- **No test ever calls `sleep`.** Wait on a condition: `roostctl wait`
  (`--state`, `--text`, `--gone`) or the harness's `wait_*` helpers.
  `events.subscribe` is still `not-implemented` on both UI sockets
  (only a host session pushes), so `wait` is poll-backed today — the
  *interface* is what tests depend on, so swapping in wire events later
  changes no test.
- **Content as text, pixels only when targeted.** Assert with
  `tab.dump`, not OCR or whole-window image diffs. Layer-2 checks are
  targeted color probes ("this cell is amber `#f0a040`"), never golden
  full-frame comparisons.
- **Isolation by construction.** Each test creates its own project and
  cascade-closes it, *and* a harness-launched UI runs against a
  throwaway `ROOST_STATE_DIR` — a run never touches the developer's real
  saved tabs.

## Test-mode IPC ops

`ROOST_TEST_MODE=1` at UI launch (CI sets it; `make *-ci` sets it)
unlocks ops that would otherwise be a footgun. Both gated ops answer
`not-enabled` when the flag is absent — a deterministic error rather
than silent acceptance. The flag is read once at boot, so a tester
cannot toggle the gate mid-session.

| Op | What it does | Gated |
|---|---|---|
| `tab.feed_pty_bytes` | Inject bytes into a live tab's PTY-output drain — indistinguishable from real shell output to the OSC scanner and libghostty, because it rides the same channel (no shadow drain). | yes |
| `tab.capture_pty_input` | Read (and optionally drain) the bytes the UI queued onto a tab's PTY-input side — keystrokes, paste, synthesized OSC replies. One tap point catches everything. | yes |
| `tab.dump_resolved` | Walk the viewport through the same `resolve_cell_colors` call the production paint path runs. | no |

Together the first two cover byte-level OSC and reply wiring end to end
without needing a real shell — the pattern walk is in
[`tools/roosttest/README.md`](https://github.com/charliek/roost/blob/main/tools/roosttest/README.md).
`tab.dump_resolved` is ungated for the same reason the gated pair is
vision-compliant: it is a richer *read* of an existing surface, not a
new one.

A sibling gate, `ROOST_TEST_PANIC`, forces the crash-report + abort path
in the Rust UI end to end (`=1` panics on the main thread at startup,
`=thread` from a named background thread). It fires right after the
panic hook is installed and before the single-instance lock, so it never
touches a running instance.

## Harness flags

Full operational detail lives in
[`tools/roosttest/README.md`](https://github.com/charliek/roost/blob/main/tools/roosttest/README.md).

| Knob | Set by | Effect |
|---|---|---|
| `ROOST_TEST_MODE=1` | CI; `make *-ci` | Unlocks the gated ops above. Read once at UI boot. |
| `--roost-fresh` / `ROOST_TEST_FRESH=1` | `make *-ci`; CI | The harness **owns** a hermetic instance: force-quit any running UI, launch with isolated state, always quit at teardown. Also flips precondition-skips into hard failures. |
| `ROOST_STATE_DIR` | harness (per-run `mkdtemp`) | Production env, both UIs. Redirects only `state.json`'s directory; socket/lock/log stay on the default path so the harness still finds the UI. |
| `ROOST_DEFAULTS_SUITE` | harness (Mac) | Production env, Mac only. Redirects `UserDefaults` (sidebar visibility/width) to a throwaway suite — the analog of `ROOST_STATE_DIR`, which cannot reach it. |
| `ROOST_TEST_TIMEOUT_SCALE` | CI (slower runners) | Scales every `wait_*` budget. Local default 1. |
| `ROOST_CONFIG` | harness | Points the UI at the seeded fixture config (launcher commands, theme). |
| `ROOST_ICED_APP` | `make e2e-iced-bundle`; CI | Drives the iced target through a real `.app` bundle via LaunchServices instead of a bare binary. |

### The skip policy (the trustworthiness rule)

A `skip` must mean only *"this environment genuinely cannot exercise
this"* — never "the setup didn't work" or "we didn't turn the mode on."
Three helpers in `tools/roosttest/util.py` enforce it:

- `precondition(ok, reason)` — a **setup** precondition (seed config
  present, OSC 7 cwd tracking working) is a hard failure in fresh mode,
  where the harness guarantees the environment; a graceful skip
  otherwise, since an ad-hoc dev UI may genuinely lack the capability.
- `skip_on_ci(reason, alt_coverage=…)` — for the rare test that cannot
  run remotely. It **must** cite where the regression class is otherwise
  covered.
- `cwd_reaches(...)` — the shared, scaled cwd poll, replacing per-file
  copies that ignored `ROOST_TEST_TIMEOUT_SCALE`.

Every run prints a `SKIPS: N` summary listing each skipped test and its
reason (`conftest.py::pytest_terminal_summary`), so a run that quietly
skipped half the suite can never read as "all green" — the failure mode
that motivated the rule.

## Make targets

| Target | What it runs |
|---|---|
| `make test` | `cargo test --workspace` (+ the `roost-vt` ffi tests) + `swift test` + the harness's own unit tests |
| `make e2e` | Dispatch on `ROOST_TARGET` (`mac` \| `iced`, default `iced`) to one of the two below |
| `make e2e-iced` / `make e2e-mac` | Quick local functional runs — reuse a running UI if one is present |
| `make e2e-iced-ci` / `make e2e-mac-ci` | CI parity: `ROOST_TEST_MODE=1` + `--roost-fresh`, so a local run exercises the **same set CI does**. Both are **destructive** (they force-quit a running UI) and labeled accordingly. |
| `make e2e-iced-exit` / `make e2e-iced-menu-quit` | Lifecycle lanes in their own runs — the UI they drive exits, so they cannot share a session-scoped fixture |
| `make e2e-iced-bundle` / `make e2e-iced-sparkle` | macOS only: assemble `Roost-Iced.app` (test-keyed, for Sparkle) and run the curated smoke against the real bundle |
| `make test-iced-real-input` / `make test-iced-wayland-input` | Layer 3 — X11 (Xvfb + xdotool) and Wayland (cage + a real uinput seat) |
| `make smoke-iced` / `make smoke-mac` | Layer 2 — screenshot-driven UI smoke against a running UI |

`make e2e` **dispatches** rather than running the whole `tools/roosttest`
directory: that directory contains modules that deliberately end the UI
they drive, which under the iced default would strand every module the
session-scoped fixture runs afterward. `e2e-iced` runs a curated list
instead. **The enumerated-list trap:** a new roosttest module needs
adding to `ICED_E2E_TESTS` in the `Makefile` *and* to the corresponding
lists in `ci.yml`, or it never runs on the iced target.

## CI lanes

The single required check is `ci-success`, which derives its membership
from its own `needs:` list — ten jobs: the `changes` path-filter job
plus the nine required ones below. (The table's last row,
`e2e-iced-wayland-drag`, is a non-blocking signal lane and is
deliberately *not* one of the ten.) `changes` and `ci-success` always
run; every other job is conditionally gated on `changes`' outputs, so a
PR pays only for what it touches.

| Job | Runs | Required |
|---|---|---|
| `rust-lint` | `cargo fmt --check`, clippy at `-D warnings` | ✅ |
| `harness-unit` | `tools/roosttest_unit` — target/path/capability wiring | ✅ |
| `themes-parity` | The Rust + Mac bundled-theme copies are byte-identical | ✅ |
| `rust-build` | `cargo build`/`test` on Linux + macOS, plus the `roost-vt` ffi and `roost-engine/facade` passes | ✅ |
| `swift-mac` | `swift build` / `swift test`, release bundle, embedded `roostctl`, entitlements + link guards | ✅ |
| `iced-build-e2e` | 2×2 matrix (ubuntu, macOS) × (wgpu, tiny-skia): build + unit + toolkit-boundary check, then functional E2E — Linux under **both** X11 (xvfb) and **Wayland** (headless weston), plus the exit-on-empty, menu-quit, real-input clipboard, the five host-session client lanes (below), and (macOS) bundle-identity + smoke lanes | ✅ |
| `iced-release` | Release-profile build, the real `.deb`, a release-binary E2E subset, packaged-profile assertion, artifact smoke, dependency-closure check | ✅ |
| `e2e-mac` | The full `tools/roosttest` directory against a bundled `Roost.app` on a GUI-session runner | ✅ |
| `session-e2e` | The headless host-session lanes — `roost-session` driven with no UI and no display (`SESSION_E2E_TESTS`, marker-based rather than an enumerated CI list) | ✅ |
| `e2e-iced-wayland-drag` | `roost-iced` fullscreen under `cage` with a real `/dev/uinput` seat — the pointer-drag + system-clipboard guard the IPC-driven Wayland lane cannot cover | ❌ soft (`continue-on-error`, absent from `ci-success`) |

**The five host-session client lanes inside `iced-build-e2e`.** They
need a UI *and* a `roost-session` daemon, so they are in neither
`ICED_E2E_TESTS` nor `SESSION_E2E_TESTS` and run as their own steps —
`e2e-host-client`, `e2e-host-ssh`, `e2e-host-missing-daemon`,
`e2e-host-local-spawn` (added by plan 044 for #397) and
`e2e-host-bootstrap`, each with an X11 and a Wayland twin. They run
**serialized, never beside one another**: they share the client's
incarnation counter and the default session socket, so two at once bind
probes to each other's terminals and fail looking exactly like a product
bug. The Makefile targets and the CI steps both carry that warning.

Two notes on the soft lane: it pins `ROOST_REQUIRE_REAL_INPUT=1` so a
missing `cage`, `/dev/uinput`, or binary fails rather than silently
passing as a skip, and it tees diagnostics to an uploaded artifact —
`continue-on-error` hides a failure's *conclusion*, so the log body has
to be the honest record. Promotion to required is tracked separately.

Both required E2E lanes run `--roost-fresh` with a throwaway
`ROOST_STATE_DIR` and upload diagnostics on failure (the mac lane
additionally emits JUnit XML for GitHub's test annotations). Neither reruns failed tests: a genuine
intermittent bug must not be masked. The macOS lane additionally scales
its timeouts (`ROOST_TEST_TIMEOUT_SCALE`) and clears any stale
socket/lock before launching, since a crashed instance's held
single-instance flock is the one cascade mode that wedges the next run.

## Adding coverage

- **Behavior or content** — a Layer 1 case in `tools/roosttest/`. Use
  the fixture that yields a clean workspace; assert with `tab.dump` /
  `tab.list`. Remember the enumerated-list trap above.
- **Rendering** — a Layer 2 check via `tools/screenshot/` and
  `pngtool.py`. `tab.dump` is text-only, so anything about color or
  layout needs pixels.
- **Real key/pointer/clipboard behavior** — Layer 3, local, Linux for
  now. A Mac CGEvent sibling is
  [#285](https://github.com/charliek/roost/issues/285).

The guiding rule: an agent should be able to go from "I changed X" to
"here is the exact command that proves X still works" without guessing.
