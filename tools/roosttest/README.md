# roosttest — pytest E2E harness

Functional end-to-end tests that drive a **real** Roost UI (Mac, GTK, or Iced)
over the JSON IPC socket and assert on the op set — exactly what users
and `roostctl` drive (the [north star](../../docs/development/vision.md#the-command-core-north-star)).
Most tests read back via `tab.dump` / `tab.list` / `identify`; the
byte-level OSC pipeline tests additionally use the gated test-mode
IPC ops (`tab.feed_pty_bytes` / `tab.capture_pty_input` /
`tab.dump_resolved`, all `ROOST_TEST_MODE=1`-only) — see "OSC-routed
regression patterns" below.

## Run

```bash
make e2e            # default target ($ROOST_TARGET or gtk); reuses a running UI
make e2e-gtk        # against the GTK UI
make e2e-mac        # against the Mac app
make e2e-gtk-ci     # CI parity: ROOST_TEST_MODE=1 + --roost-fresh (owns a fresh UI)
make e2e-mac-ci     # CI parity (DESTRUCTIVE: force-quits any running Roost.app)
# or directly:
uv run --group test pytest tools/roosttest --roost-target mac -v
uv run --group test pytest tools/roosttest --roost-target iced -v
```

The session fixture launches the UI if it isn't already running (and
quits only what it launched), so a bare `make e2e` is self-contained.
Build first if needed: `make build` (GTK + roostctl) / `make bundle` (Mac).
`ROOST_GTK_BIN` and `ROOST_ICED_BIN` select explicit Rust UI executables;
`ROOST_ROOSTCTL` selects the CLI. The shed uses these to run shed-local ELF
artifacts while the mounted repository's `target/` contains macOS output.

Use the **`*-ci`** targets to reproduce CI locally: they unlock the
test-mode-gated ops (`ROOST_TEST_MODE=1`) and force a fresh harness-owned
instance (`--roost-fresh`), so you run the *same set CI does* rather than
silently skipping ~30 mode-gated tests. See "Hermetic / fresh mode" below.

## Layout

| File | What |
|---|---|
| `client.py` | `Roost` — a thin JSON-IPC client (direct Unix socket). Op methods (`open_tab`, `set_state`, `agent_report`, `dump`, …), the agent-axis readers (`shell_state`, `agent_lifecycle`, `ownership`, `hook_active`) + no-`sleep` waits (`wait_state`, `wait_lifecycle`, `wait_shell_state`, `wait_notification`, `wait_text`, `wait_gone`) + `run()` (wait for prompt, then send a command). |
| `ui.py` | Launch/quit a UI per target + socket-path resolution. `wait_alive` also confirms the UI's event subscription is live (see below). |
| `conftest.py` | Fixtures: `target` (`--roost-target`), `fresh` (`--roost-fresh`/`ROOST_TEST_FRESH`), a session fixture that owns/ensures the UI (hermetic in fresh mode), `roost` (a client), `project` (a throwaway, cascade-cleaned project). Also the `SKIPS: N` terminal summary. |
| `util.py` | Cross-file helpers: `precondition` / `skip_on_ci` (the skip policy), `cwd_reaches` (scaled cwd poll), `wait_tab_attached`, `wait_shell_ready` (pre-input race-fix for shells that emit pre-prompt content), drain helpers. |
| `test_smoke.py` | The smoke suite: content via `tab.dump`, state progression, notifications, focus, title-lock, cascade-close. |
| `test_palette.py` | The command palette as a driveable surface: open, introspect rows, filter, activate (which dispatches the same command its keybind would), push a sub-frame, dismiss. |
| `test_notifications.py` | The multi-project notification inbox: `view_notifications` frame, jump-to-notification (focuses the tab + clears its badge), clear-all. |
| `test_agent_lifecycle.py` | The agent state model end to end: synthetic Claude hook payloads driven through the **real `roostctl claude-hook` binary** (adapter → CLI → IPC → workspace → the derived `state` / `hook_active` / axis fields on `tab.list`). Covers the full turn, `Stop` with in-flight `background_tasks`, `StopFailure` (and that `tab.state` stays a closed four-value enum), unknown `notification_type`, `agent_id` events, foreign-session rejection, the legacy `prompt-submit` spelling, the deprecated `tab.set_hook_active` alias, and the OSC 133 `D`/`A` failsafe. |
| `test_launcher.py` | The custom-command launcher (Cmd/Alt+Shift+T): lists the seeded commands + activating one spawns a tab that runs it. |
| `test_newtab_cwd.py` | New-tab cwd inheritance: `palette.activate("new_tab")` (Cmd-T / Ctrl-T) and the launcher both spawn in the active tab's live (OSC 7) cwd, not the project cwd. Emits OSC 7 itself so it's shell-independent. |
| `test_terminal.py` | Program-driven terminal behavior: `test_cwd_tracking_follows_cd` (`cd` + an explicit OSC 7 emit → tracked cwd; cross-platform) and `test_title_follows_cwd` (title derives from cwd; skipped on Mac — shell-OSC-0-driven, see issue #196). |
| `test_test_ops.py` | Smoke triple for the test-only IPC ops (`tab.feed_pty_bytes`, `tab.capture_pty_input`, `tab.dump_resolved`) — the scaffolding for the byte-level OSC pipeline tests. Skipped without `ROOST_TEST_MODE=1`. |
| `test_osc_pipeline.py` | End-to-end OSC pipeline: bold + inverse resolver call-site coverage (#142), OSC 10/11/12 set/query reply round-trips (#145), and parity OSC 0/7/9 routing tests. Drives bytes via `tab.feed_pty_bytes`; reads back via `tab.dump_resolved` + `tab.capture_pty_input`. The canonical example for the "OSC-routed regression patterns" section below. |
| `test_device_queries.py` | End-to-end device-query replies (#247): each engine-autonomous query (DA1, DSR 5n/6n, DECRQM ?25, XTVERSION `libghostty`, Kitty keyboard `ESC[?u`) fed via `tab.feed_pty_bytes` produces its reply on the input side, read back via `tab.capture_pty_input`. Pins the `write_pty` drain wiring both UIs install. Skipped without `ROOST_TEST_MODE=1`. |
| `test_selection.py` | The `selection.*` op set: set/dump/clear round trips, and copy completeness (#249) — a selection scrolled into scrollback still copies in full, a multi-row selection copies every row in order, a reversed drag copies in document order, and wide/CJK glyphs copy without a phantom space. IPC-only (no pasteboard), so it runs in every lane including headless Wayland. |
| `test_osc52.py` | Program-initiated OSC 52 clipboard writes, plus the `clipboard.write`/`clipboard.dump` round trip they read through. Touches the host pasteboard, so it is the one module the headless-Wayland lanes skip. |
| `test_ime.py` | End-to-end IME (plan 021): `tab.feed_ime` (preedit/commit/clear) driven through the same route the iced adapter's winit IME handler takes — byte-exact commit encoding, preedit-only-at-cursor (never reaches the PTY), single-emit on preedit-then-commit, and the one-shot discard latch that drops a stray commit after a route change (e.g. opening the palette) cancels a live composition. Iced-only; skipped without `ROOST_TEST_MODE=1`. |
| `fixtures/launcher.conf` | Seed config the harness points the UI at via `ROOST_CONFIG` (see below), giving the launcher tests a deterministic command list. |

The shared `palette` fixture (open from closed, leave closed) lives in
`conftest.py`. The two UIs expose one command set (kept at parity), so
`test_palette.py`'s `COMMON_COMMAND_IDS` is the full palette command list
and is asserted present on whichever UI is under test.

## Seeding config (`ROOST_CONFIG`)

`ui.launch` sets `ROOST_CONFIG=fixtures/launcher.conf` on the UIs it
starts (GTK via env; Mac via `open --env`), so the launcher reads a
known command list. It applies only to harness-launched UIs — a
developer's already-running UI keeps its own config, so the launcher
tests `precondition` on the seed: a graceful skip against an ad-hoc dev
UI, but a hard failure in fresh mode (where the harness guarantees the
seed). (`ROOST_CONFIG` is a real override on both UIs, mirroring
`ROOST_SOCKET` / `ROOST_BUNDLE_PROFILE`.)

## Hermetic / fresh mode (`--roost-fresh`, `ROOST_STATE_DIR`)

A **harness-launched** UI always runs against a throwaway state dir, so a
run never reads or writes the developer's real `state.json`/tabs:

- `ROOST_STATE_DIR` (prod env on **both** UIs) redirects `state.json`'s
  directory **and the state lock beside it**; the socket, the socket/bind
  lock and the log stay on the default profile path, so `ui.py` still finds
  the UI by its unchanged socket. The harness `mkdtemp`s one per session
  and cleans it up. (Stricter than `ROOST_CONFIG`: must be absolute — see
  [paths.md](../../docs/reference/paths.md).)
- `ROOST_DEFAULTS_SUITE` (prod env, **Mac** only) redirects the app's
  `UserDefaults` (sidebar visibility/width) to a throwaway suite —
  `ROOST_STATE_DIR` can't reach `UserDefaults`.

`--roost-fresh` / `ROOST_TEST_FRESH=1` makes the harness **own** the
instance: it force-quits any running UI first (lock-safe on Mac via
`_quit_mac_process` + `_mac_cleanup`), launches a hermetic one, and always
quits it at teardown — vs. the default, which reuses a developer's running
UI and leaves it alone. Fresh mode is what `make e2e-*-ci` (and CI) use; it
also flips setup preconditions to hard failures (below). (It replaced the
old `ROOST_TEST_RESET_STATE`, which *deleted* the real `state.json` on Mac.)

### Teardown and the two instance locks

A UI holds two locks: the **socket/bind lock** (`<socket dir>/roost.lock`)
and the **state lock** (`<state dir>/state.lock`). Both live on inodes, not
names, so unlinking either out from under a live process frees the *name* —
the next launch takes a fresh inode and two UIs run against one socket or
one `state.json`. `end_session` therefore releases in the UI's own order,
with a proof at each step:

1. `_cleanup_owned_rust_runtime` — waits for the harness's own child to
   exit, takes `roost.lock` `LOCK_NB`, confirms nothing answers `identify`,
   and (dev, ino)-validates the lock before *and* after unlinking the
   socket.
2. `_remove_session_state` — takes `state.lock` `LOCK_NB` and
   (dev, ino)-validates it before deleting the session state dir. If it is
   held, teardown **raises** rather than deleting a live UI's lock.

Mac has no child handle for either proof, so `quit("mac")` escalates
`osascript quit` → SIGTERM → SIGKILL and confirms the process is gone —
strictly stronger than a flock probe, and it covers both locks at once.

A UI that exits *refusing* to start because another process holds the state
lock is surfaced by `wait_alive` as that refusal (a `RuntimeError` carrying
the UI's own message), not as a boot timeout.

The GTK launch env is sanitized (the UI inherits the parent env): the
per-tab vars Roost injects itself — `ROOST_SHELL_FEATURES`, etc. — and the
profile selector are stripped, so a value exported in the shell that ran
pytest can't leak into the UI and every tab.

## Skip policy (a skip = a genuine environment limit, never a silent gap)

A `skip` must mean only "this environment genuinely can't exercise this."
Helpers in `util.py`:

- `precondition(ok, reason)` — a *setup* precondition (seed config present,
  OSC 7 tracked) is a **hard failure in fresh mode** (the harness
  guarantees the environment → a failure is a real regression); a graceful
  skip otherwise. Use it instead of `pytest.skip` for "the setup didn't
  produce what I need."
- `skip_on_ci(reason, alt_coverage=…)` — for a test that genuinely can't
  run remotely (e.g. quit→relaunch under bare xvfb). **Must** cite where
  the regression class is otherwise covered.
- `cwd_reaches(...)` — the shared, `ROOST_TEST_TIMEOUT_SCALE`-scaled cwd poll.

Every run prints a **`SKIPS: N`** summary (each skipped test + reason) via
`conftest.py::pytest_terminal_summary`, so a half-skipped run can't read as
"all green." The zsh + modern-bash CI-provisioning gap (issue #197) is
closed: the GTK and Mac CI runners install zsh and modern bash; the
auto-bootstrap tests now use `precondition(...)` rather than `pytest.skip`,
so a missed CI install hard-fails in fresh mode rather than silently
skipping. The `wait_shell_ready` helper in `util.py` is the canonical
pre-input pattern for any test that spawns a non-bare shell — it works
around shells that emit pre-prompt output (compinit, MOTD, `--posix`
recreation) which would otherwise race the harness's "viewport non-empty"
readiness check.

## Determinism notes (why it isn't flaky)

- **No sleeps.** Tests wait on conditions via the op set — `wait_state`,
  `wait_text` (polls `tab.dump`), `wait_gone`.
- **Content via text, not pixels.** `tab.dump` returns the viewport as
  text; assert exact strings. `run()` waits for the shell prompt before
  sending, and tests assert on a marker that appears only in command
  *output*, never the echoed command.
- **Startup readiness.** `ui.wait_alive` waits past two boot stages: the
  IPC socket answers `identify` before the workspace exists (wait for a
  tab), and the event subscription comes up at the end of bootstrap. It
  round-trips a **probe tab** — open via IPC, require it to materialize a
  live terminal (`dump` succeeds), then close it — so tests only start
  once the UI is fully up. A tab opened via IPC *before* the
  subscription is live no longer races permanently: both UIs reconcile
  against a snapshot as the subscription's first action
  (resync-on-subscribe), so the probe is a readiness gate, not a
  workaround for a dropped event.
- **Isolation.** Each test gets its own `project` fixture and
  cascade-cleans it; a harness-launched UI also runs against a throwaway
  `ROOST_STATE_DIR` (+ `ROOST_DEFAULTS_SUITE` on Mac), so a run never
  touches the dev's real workspace — see "Hermetic / fresh mode" above.

## Writing a test

```python
def test_echo(roost, project):
    tab = roost.open_tab(project, cwd="/tmp")
    roost.run(tab, "printf 'X=%s\\n' 42")   # waits for prompt, sends
    roost.wait_text(tab, "X=42")            # waits for the output
    assert "X=42" in roost.dump_text(tab)
```

### OSC-routed regression patterns *(test-mode IPC ops)*

When the behavior under test is a **byte-level wiring** detail — does
the production code path actually drive the resolver correctly?, does
an OSC reply reach `send_input`? — go through the gated
`tab.feed_pty_bytes` + `tab.capture_pty_input` ops instead of trying
to drive the shell into emitting the sequence. They require
`ROOST_TEST_MODE=1` at UI launch (CI sets it; the harness's
`tools/roosttest/test_test_ops.py` skips otherwise):

```python
def test_osc11_set_then_query_replies_with_new_bg(roost, project):
    tab = roost.open_tab(project, cwd="/tmp")
    # SET in one chunk, QUERY in the next: a SET affects a LATER chunk's
    # query, while SET+QUERY in ONE chunk answers the pre-chunk color.
    roost.tab_feed_pty_bytes(tab, b"\x1b]11;rgb:00/11/22\x07")
    roost.tab_feed_pty_bytes(tab, b"\x1b]11;?\x07")
    reply = roost.tab_capture_pty_input(tab, drain=True)
    assert b"0000/1111/2222" in reply
```

**What `tab.feed_pty_bytes` exercises on iced.** Since plan 026's D10,
iced's OSC scan lives in the PTY drain (`TabSession`'s forwarding task
owns the tab's only `OscRouter` and answers color queries there, ahead
of the UI's event loop — that latency is what leaked replies into the
shell prompt). `tab.feed_pty_bytes` injects on the UI thread, so it
cannot go through the drain; it routes through
`TerminalTab::scan_and_write_vt`, which hands the bytes to the SAME
router and the SAME color state via `TabSession::scan_osc` before
writing them to the terminal. So these tests still walk the production
scan, the production color state and the production input channel — but
they cannot prove the reply left *without* the UI. That property has its
own test at the engine level:
`crates/roost-engine/tests/osc_drain_reply_test.rs`. GTK and Mac keep
their own UI-side routers, and `feed_pty_bytes` is their production path
by construction.

**Corollary — feed a quiet tab.** Because the injector bypasses the
drain, injected bytes and live PTY bytes are two producers into one
streaming scanner: a chunk fed while the shell is still writing can be
scanned out of terminal order, or land mid-sequence and corrupt the
parse for both. The `wait_tab_quiet` rule above (already required so a
prompt doesn't overwrite seeded rows) covers this too — it is the reason
it applies to OSC-only feeds on iced as well, not just to tests that
seed viewport content. Production is unaffected: `tab.feed_pty_bytes` is
`ROOST_TEST_MODE=1`-gated and the forwarding task is the only scanner
otherwise.

**Seed only after the tab goes quiet.** `tab.feed_pty_bytes` applies its
bytes as soon as the UI services the op and does *not* serialize with PTY
output still in flight, so a seed sent at `wait_tab_attached` time can land
*ahead* of the shell's prompt — which then overwrites (or appends to) the
seeded row. Any test that seeds viewport content, or state the shell also
writes (OSC 7 cwd, OSC 0 title), waits on `util.wait_tab_quiet(roost, tab)`
first: non-empty `tab.dump` text that is byte-identical across consecutive
polls. Condition wait, never a sleep. Tests that only feed drain-observable
queries (device-attribute replies, mode enables) don't need it.

For resolver-output asserts (theme bold-color, SGR inverse swap,
etc.), `roost.tab_dump_resolved(tab)` walks the viewport through the
production color resolver and returns per-cell `{fg, bg, bold,
inverse, ...}` — see the smoke test in `test_test_ops.py`. This op
is ungated.

## Out of scope here (use the other harnesses)

Some behavior isn't deterministically drivable through the IPC op set —
it's pixel- or input- or shell-level. It lives elsewhere, by design:

| Behavior | Why not here | Where |
|---|---|---|
| Real mouse selection, real clipboard paste | a physical drag + the OS pasteboard, not IPC (what copy *contains* is covered here by `test_selection.py` via `selection.set`/`selection.dump`) | `tools/input/linux` (uinput inject + clipread) |
| Live resize / reflow | the UI sizes the grid to the window, so `tab.resize` doesn't pin a size | `tools/screenshot` (resize window, check reflow) |
| Theme color rendering | `tab.dump` is text-only (no color) | `tools/screenshot` screenshots |
| OSC 2 window-title | cwd-derived title + the shell re-emits each prompt overwrites it | `tools/screenshot` (visible title) |
| OSC parsing itself | — | `roost-osc` unit tests (osc2/osc7/osc777) |
| Sidebar open/close | no IPC-observable state | `tools/screenshot`, or add an `identify` field |
| Real shell-driven side effects (`cd` updating cwd, etc.) | the test-mode `tab.feed_pty_bytes` op *simulates* PTY output, it doesn't run a real shell | `tools/input/linux/` (real key+pointer injection) when the bug is in the shell↔UI handshake |

See [`docs/development/test-automation.md`](../../docs/development/test-automation.md)
for the plan (CI tiers, `roostctl wait`, the relationship to
`tools/screenshot/` and `tools/input/linux/`).
