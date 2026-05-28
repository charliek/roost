# roosttest — pytest E2E harness

Functional end-to-end tests that drive a **real** Roost UI (Mac or GTK)
over the JSON IPC socket and assert on the op set — exactly what users
and `roostctl` drive (the [north star](../../docs/development/vision.md#the-command-core-north-star)).
No test-only backdoors; assertions read back via `tab.dump` / `tab.list`
/ `identify`.

## Run

```bash
make e2e            # default target ($ROOST_TARGET or gtk)
make e2e-gtk        # against the GTK UI
make e2e-mac        # against the Mac app
# or directly:
uv run --group test pytest tools/roosttest --roost-target mac -v
```

The session fixture launches the UI if it isn't already running (and
quits only what it launched), so a bare `make e2e` is self-contained.
Build first if needed: `make build` (GTK + roostctl) / `make bundle` (Mac).

## Layout

| File | What |
|---|---|
| `client.py` | `Roost` — a thin JSON-IPC client (direct Unix socket). Op methods (`open_tab`, `set_state`, `dump`, …) + no-`sleep` waits (`wait_state`, `wait_text`, `wait_gone`) + `run()` (wait for prompt, then send a command). |
| `ui.py` | Launch/quit a UI per target + socket-path resolution. `wait_alive` also confirms the UI's event subscription is live (see below). |
| `conftest.py` | Fixtures: `target` (`--roost-target`), a session fixture that ensures the UI is up, `roost` (a client), `project` (a throwaway, cascade-cleaned project). |
| `test_smoke.py` | The smoke suite: content via `tab.dump`, state progression, notifications, focus, title-lock, cascade-close. |
| `test_palette.py` | The command palette as a driveable surface: open, introspect rows, filter, activate (which dispatches the same command its keybind would), push a sub-frame, dismiss. |
| `test_notifications.py` | The multi-project notification inbox: `view_notifications` frame, jump-to-notification (focuses the tab + clears its badge), clear-all. |
| `test_launcher.py` | The custom-command launcher (Cmd/Alt+Shift+T): lists the seeded commands + activating one spawns a tab that runs it. |
| `test_newtab_cwd.py` | New-tab cwd inheritance: `palette.activate("new_tab")` (Cmd-T / Ctrl-T) and the launcher both spawn in the active tab's live (OSC 7) cwd, not the project cwd. Emits OSC 7 itself so it's shell-independent. |
| `test_terminal.py` | Program-driven terminal behavior: OSC 7 cwd tracking via a real `cd` (skip-guarded for shells without OSC 7). |
| `test_test_ops.py` | Smoke triple for the test-only IPC ops (`tab.feed_pty_bytes`, `tab.capture_pty_input`, `tab.dump_resolved`) — the scaffolding for the byte-level OSC pipeline tests. Skipped without `ROOST_TEST_MODE=1`. |
| `test_osc_pipeline.py` | End-to-end OSC pipeline: bold + inverse resolver call-site coverage (#142), OSC 10/11/12 set/query reply round-trips (#145), and parity OSC 0/7/9 routing tests. Drives bytes via `tab.feed_pty_bytes`; reads back via `tab.dump_resolved` + `tab.capture_pty_input`. The canonical example for the "OSC-routed regression patterns" section below. |
| `fixtures/launcher.conf` | Seed config the harness points the UI at via `ROOST_CONFIG` (see below), giving the launcher tests a deterministic command list. |

The shared `palette` fixture (open from closed, leave closed) lives in
`conftest.py`. The two UIs expose one command set (kept at parity), so
`test_palette.py`'s `COMMON_COMMAND_IDS` is the full palette command list
and is asserted present on whichever UI is under test.

## Seeding config (`ROOST_CONFIG`)

`ui.launch` sets `ROOST_CONFIG=fixtures/launcher.conf` on the UIs it
starts (GTK via env; Mac via `open --env`), so the launcher reads a
known command list. It applies only to harness-launched UIs — a
developer's already-running UI keeps its own config, and the launcher
tests `skip` when the seed isn't active. (`ROOST_CONFIG` is a real
override on both UIs, mirroring `ROOST_SOCKET` / `ROOST_BUNDLE_PROFILE`.)

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
  cascade-cleans it.

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
    # SET in one chunk so libghostty processes it before the next
    # scanner.feed sees the QUERY.
    roost.tab_feed_pty_bytes(tab, b"\x1b]11;rgb:00/11/22\x07")
    roost.tab_feed_pty_bytes(tab, b"\x1b]11;?\x07")
    reply = roost.tab_capture_pty_input(tab, drain=True)
    assert b"0000/1111/2222" in reply
```

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
| Selection + copy, real clipboard paste | mouse selection + OS clipboard, not IPC | `tools/input/linux` (uinput inject + clipread) |
| Live resize / reflow | the UI sizes the grid to the window, so `tab.resize` doesn't pin a size | `tools/screenshot` (resize window, check reflow) |
| Theme color rendering | `tab.dump` is text-only (no color) | `tools/screenshot` screenshots |
| OSC 2 window-title | cwd-derived title + the shell re-emits each prompt overwrites it | `tools/screenshot` (visible title) |
| OSC parsing itself | — | `roost-osc` unit tests (osc2/osc7/osc777) |
| Sidebar open/close | no IPC-observable state | `tools/screenshot`, or add an `identify` field |
| Real shell-driven side effects (`cd` updating cwd, etc.) | the test-mode `tab.feed_pty_bytes` op *simulates* PTY output, it doesn't run a real shell | `tools/input/linux/` (real key+pointer injection) when the bug is in the shell↔UI handshake |

See [`docs/development/test-automation.md`](../../docs/development/test-automation.md)
for the plan (CI tiers, `roostctl wait`, the relationship to
`tools/screenshot/` and `tools/input/linux/`).
