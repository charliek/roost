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
`tools/shed/shed-test.sh` provisions on first use, caches via a snapshot, and
builds shed-local so your Mac `target/` + ghostty outputs are never clobbered:

```bash
tools/shed/shed-test.sh                 # ensure box, build, run the Wayland drag guard
tools/shed/shed-test.sh --build-only    # just build roost-linux + roostctl in the shed
tools/shed/shed-test.sh --shell         # drop into the dev shed (repo at ~/roost)
tools/shed/shed-test.sh --snapshot-base # cache the provisioned box for fast future boots
tools/shed/shed-test.sh --reprovision   # rebuild box + snapshot from scratch
tools/shed/shed-test.sh --stop          # stop the VM when done (it's a heavy env)
```

A green drag run mirrors CI's non-blocking `e2e-gtk-wayland-drag` job — same
signal, locally, before you push.

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
