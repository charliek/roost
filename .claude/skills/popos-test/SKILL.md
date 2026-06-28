---
name: popos-test
description: Run roost's Linux GTK4/Wayland tests locally on a native Pop!_OS COSMIC dev box (no VM). The native-Linux counterpart to the `linux-test` skill (which is for Macs via a shed VM). Use when on Linux and asked to run/verify the e2e-gtk suite, reproduce a GTK crash/critical, or check which test tiers run locally vs. need CI/shed. Covers the apt deps, the seat0 caveat (the live COSMIC session owns input, so the real-pointer cage tier can't run locally), workspace isolation, and the gdb trick for focus criticals.
---

# Linux testing on a native Pop!_OS COSMIC box

You're already on Linux, so — unlike the Mac `linux-test` path — you do **not**
need a shed VM. Build + run the suite directly. The catch is the **real-input**
tier: your live COSMIC session owns `seat0`, so a second compositor can't grab
input devices. Use this matrix:

| Tier | Runs locally? | How |
|---|---|---|
| **X11 / Xvfb** (`e2e-gtk` — the main pytest suite) | ✅ | `xvfb-run` + pytest |
| **weston / Wayland** | ✅ | weston headless |
| **headless `cage`** (rendering only) | ✅ | `WLR_BACKENDS=headless cage -- …` |
| **`cage` + `/dev/uinput`** (real-pointer drag/focus guard) | ❌ | CI `e2e-gtk-wayland-drag` or a shed VM |

The last row fails on the live desktop with `libseat: Could not take control of
session: Device or resource busy` — COSMIC holds the seat/VT. That tier needs an
*isolated* seat (CI's headless runner + seatd, or the shed VM's fresh kernel).
Don't fight it locally.

## Prerequisites (one-time)

```bash
sudo apt-get install -y \
  libgtk-4-dev libadwaita-1-dev pkg-config libclang-dev clang \
  weston cage xvfb xdotool python3-pytest wl-clipboard zsh
```

- **Do NOT install `seatd`** on Pop!_OS COSMIC: it collides with `pop-desktop` /
  `pop-de-cosmic` (apt reports a `pkgProblemResolver` break). It isn't needed —
  `libseat1` is already present and `logind` provides `seat0`. (`seatd` is only
  for the headless real-input tier, which you run on CI/shed anyway.)
- For the (CI/shed-only) uinput tier, the device perms are:
  `sudo modprobe uinput && sudo chmod 0666 /dev/uinput` (resets each boot).
- Toolchain: `mise install` (rust/zig pinned), then `third_party/ghostty/build.sh` once.

## Run the e2e-gtk suite locally

```bash
make e2e-gtk            # the convenience target
# …or directly, isolated so it never touches your real workspace:
RUN=$(mktemp -d)
XDG_DATA_HOME="$RUN/data" XDG_STATE_HOME="$RUN/state" ROOST_E2E_LOG_DIR="$RUN/logs" \
  ROOST_TEST_MODE=1 ROOST_TEST_TIMEOUT_SCALE=3 \
  xvfb-run -a --server-args="-screen 0 2560x1440x24" \
  uv run --group test pytest tools/roosttest --roost-target gtk --roost-fresh -q --timeout=90
```

- **Isolate `XDG_DATA_HOME` + `XDG_STATE_HOME`** to a scratch dir — `state.json`
  resolves from `XDG_DATA_HOME` (NOT `XDG_STATE_HOME`), so without this the
  harness loads/rewrites your real `~/.local/share/roost/state.json`.
- **Use the big xvfb screen** (`2560x1440x24`, what CI uses): the sidebar /
  theme-frame geometry tests time out ("sidebar to settle visible with non-zero
  width") on a small default screen — that's environmental, not a real failure.
- `--roost-fresh` makes the harness own a hermetic UI; `ROOST_TEST_MODE=1`
  unlocks the gated test ops (`tab.feed_pty_bytes`, etc.).
- The session-level `Gtk-CRITICAL` gate (in `conftest.py`'s `_ui_session`) fails
  the run on any non-allowlisted GLib `*-CRITICAL` — a regression like #234
  surfaces there as an ERROR-at-teardown.

## See the UI live / drive it by hand

The binary inherits your real `DISPLAY`/`WAYLAND_DISPLAY`, so launching it puts a
window on your COSMIC desktop — keep it isolated so your workspace is untouched:

```bash
XDG_DATA_HOME="$RUN/data" XDG_STATE_HOME="$RUN/state" ROOST_TEST_MODE=1 \
  ./target/debug/roost > "$RUN/roost.log" 2>&1 &
roostctl --target gtk identify           # wait for the socket
roostctl --target gtk project create --name Test
roostctl --target gtk tab open --project-id <id> --cwd "$HOME" -- bash
roostctl --target gtk notify --tab <id> --title "…" --body "…"
roostctl --target gtk screenshot --out /tmp/shot.png   # in-process render, no OS capture
```

## Debug a GTK crash / critical

Launch under gdb with a **pending** breakpoint on the assertion emitter — it
catches the first `GTK_IS_WIDGET`-family critical with a full backtrace:

```bash
gdb -batch -nx -ex 'set confirm off' -ex 'set breakpoint pending on' \
  -ex 'break g_return_if_fail_warning' -ex run -ex 'bt 40' -ex kill -ex quit \
  --args ./target/debug/roost
```

- **Don't `gdb -p` an already-running roost** — `yama ptrace_scope=1` blocks
  attaching to a non-descendant. Launch *under* gdb instead.
- The log rate-limiter (`install_log_filter`) bounds any warning storm (~20/s),
  so even a per-frame critical can't peg a core or fill a redirected log — a
  bounded capture is safe (this is what made #234 diagnosable without a 52 GB log).
