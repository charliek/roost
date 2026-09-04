# roosttest — pytest E2E harness

Functional end-to-end tests that drive a **real** Roost UI (Mac or Iced)
over the JSON IPC socket and assert on the op set (one headless
exception: `test_session.py` drives the UI-less `roost-session` daemon) — exactly what users
and `roostctl` drive (the [north star](../../docs/development/vision.md#the-command-core-north-star)).
Most tests read back via `tab.dump` / `tab.list` / `identify`; the
byte-level OSC pipeline tests additionally use the gated test-mode
IPC ops (`tab.feed_pty_bytes` / `tab.capture_pty_input` /
`tab.dump_resolved`, all `ROOST_TEST_MODE=1`-only) — see "OSC-routed
regression patterns" below.

## Run

```bash
make e2e            # dispatch on $ROOST_TARGET (default iced): the curated e2e-iced lane, or e2e-mac
make e2e-iced       # the curated iced lane (see ICED_E2E_TESTS in the Makefile)
make e2e-mac        # against the Mac app (full tools/roosttest directory)
make e2e-iced-ci    # CI parity: ROOST_TEST_MODE=1 + --roost-fresh (owns a fresh UI)
make e2e-mac-ci     # CI parity (DESTRUCTIVE: force-quits any running Roost.app)
make e2e-session    # the headless host-session lane (no UI, no display)
make e2e-host-client # HS-2: a roost-session daemon beside the Iced UI (needs both)
make e2e-host-ssh   # HS-3: the same pair with a real SshTunnel between them (fake ssh)
# or directly:
uv run --group test pytest tools/roosttest --roost-target mac -v
uv run --group test pytest tools/roosttest --roost-target iced -v
```

The session fixture launches the UI if it isn't already running (and
quits only what it launched), so a bare `make e2e` is self-contained.
Build first if needed: `make build` (iced + roostctl) / `make bundle` (Mac).
`ROOST_ICED_BIN` selects an explicit iced UI executable;
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
| `session.py` | The **host-session** launcher: build a throwaway `roost-session` profile (per-OS env isolation), spawn the daemon in either start shape, parse its readiness verdict, and tear every process down before the root goes. Deliberately parallel to `ui.py` rather than part of it — a session is not a UI. |
| `eventstream.py` | `EventStream` — a second connection that sends `events.subscribe`, keeps the ack's fence revision, then reads pushed `EventBatch` frames. Separate from `client.py` because the push stream is one-way and would sit in front of every ordinary `call`. |
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
| `test_dock_badge.py` | The macOS Dock-tile badge (plan 027 C7, the M6 § 6b native seam's first consumer): a pending notification badges the tile with the inbox count and clearing the inbox removes it, read back off AppKit via the `app.dock_badge` test-mode op rather than recomputed from the inbox. Skips unless the host is macOS **and** the target is iced — `make e2e-mac` collects the whole directory, so an OS-only skip would aim an iced-only op at the Swift app. Skipped without `ROOST_TEST_MODE=1`. |
| `test_exit_on_empty.py` | Iced's exit-on-empty policy (plan 026 D8, mac parity): deleting the LAST project over IPC replies successfully, the process then exits on its own with status 0, and the throwaway `state.json` records the emptied workspace (proving `App`'s drop-time flush ran). **Runs in its own pytest invocation** — `make e2e-iced-exit`, and its own ci.yml step per lane — because it ends the UI it drives; a mid-suite exit would strand every module after it in the session-scoped harness. Deliberately NOT in `ICED_E2E_TESTS`, and self-enforcing: it skips (loudly) whenever it is collected beside another module, so a whole-directory run can't be poisoned by it. Also skipped off iced and without `--roost-fresh`. |
| `test_session.py` | The headless host session (`roost-session`, plan 035): both start shapes, the readiness verdicts (`ready` / `already-running` / `error`), the D2b two-first-starts race, SIGTERM-converges-with-stop, stale-socket recovery after SIGKILL, the umask posture (socket 0600, dirs 0700, `state.json` + log 0600), layout restore across a restart, headless OSC + natural-exit handling, `tab.open` racing `session.stop`, the Python-side event-push reader, and `roostctl session start/stop/status` end to end. **Runs no UI at all** — `make e2e-session` and its own `session-e2e` CI job; every test carries the `session_daemon` marker, which is what tells `conftest`'s autouse UI fixture to stand down, and every whole-directory run (`e2e-mac`, `e2e-mac-ci`, the `e2e-mac` CI job, release.yml's bundle E2E) deselects it — see `DAEMON_E2E_DESELECT` in the Makefile — so the Mac gate never has to build `roost-session`. |
| `test_host_client.py` | HS-2's **client** side (plan 037): a real `roost-session` daemon beside the harness UI, wired together with `host.add` + `host.connect` and observed entirely over IPC. Covers the roadmap acceptance (a marker survives disconnect + reconnect), attach fidelity (the session's `tab.dump_resolved` vs the UI's for the `h<host>.<id>` spelling), never-blank/no-duplication on refocus, disconnect-vs-stop, the build-mismatch `needs-restart` state and the restart composition, takeover (a scripted wire client displaces the UI; the frozen frame survives and `host.connect` takes it back), lease-holder-only effects, the local/host id collision, the attach latency budget, and the attention-surface regressions. **Needs a UI *and* a daemon**, so it is in neither `ICED_E2E_TESTS` nor `SESSION_E2E_TESTS`: `make e2e-host-client` / `make e2e-host-client-ci`, its own Linux X11 + Wayland steps in `iced-build-e2e`, and the `host_client` marker every whole-directory run and every macOS iced cell deselects. |
| `test_ssh_transport.py` | HS-3's **far side** (plan 038): a real `roost-session client-bridge` — the process `ssh` execs on the host — in front of a real session, with the test itself playing the tunnel's local half (`BridgeListener`: accept, spawn one bridge per connection, pump both directions, propagate each half-close). Covers control-plane transparency (`session.identify` equal to the same op dialed directly, plus a subscribed event stream), a full attach with byte-fidelity echo through both pumps, a SIGKILLed bridge leaving nothing wedged, both half-close directions ending with an exit-0 child, and the no-session failure the client classifies on (`client-bridge: no session`, exit 1, empty stdout). **Runs no UI** — `session_daemon`, so it rides `make e2e-session` and the `session-e2e` job. The one link it does not contain is the `ssh` hop itself, which `crates/roost-ipc/tests/ssh_transport_test.rs` and `test_host_ssh.py` cover. |
| `test_host_ssh.py` | HS-3's **UI side** (plan 038): the app's own `SshTunnel` reaching a real session, with only the `ssh` binary faked (`fixtures/fake-ssh.sh`, pointed at by `$ROOST_SSH_BIN`, which the module sets at *import* so the launched UI inherits it). Covers a connect that hydrates and renders (asserted on bytes, and on the invocation log proving the mux warm-up + per-connection execs really ran), a SIGKILLed transport landing the host in `disconnected` with a reconnect that finds the same terminal, and the three classified failures — auth, changed host key (which must never offer to accept it), and a missing remote `roost-session`. **Needs a UI *and* a daemon**, like `test_host_client.py`, and must never run beside it: `make e2e-host-ssh` / `make e2e-host-ssh-ci`, plus its own Linux X11 + Wayland steps ordered after the host-client ones. |
| `test_host_bootstrap.py` | HS-3 slice 2's **install/upgrade** flow (plan 039), through the real UI: the same fake `ssh`, but in `run-remote` mode, where the generated remote scripts genuinely execute — so the probe ladder, the four-exec staged install and the atomic commit really happen rather than being stubbed. Covers each row of the action matrix (missing / mismatch / compatible, with and without a session already running — including the running-mismatch upgrade's stop → await-gone → start order), an install outside the remote's `PATH`, cancel mutating nothing, a checksum failure leaving the jail untouched, the socket-target case that must offer no update button, and `roostctl` never prompting. A `BootstrapJail` (`$HOME`, `$PATH`, `ROOST_BOOTSTRAP_FS_ROOT`) makes the developer's own machine invisible to those scripts, and the binary the job starts inside it is a real `roost-session`. Every verdict is read through `host.status` — a bootstrap note or refusal in `reason`, a successful install proved by `generation` advancing, since only the Ok arm reconnects. `host_client`-marked like the other two: `make e2e-host-bootstrap` / `-ci`, its own Linux X11 + Wayland steps. |
| `test_host_local_missing_daemon.py` | Plan 042 W2: a **localhost** session whose `roost-session` cannot be started settles once and says why. `$ROOST_SESSION_BIN` points at a path that does not exist — set at *import*, so the harness's UI inherits it, the same reason `test_host_ssh.py` sets `$ROOST_SSH_BIN` there — and the whole assertion is one `host.status` row: `disconnected`, the rollup `disconnected — cannot find roost-session`, `retry` **absent**, `generation` 0 before the connect and 1 after, and a `detail` naming the override rung. The row is then held flat for 3s (≥ 8 rungs of the ladder this replaced) and the palette must still offer Connect. `host_client`-marked; `make e2e-host-missing-daemon` / `-ci`, ordered after the host-ssh steps and never beside another host lane. |
| `test_host_local_spawn.py` | Plan 044 W2 (#397): the positive twin of the module above — a **localhost** session the UI really does spawn, from a UI running under the harness's throwaway `ROOST_STATE_DIR`. Before #397 that was impossible: the daemon inherited the seam, resolved the *UI's* state dir, and refused the `state.lock` the UI holds. The lane connects, waits for `connected` with no `detail` and no `retry`, asserts `<UI state dir>/session/state.lock` (held from daemon startup, so it has no race) and then polls for `state.json` (written on the first commit, so it trails the connect), and finishes by proving the derived state dir is still *discoverable* — `roostctl session status` exits 0, because the seam moves state and never the socket. Its preflight goes through `precondition`, not `pytest.skip`: a session already listening on the default socket is a hard failure in fresh mode (a leaked daemon that greened this lane would hide the regression forever) and a skip on a developer's box, where it is their session and this lane must never stop it; a run that reuses a developer's already-running UI skips by name, since the state dir of a UI the harness did not launch is not knowable from outside — a non-fresh run with nothing running still launches its own UI and asserts for real. Teardown stops only what it started. `host_client`-marked; `make e2e-host-local-spawn` / `-ci`, ordered after the missing-daemon steps and never beside another host lane — it owns the default session socket for its duration. |
| `fixtures/launcher.conf` | Seed config the harness points the UI at via `ROOST_CONFIG` (see below), giving the launcher tests a deterministic command list. |
| `fixtures/fake-ssh.sh` | The stand-in `ssh` the HS-3 UI lanes (`test_host_ssh.py`, `test_host_bootstrap.py`) and `crates/roost-ipc/tests/ssh_transport_test.rs` drive: an argv log, the mux/`-O exit` choreography, one failure mode per line of the classifier's table, and the `run-remote` mode that really executes the argv it is handed. Its header is the contract. |

The shared `palette` fixture (open from closed, leave closed) lives in
`conftest.py`. The two UIs expose one command set (kept at parity), so
`test_palette.py`'s `COMMON_COMMAND_IDS` is the full palette command list
and is asserted present on whichever UI is under test.

## Seeding config (`ROOST_CONFIG`)

`ui.launch` sets `ROOST_CONFIG=fixtures/launcher.conf` on the UIs it
starts (iced via env; Mac via `open --env`), so the launcher reads a
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

The iced launch env is sanitized (the UI inherits the parent env): the
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
closed: the iced and Mac CI runners install zsh and modern bash; the
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

### Host connection state: `host.status`, never the log

Assert a host's connection state through the `host.status` op
(`client.host_status(id=None)`), never by scraping the UI's `tracing`
output. A log line is an operator convenience that a refactor is free to
reword or drop; a test reading one passes just as happily against a
feature that has stopped working, which is exactly what
`test_host_ssh.py` did before plan 042. The op returns the same state
the sidebar's band is drawn from: `state`, `reason` (untruncated),
`rollup` (the band's own string), `generation`, and `retry`.

Two edges are worth knowing before writing a wait against it:

- **`generation` counts attempts *started*, not connections.** An ssh
  establish that never succeeds still advances it, and it survives a
  disconnect. Read it *before* the op that starts an attempt and wait
  for it to advance — two consecutive attempts can fail with
  byte-identical reasons, so "disconnected with a reason" cannot tell
  one from the next. It is also the flatness check for "nothing further
  happened": a timer that fired and re-armed between two polls can leave
  `retry` looking absent at both, but cannot leave `generation` still.
- **`retry.attempt` is monotonic within one outage, and a poll may skip
  a rung.** Wait for `attempt >= n` and assert non-decrease across polls
  while the same ladder is active; never demand to see a particular
  rung, which a short `ROOST_SSH_RECONNECT_BASE_MS` makes a race. The
  counter legitimately resets to `1` when suspend detection restarts
  the ladder (`host_conn.rs`'s `restart_ladder`) — a suspended test box
  is outside a lane's contract, so no lane tolerates that reset.

`retry` is present only while a rung is armed, and it carries
`attempt`/`budget` for the ssh ladder alone — a localhost backoff
reports `delay_ms` and nothing else.

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
    wait_tab_quiet(roost, tab)  # a late shell byte would join the capture
    # SET then QUERY. libghostty answers the query from
    # `override orelse default`, in wire order — so a SET is visible to
    # any QUERY behind it, in a later chunk OR the same one.
    roost.tab_feed_pty_bytes(tab, b"\x1b]11;rgb:00/11/22\x07")
    roost.tab_feed_pty_bytes(tab, b"\x1b]11;?\x07")
    reply = roost.tab_capture_pty_input(tab, drain=True)
    assert b"0000/1111/2222" in reply
    assert reply.count(b"\x1b]11;rgb:") == 1   # exactly one answer
```

**Count the replies.** Color-query replies come from libghostty's
`write_pty` effect (`docs/reference/terminal-queries.md`); Roost used to
synthesize its own alongside them, which double-answered every query.
Any new color-query case asserts the reply COUNT, not just its presence
— and drains for a beat past the first match, since a duplicate arrives
after it (`_drain_settled` in `test_osc_pipeline.py`).

**What `tab.feed_pty_bytes` exercises on iced.** Since plan 026's D10,
iced's OSC scan lives in the PTY drain (`TabSession`'s forwarding task
owns the tab's only `OscRouter`). `tab.feed_pty_bytes` injects on the UI
thread, so it cannot go through the drain; it routes through
`TerminalTab::scan_and_write_vt`, which hands the bytes to the SAME
router and the SAME color state via `TabSession::scan_osc` before
writing them to the terminal. So these tests still walk the production
scan, the production color state and the production input channel. The
complementary property — that the drain itself enqueues NOTHING for a
color query — has its own test at the engine level:
`crates/roost-engine/tests/osc_drain_reply_test.rs`. Mac keeps its own
UI-side router, and `feed_pty_bytes` is its production path by
construction.

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
| Real mouse selection, real clipboard paste | a physical drag + the OS pasteboard, not IPC (what copy *contains* is covered here by `test_selection.py` via `selection.set`/`selection.dump`) | `tools/input/linux` (uinput injectors + `iced_clipboard_check.py`) |
| Live resize / reflow | the UI sizes the grid to the window, so `tab.resize` doesn't pin a size | `tools/screenshot` (resize window, check reflow) |
| Theme color rendering | `tab.dump` is text-only (no color) | `tools/screenshot` screenshots |
| OSC 2 window-title | cwd-derived title + the shell re-emits each prompt overwrites it | `tools/screenshot` (visible title) |
| OSC parsing itself | — | `roost-osc` unit tests (osc2/osc7/osc777) |
| Sidebar open/close | no IPC-observable state | `tools/screenshot`, or add an `identify` field |
| Real shell-driven side effects (`cd` updating cwd, etc.) | the test-mode `tab.feed_pty_bytes` op *simulates* PTY output, it doesn't run a real shell | `tools/input/linux/` (real key+pointer injection) when the bug is in the shell↔UI handshake |

See [`docs/development/test-automation.md`](../../docs/development/test-automation.md)
for the plan (CI tiers, `roostctl wait`, the relationship to
`tools/screenshot/` and `tools/input/linux/`).
