---
name: linux-test
description: Run roost's Linux iced tests from a Mac inside a shed VM (Apple VZ microVM with a real kernel + /dev/uinput). The only local way to exercise the cage+uinput Wayland pointer-drag guard, which Docker Desktop can't run (its LinuxKit kernel has no /dev/uinput). Use when asked to run/verify Linux behavior, the e2e-iced suites, or the Wayland iced real-input checks locally on a Mac (vs. only on CI).
---

# Linux testing on a Mac (via shed)

roost's Linux UI (`crates/roost-iced/`, iced — what the `.deb` ships as
`/usr/bin/roost`) and its real-input test tiers — X11/Xvfb, weston/Wayland,
and the **cage + `/dev/uinput` Wayland pointer-drag guard** — only run on
Linux. Docker Desktop **cannot** run the drag tier: its shared LinuxKit
kernel has no `uinput`. A **shed** is an Apple VZ Linux microVM with a *real
Ubuntu kernel* (+ uinput built in, + `sudo`), so it runs all three tiers,
with the repo mounted via VirtioFS (edit on the Mac, build+test in the VM).

Wayland is the **primary** lane here — winit's Wayland backend is only
exercised on a real compositor, never under Xvfb, so Wayland-specific bugs
slip through an X11-only run.

## Prerequisites
- `shed` CLI installed + a shed-server online (`shed server list` shows `online`).
  If shed isn't set up, see the ../shed macOS quickstart; stop and tell the user.
- macOS / Apple Silicon.

## Run it (one wrapper)
`tools/shed/shed-test.sh` provisions on first use, builds `roost-iced` +
`roostctl` shed-local (via `tools/shed/build-in-shed.sh`, so your Mac
`target/` + ghostty outputs are never clobbered), then runs the three iced
real-input lanes. Run it from the repo root. The persistent `roost-dev` box
IS the day-to-day cache (stop/start reuses its build cache); the
**snapshot is opt-in** — a bare run does NOT auto-snapshot, so run
`--snapshot-base` once if you want fast cold re-creates after a teardown:

```bash
tools/shed/shed-test.sh                 # ensure box, build, run the three iced real-input lanes
tools/shed/shed-test.sh --build-only    # just build roost-iced + roostctl in the shed
tools/shed/shed-test.sh --shell         # drop into the dev shed (repo at ~/roost)
tools/shed/shed-test.sh --snapshot-base # cache the provisioned box for fast future boots
tools/shed/shed-test.sh --reprovision   # rebuild box + snapshot from scratch
tools/shed/shed-test.sh --stop          # stop the VM when done (it's a heavy env)
```

The three lanes it runs, in order:
1. `tools/input/linux/iced_clipboard_check.py` — X11 real-input clipboard
   (also covers tab/project drag reorder + the delete-confirm keyboard flow).
2. `tools/input/linux/iced_native_file_drop_check.py` — native X11 file-drop
   target (a throwaway GTK app is the XDND drag *source*; `roost-iced` itself
   stays GTK-free, verified independently by `make check-iced`'s boundary gate).
3. `tools/input/linux/iced_wayland_clipboard_check.py` — the **cage +
   /dev/uinput Wayland real-seat guard**, including `_wayland_tab_reorder`'s
   real compositor-seat drag assertions. This mirrors CI's soft
   `e2e-iced-wayland-drag` job — same signal, locally, before you push.

All three set `ROOST_REQUIRE_REAL_INPUT=1` — without it these scripts SKIP
(exit 0) when cage/uinput/the binary are missing, which would go
green-by-skip silently. A green run of lane 3 ends with the guard reporting
success on a real compositor seat, not a skip.

## The e2e-iced functional suite in the shed

`shed-test.sh` only runs the three real-input lanes above; the pytest
**functional** suite (`tools/roosttest`, what `make e2e-iced` runs) needs its
own invocation. Weston (Wayland) is the primary tier here; Xvfb (X11) is the
secondary fallback for when a check needs a stable, non-Wayland baseline.
Point the harness at the shed-local binary via `ROOST_ICED_BIN` (it does
read that override for the iced target — no bind-mount needed):

```bash
tools/shed/shed-test.sh --build-only            # builds ~/rt/debug/{roost-iced,roostctl}
shed exec roost-dev -- bash -lc '
  cd ~/roost
  mkdir -p /tmp/xdgrt-iced && chmod 700 /tmp/xdgrt-iced
  # Wayland (primary): headless weston, the Iced CI Wayland lane.
  ROOST_ICED_BIN=$HOME/rt/debug/roost-iced XDG_RUNTIME_DIR=/tmp/xdgrt-iced ROOST_TEST_MODE=1 \
    tools/wayland/weston-run.sh \
    pytest tools/roosttest/test_smoke.py \
      tools/roosttest/test_iced_walking_skeleton.py \
      --roost-target iced --roost-fresh -q
  # X11 (secondary): the same modules under Xvfb.
  ROOST_ICED_BIN=$HOME/rt/debug/roost-iced XDG_RUNTIME_DIR=/tmp/xdgrt-iced ROOST_TEST_MODE=1 \
    xvfb-run -a --server-args="-screen 0 1920x1080x24" \
    pytest tools/roosttest/test_smoke.py \
      tools/roosttest/test_iced_walking_skeleton.py \
      --roost-target iced --roost-fresh -q'
```

Swap in the full curated list (`ICED_E2E_TESTS` in the Makefile) for a
CI-equivalent run — the smoke + walking-skeleton pair above is the quick
sanity check, not the whole gate. There is no `uv` in the shed (CI uses
`uv run`); use the system `pytest` binary instead.

**`XDG_RUNTIME_DIR`**: set it to a fresh dir for isolation. (Unset is
not broken — both the harness and `roost-ipc` fall back to the same
`/tmp/roost-iced-<uid>/roost.sock`, so they still find each other.) Note
`ROOST_ICED_BIN` only overrides the binary path; it does not change which
bundle profile it resolves — a plain `cargo build -p roost-iced` (no
`linux-package` feature, what `build-in-shed.sh` does) still resolves the
**dev** `roost-iced` profile (`$XDG_RUNTIME_DIR/roost-iced/roost.sock`), which
is what the harness expects.

### Visual screenshot on real Linux
Launch the shed binary directly under Xvfb or weston (skip the harness),
seed via `roostctl`, `screenshot` to a mount path, read it on the Mac
(`target/` is gitignored): `… screenshot --out ~/roost/target/.shot.png`
then open `target/.shot.png` on the Mac. This is the way to see the *real*
Linux render (translucency still needs a real compositor). UI-only states
are reachable headlessly via the palette ops — `roostctl … palette open`
then `palette activate <id>` (positional id, e.g. `close_project`) drives
the same dispatch as the keybind, so you can screenshot modal overlays with
no XTEST.

### Dev loop: a matching `roost-session` daemon on a real remote
A separate loop from the three lanes above — for exercising the
host-sessions bootstrap (Add Host → install → connect) against a real
Linux remote, not for the iced real-input tests. `tools/session/dev-session.sh`
builds `roost-session` in a shed of the target's own architecture
(`roost-dev` for aarch64; a shed on a remote shed server such as `mini3`
for amd64 — no cross-compile), fetches it back proving the version pin
matches this checkout, and can point a local `Roost-Iced.app` at it via
`ROOST_SESSION_INSTALL_BIN` so the product's own bootstrap does the
install. See `tools/session/README.md` and
`docs/development/host-sessions.md`'s "Dev loop" section — the live
SSH-severance harness that runs against a host connected this way is a
separate tool (`tools/session/live/`), not covered here.

## How it works (so you can debug it)
- **`.shed/provision.yaml`** — an `install` hook (once: build deps for
  `roost-iced`, weston/cage/seatd, Xvfb/xdotool, python test deps, GTK4-dev
  — kept ONLY as the XDND drag-source dependency of
  `iced_native_file_drop_check.py`, not to build `roost-iced` itself — via
  mise) and a `startup` hook (every boot: start seatd + `chmod 0666
  /dev/uinput /run/seatd.sock` so the drag test can inject — these reset to
  root-only each boot).
- **Box model:** a long-lived `roost-dev` shed + a `roost-base` snapshot cache.
  Treat both as a *cache* — assume a shed upgrade invalidates them; just
  `--reprovision` (or `shed delete roost-dev -f; shed snapshot delete roost-base -f`)
  and re-run. The snapshot makes a fresh box boot in seconds instead of
  re-running the full install hook.
- **`tools/shed/build-in-shed.sh`** — bind-mounts shed-local dirs over the
  hardcoded `third_party/ghostty/{src,out}` and points `CARGO_TARGET_DIR` at
  a shed-local dir (`~/rt`), so the build never touches the macOS artifacts
  in the mount.

## Gotchas
- First provision + first build are slow (apt + ghostty zig + cargo); the
  snapshot + shed-local cargo cache make repeat runs fast.
- **Disk fills with cargo incremental artifacts.** `~/rt/debug/incremental`
  can grow to fill the VM's disk under repeated builds — `cargo-101`/`mkdir
  ENOSPC` errors are the symptom. Check `df` first; `rm -rf
  ~/rt/debug/incremental` frees roughly 7G.
- `shed exec` runs `bash -lc` (login PATH works) but does **not** pick up
  `usermod` group changes until the VM restarts — that's why the startup hook
  `chmod`s the seat/uinput nodes instead of relying on the `video` group.
- Piping a hung test through `| tail` loses its output (Python buffers stdout
  off a tty, and a kill drops the buffer). Use `python3 -u … > file 2>&1` so
  partial output survives a hang.
- **`pkill -f` inside `shed exec … bash -lc '…'` kills the script's own
  shell**: the `-f` pattern matches the full `bash -lc <script>` command line,
  which *contains* the pattern you typed — the script dies mid-run and
  `shed exec` returns 255. Use `pkill -x <binary-name>`, anchor the pattern
  to the binary path, or kill by saved PID (`APP=$!; kill $APP`).
