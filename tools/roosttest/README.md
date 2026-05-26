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
| `test_palette.py` | The command palette as a driveable surface: open, introspect rows, filter, activate (which dispatches the same command its keybind would), push a sub-frame, dismiss. A local `palette` fixture drives from a known-closed state and leaves it closed. |

## Known cross-UI parity gap (palette command set)

The two UIs' command palettes don't expose an identical command set, so
`test_palette.py` asserts only on the ids **common to both** (see
`COMMON_COMMAND_IDS`):

- Mac lists `close_project` + a Mac-only `jump_to_unread`.
- GTK lists `delete_project` (different id *and* semantics) and has no
  jump-to-unread.

This is a product decision (are "close" and "delete" the same action?
should GTK gain jump-to-unread?), not a wiring bug — left for a
follow-up rather than silently unified. If it's reconciled, fold those
ids into `COMMON_COMMAND_IDS`.

## Determinism notes (why it isn't flaky)

- **No sleeps.** Tests wait on conditions via the op set — `wait_state`,
  `wait_text` (polls `tab.dump`), `wait_gone`.
- **Content via text, not pixels.** `tab.dump` returns the viewport as
  text; assert exact strings. `run()` waits for the shell prompt before
  sending, and tests assert on a marker that appears only in command
  *output*, never the echoed command.
- **Startup readiness.** `ui.wait_alive` clears two boot races: the IPC
  socket answers `identify` before the workspace exists (wait for a
  tab), and the UI's event subscription starts at the end of bootstrap
  (a tab opened before then is missed). It round-trips a **probe tab**
  — open via IPC, require it to materialize a live terminal (`dump`
  succeeds), then close it — so tests only start once an IPC-opened tab
  reliably becomes live.
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

See [`docs/development/test-automation.md`](../../docs/development/test-automation.md)
for the plan (CI tiers, `roostctl wait`, the relationship to
`tools/uitest/` and `tools/linux/`).
