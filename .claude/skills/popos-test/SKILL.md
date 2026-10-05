---
name: popos-test
description: Run roost's Linux iced tests locally on a native Pop!_OS COSMIC dev box (no VM). The native-Linux counterpart to the `linux-test` skill (which is for Macs via a shed VM). Use when on Linux and asked to run/verify the e2e-iced suite, the Wayland functional tier, or check which test tiers run locally vs. need CI/shed. Covers the apt deps, `make e2e-iced`/`e2e-iced-ci`, the weston headless tier, the seat0 caveat (the live COSMIC session owns input, so the cage+uinput real-input tier can't run locally), workspace isolation, and where logs live.
---

# Linux testing on a native Pop!_OS COSMIC box

For a Pop!_OS COSMIC machine you are logged into (no host names belong in this
file). Mac work: `mac-test`. Linux from a Mac: `linux-test`.

You're already on Linux, so — unlike the Mac `linux-test` path — you do **not**
need a shed VM. Build + run the **iced** UI (`crates/roost-iced/`, what the
`.deb` ships as `/usr/bin/roost`) directly. Wayland is the primary lane:
winit's Wayland backend is only exercised on a real compositor, never under
Xvfb, so Wayland-specific bugs slip through an X11-only run. The catch is the
**real-input** tier: your live COSMIC session owns `seat0`, so a second
compositor can't grab input devices. Use this matrix:

| Tier | Runs locally? | How |
|---|---|---|
| **X11 / Xvfb** (`e2e-iced` functional suite, secondary) | ✅ | `xvfb-run` + pytest |
| **weston / Wayland** (`e2e-iced` functional suite, primary) | ✅ | weston headless (`tools/wayland/weston-run.sh`) |
| **headless `cage`** (rendering only) | ✅ | `WLR_BACKENDS=headless cage -- …` |
| **`cage` + `/dev/uinput`** (real-pointer drag/clipboard guard, `iced_wayland_clipboard_check.py`) | ❌ | CI `e2e-iced-wayland-drag` or a shed VM |

The last row fails on the live desktop with `libseat: Could not take control of
session: Device or resource busy` — COSMIC holds the seat/VT. That tier needs an
*isolated* seat (CI's headless runner + seatd, or the shed VM's fresh kernel).
Don't fight it locally — use the `linux-test` skill (shed) or CI instead.

## Prerequisites (one-time)

```bash
sudo apt-get install -y \
  libclang-dev pkg-config clang \
  weston cage xvfb xdotool python3-pytest wl-clipboard zsh
```

- **GTK4 dev packages** (`libgtk-4-dev libadwaita-1-dev`) are only needed if
  you'll run `tools/input/linux/iced_native_file_drop_check.py` — it launches
  a small throwaway GTK app as its XDND drag *source* to exercise
  `roost-iced`'s native file-drop target. `roost-iced` itself is GTK-free
  (verified independently by `make check-iced`'s dependency-boundary gate);
  skip these packages if you're not running that one check.
- **Do NOT install `seatd`** on Pop!_OS COSMIC: it collides with `pop-desktop` /
  `pop-de-cosmic` (apt reports a `pkgProblemResolver` break). It isn't needed —
  `libseat1` is already present and `logind` provides `seat0`. (`seatd` is only
  for the headless real-input tier, which you run on CI/shed anyway.)
- Toolchain: `mise install` (rust/zig pinned), then `third_party/ghostty/build.sh` once.

## Run the e2e-iced suite locally

```bash
make e2e-iced       # the curated ICED_E2E_TESTS lane against a dev build
make e2e-iced-ci    # same lane, fresh harness UI (CI parity — sets ROOST_TEST_MODE)
```

Neither bare command isolates your user's XDG dirs: the dev `roost-iced` profile
still reads your real state and runtime dir. Use the isolated form below when
that matters.

On a live COSMIC session `$WAYLAND_DISPLAY` is already set, so these targets
will run against it directly with no weston needed, but **prefer
`tools/wayland/weston-run.sh make e2e-iced-ci`** instead. The pixel and
render-stat tests (screenshot- and `render_stats`-based: sidebar dot colors,
tab-strip painting, sprite cells, IME preedit, the renderer-geometry
walking-skeleton case) self-skip whenever `WAYLAND_DISPLAY` names a live desktop
compositor rather than the harness's own weston socket: a live session's own
window decorations and output scaling make captured pixels unreliable
(issue #488). Running directly against the live session is therefore not full
coverage. `weston-run.sh` mints its own `wayland-roost-$$` socket, which the
self-skip recognizes, so they run there. To see exactly which tests skipped in a
run, use `pytest -rs`; do not trust a fixed count, since the set changes as
tests are added. `e2e-iced` also skips the clipboard tests
(`ICED_CLIPBOARD_TESTS`: `test_osc52`, `test_paste_files`,
`test_context_menu_clipboard`) when `WAYLAND_DISPLAY` is set, since Wayland
clipboard needs a focused seat/serial only a real interactive session provides;
use `xvfb-run` (below) to exercise those specifically.

More lanes and rules (shared with `linux-test`, which has the full text):

- Own-invocation lanes: `make e2e-iced-exit`, `e2e-iced-menu-quit` and
  `e2e-iced-release-ci` (needs `ROOST_ICED_BIN`, a real release binary).
- Host lanes (`e2e-host-*`, `e2e-local-backend`, `e2e-old-session`) run one at a
  time, never concurrently.
- Hand-run pytest over the iced list adds
  `-m 'not session_daemon and not host_client'` (`DAEMON_E2E_DESELECT`) and, on
  a slow machine, `ROOST_TEST_TIMEOUT_SCALE=3`.
- `make test-harness` includes `test_e2e_lists.py`, which fails when a roosttest
  module and the Makefile/CI lane lists drift.
- Keep the real pointer off the Roost window during IPC-driven drag tests
  (#606), and start a gesture test with no status toast up (#608).
- For Mac work (including real input on macOS) use the `mac-test` skill.

Or run the curated lane isolated, so a run never touches your real workspace:

```bash
RUN=$(mktemp -d)
env -u HERDR_ENV -u TMUX -u ROOST_SOCKET -u ROOST_TAB_ID \
  XDG_RUNTIME_DIR="$RUN/rt" XDG_DATA_HOME="$RUN/data" XDG_STATE_HOME="$RUN/state" \
  XDG_CACHE_HOME="$RUN/cache" XDG_CONFIG_HOME="$RUN/config" \
  ROOST_TEST_TIMEOUT_SCALE=3 \
  tools/wayland/weston-run.sh make e2e-iced-ci
```

For a hand-picked module set, replace the last line with
`uv run --group test pytest <modules> -m 'not session_daemon and not host_client' --roost-target iced --roost-fresh -q`
(the Makefile's `DAEMON_E2E_DESELECT`); never the whole `tools/roosttest`
directory, which includes dedicated-lane and host modules.

- **Isolate `XDG_RUNTIME_DIR`, `XDG_DATA_HOME`, `XDG_STATE_HOME`,
  `XDG_CACHE_HOME` and `XDG_CONFIG_HOME`** to a scratch dir (plus `ROOST_CONFIG`
  for a lane that seeds config), or set `ROOST_STATE_DIR` to redirect just
  `state.json` — the
  dev `roost-iced` bundle profile otherwise reads/writes your real
  `~/.local/share/roost-iced/state.json` and dials your real
  `$XDG_RUNTIME_DIR/roost-iced/roost.sock`.
- `tools/wayland/weston-run.sh` is the Wayland-primary tier (headless weston,
  what `iced-build-e2e`'s Wayland lane runs in CI); swap in `xvfb-run -a
  --server-args="-screen 0 2560x1440x24"` for the X11 secondary tier — use
  CI's screen size if a geometry-sensitive test flakes on a smaller default.
- Unset `HERDR_ENV` (and `TMUX`, etc.) first: it leaks through an isolated Roost
  into its tabs. **Never run `/usr/bin/roost --version`** to probe an install:
  there is no such flag and it launches the real UI; use `dpkg -s roost`.
- Real X input under Xvfb comes from `xdotool`; a Wayland lane with real keys is
  weston's x11 backend with kiosk-shell, nested in Xvfb.
- `--roost-fresh` makes the harness own a hermetic UI; `ROOST_TEST_MODE=1`
  unlocks the gated test ops (`tab.feed_pty_bytes`, etc.).

## See the UI live / drive it by hand

The binary inherits your real `DISPLAY`/`WAYLAND_DISPLAY`, so launching it puts
a window on your COSMIC desktop — keep it isolated so your workspace is
untouched:

```bash
# Export the isolation env for the WHOLE sequence — roostctl resolves the
# socket from the same XDG vars, so a prefix on only the UI line would
# leave every roostctl below dialing the normal namespace instead.
export XDG_RUNTIME_DIR="$RUN/rt" XDG_DATA_HOME="$RUN/data" XDG_STATE_HOME="$RUN/state"
# An inherited ROOST_SOCKET outranks --target, so clear it (and the tab id) too.
unset ROOST_SOCKET ROOST_TAB_ID
ROOST_TEST_MODE=1 ./target/debug/roost-iced > "$RUN/roost.log" 2>&1 &
rc=./target/debug/roostctl                # the repo build; bare `roostctl` needs a PATH install
$rc --target iced identify                # wait for the socket
$rc --target iced project create --name Test
$rc --target iced tab open --project-id <id> --cwd "$HOME" -- bash
$rc --target iced notify --tab <id> --title "…" --body "…"
$rc --target iced screenshot --out /tmp/shot.png   # in-process render, no OS capture
```

## Where logs live

The dev `roost-iced` profile writes `$XDG_STATE_HOME/roost-iced/roost.log`
(default `~/.local/state/roost-iced/roost.log`) and tees to stdout — so if
you isolated `XDG_STATE_HOME` above, the log follows it into `$RUN/state`.
A packaged install (built with `--features roost-iced/linux-package`, what
the `.deb` ships) uses the production `linux` profile instead:
`~/.local/state/roost/roost.log`. Set `RUST_LOG=info,roost_ipc=debug` for
per-frame IPC tracing.
