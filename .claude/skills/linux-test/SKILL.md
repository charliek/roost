---
name: linux-test
description: Run roost's Linux tests (GTK4/Wayland) from a Mac inside a shed VM. The only local way to exercise the cage+uinput Wayland pointer-drag guard, which Docker Desktop can't run (its LinuxKit kernel has no /dev/uinput). Use when asked to run/verify Linux behavior, the e2e-gtk* suites, or the Wayland drag reorder locally on a Mac (vs. only on CI).
---

# Linux testing on a Mac (via shed)

roost's Linux UI (`crates/roost-linux/`, GTK4) and its three test tiers —
X11/Xvfb, weston/Wayland, and the **cage + /dev/uinput Wayland pointer-drag
guard** — only run on Linux. Docker Desktop **cannot** run the drag tier: its
shared LinuxKit kernel has no `uinput`. A **shed** is an Apple VZ Linux microVM
with a *real Ubuntu kernel* (+ uinput built in), so it runs all three tiers,
with the repo mounted via VirtioFS (edit on the Mac, build+test in the VM).

## Prerequisites
- `shed` CLI installed + a shed-server online (`shed server list` shows `online`).
  If shed isn't set up, see the ../shed macOS quickstart; stop and tell the user.
- macOS / Apple Silicon.

## Run it (one wrapper)
`tools/shed/shed-test.sh` provisions on first use and builds shed-local so your
Mac `target/` + ghostty outputs are never clobbered. Run it from the repo root.
The persistent `roost-dev` box IS the day-to-day cache (stop/start reuses its
build cache); the **snapshot is opt-in** — a bare run does NOT auto-snapshot, so
run `--snapshot-base` once if you want fast cold re-creates after a teardown:

```bash
tools/shed/shed-test.sh                 # ensure box, build, run the Wayland drag guard
tools/shed/shed-test.sh --build-only    # just build roost-linux + roostctl in the shed
tools/shed/shed-test.sh --shell         # drop into the dev shed (repo at ~/roost)
tools/shed/shed-test.sh --snapshot-base # cache the provisioned box for fast future boots
tools/shed/shed-test.sh --reprovision   # rebuild box + snapshot from scratch
tools/shed/shed-test.sh --stop          # stop the VM when done (it's a heavy env)
```

A green run ends with `PASS: Wayland pointer-drag guard — no surface abort`
(preceded by a `sidebar reorder OK: …` line — the hard gate). That mirrors CI's
non-blocking `e2e-gtk-wayland-drag` job — same signal, locally, before you push.

## Other tiers in the shed (beyond the drag guard)

`shed-test.sh` runs only the **Wayland drag guard**. The full **pytest `e2e-gtk`
suite** and the **real-input (XTEST) checks** need extra env knobs — each cost a
debugging cycle, so they're recorded here.

### pytest e2e-gtk (the required CI gate)
The pytest harness (`tools/roosttest/ui.py`) **hardcodes `target/debug/roost`**
(it does NOT read `ROOST_BIN`) and `cargo build`s into it if missing — which on
the VirtioFS mount is the Mac's arm64 **mach-O** (won't exec on Linux) and would
clobber the Mac `target/`. So **bind-mount the shed-local Linux binaries over it**
(guest-namespace only, never written to the mount), like `build-in-shed.sh` does
for ghostty:

```bash
tools/shed/shed-test.sh --build-only            # builds ~/rt/debug/{roost,roostctl}
shed exec roost-dev -- bash -lc '
  sudo mount --bind ~/rt/debug ~/roost/target/debug
  trap "sudo umount ~/roost/target/debug" EXIT
  head -c4 ~/roost/target/debug/roost | grep -q ELF      # the shed has no `file`
  cd ~/roost
  GDK_BACKEND=x11 ROOST_TEST_MODE=1 XDG_RUNTIME_DIR=/tmp/xdgrt \
    xvfb-run -a --server-args="-screen 0 2560x1440x24" \
    pytest tools/roosttest --roost-target gtk --roost-fresh -q'
```

Three knobs that each cost a cycle:
- **`XDG_RUNTIME_DIR`** must be set (a fresh dir). Without it the UI uses the
  fallback `/tmp/roost-<uid>/roost.sock` but the harness looks at
  `$XDG_RUNTIME_DIR/roost/roost.sock` → `wait_alive` times out → **~101 "ERROR at
  setup"**. The #1 trap.
- **`GDK_BACKEND=x11`** (matches CI). Without it GTK4 hits the libEGL/DRI3 path
  under Xvfb and the UI never becomes ready.
- **system `/usr/bin/pytest`** — there is no `uv` in the shed (CI uses `uv run`).

### real-input (XTEST) checks
`tools/input/linux/real_input_check.py` launches its OWN Xvfb + roost, so it
needs no bind-mount — just point it at the shed binaries via env (it DOES read
`ROOST_BIN`/`ROOSTCTL`) and scale timeouts up for the loaded VM:

```bash
shed exec roost-dev -- bash -lc '
  cd ~/roost
  ROOST_BIN=$HOME/rt/debug/roost ROOSTCTL=$HOME/rt/debug/roostctl \
    ROOST_TEST_MODE=1 ROOST_TEST_TIMEOUT_SCALE=3 \
    python3 tools/input/linux/real_input_check.py'
```

### visual screenshot on real Linux
Launch the shed binary directly under Xvfb (skip the harness), seed via
`roostctl`, `screenshot` to a mount path, read it on the Mac (`target/` is
gitignored): `… screenshot --out ~/roost/target/.shot.png` then open
`target/.shot.png` on the Mac. GTK chrome differs Linux↔macOS, so this is the
way to see the *real* Linux render (translucency still needs a real compositor).

## How it works (so you can debug it)
- **`.shed/provision.yaml`** — an `install` hook (once: GTK4-dev, cage, seatd,
  weston, xvfb, xdotool, pytest, rust/zig via mise) and a `startup` hook (every
  boot: start seatd + `chmod 0666 /dev/uinput /run/seatd.sock` so the drag test
  can inject — these reset to root-only each boot).
- **Box model:** a long-lived `roost-dev` shed + a `roost-base` snapshot cache.
  Treat both as a *cache* — assume a shed upgrade invalidates them; just
  `--reprovision` (or `shed delete roost-dev -f; shed snapshot delete roost-base -f`)
  and re-run. The snapshot makes a fresh box boot in seconds instead of
  re-running the full install hook.
- **`tools/shed/build-in-shed.sh`** — bind-mounts shed-local dirs over the
  hardcoded `third_party/ghostty/{src,out}` and points `CARGO_TARGET_DIR` at a
  shed-local dir, so the Linux build never touches the macOS artifacts in the
  mount. The harness reads `ROOST_BIN`/`ROOSTCTL` (set by the script) to find the
  shed-local binary.

## Gotchas
- First provision + first build are slow (apt + ghostty zig + cargo); the
  snapshot + shed-local cargo cache make repeat runs fast.
- `shed exec` runs `bash -lc` (login PATH works) but does **not** pick up
  `usermod` group changes until the VM restarts — that's why the startup hook
  `chmod`s the seat/uinput nodes instead of relying on the `video` group.
- This is *generic* wlroots Wayland (cage), not cosmic-comp — it guards GTK's
  generic GDK-Wayland path (where the crash lived); COSMIC-specific quirks still
  need a real COSMIC box.
- Under shed load (concurrent builds + repeated runs) a couple of timing-
  sensitive checks flake: `test_sidebar_collapsed_state_survives_relaunch` (a
  deferred palette-toggle racing an immediate `window.metrics` read) and
  `real_input_check.py`'s `_check_tab_reorder` (many-shell spawn). They flake on
  a clean tree too — prove regression-vs-env with a same-env baseline run before
  blaming a change; re-run, or raise `ROOST_TEST_TIMEOUT_SCALE`.
- Piping a hung test through `| tail` loses its output (Python buffers stdout off
  a tty, and a kill drops the buffer). Use `python3 -u … > file 2>&1` so partial
  output survives a hang — essential when the thing you're testing *is* a hang.
