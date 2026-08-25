---
name: popos-test
description: Run roost's Linux iced tests locally on a native Pop!_OS COSMIC dev box (no VM). The native-Linux counterpart to the `linux-test` skill (which is for Macs via a shed VM). Use when on Linux and asked to run/verify the e2e-iced suite, the Wayland functional tier, or check which test tiers run locally vs. need CI/shed. Covers the apt deps, `make e2e-iced`/`e2e-iced-ci`, the weston headless tier, the seat0 caveat (the live COSMIC session owns input, so the cage+uinput real-input tier can't run locally), workspace isolation, and where logs live.
---

# Linux testing on a native Pop!_OS COSMIC box

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
make e2e-iced-ci    # same lane, fresh + isolated state (CI parity — sets ROOST_TEST_MODE)
```

On a live COSMIC session `$WAYLAND_DISPLAY` is already set, so these targets
already run the Wayland-primary path natively — no weston needed. `e2e-iced`
checks `WAYLAND_DISPLAY` and skips the clipboard tests
(`ICED_CLIPBOARD_TESTS`) when it's set, since Wayland clipboard needs a
focused seat/serial only a real interactive session provides; that means
running it on your live desktop is actually the one place those tests *don't*
run — use `xvfb-run` (below) to exercise them.

Or reach for the pieces directly, isolated so a run never touches your real
workspace:

```bash
RUN=$(mktemp -d)
XDG_RUNTIME_DIR="$RUN/rt" XDG_DATA_HOME="$RUN/data" XDG_STATE_HOME="$RUN/state" \
  ROOST_TEST_MODE=1 ROOST_TEST_TIMEOUT_SCALE=3 \
  tools/wayland/weston-run.sh \
  uv run --group test pytest tools/roosttest --roost-target iced --roost-fresh -q
```

- **Isolate `XDG_DATA_HOME` / `XDG_STATE_HOME` / `XDG_RUNTIME_DIR`** to a
  scratch dir, or set `ROOST_STATE_DIR` to redirect just `state.json` — the
  dev `roost-iced` bundle profile otherwise reads/writes your real
  `~/.local/share/roost-iced/state.json` and dials your real
  `$XDG_RUNTIME_DIR/roost-iced/roost.sock`.
- `tools/wayland/weston-run.sh` is the Wayland-primary tier (headless weston,
  what `iced-build-e2e`'s Wayland lane runs in CI); swap in `xvfb-run -a
  --server-args="-screen 0 2560x1440x24"` for the X11 secondary tier — use
  CI's screen size if a geometry-sensitive test flakes on a smaller default.
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
