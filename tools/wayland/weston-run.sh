#!/usr/bin/env bash
# Run a command under a headless weston Wayland compositor — the Wayland
# analogue of `xvfb-run`. Exists to close the X11-only test blind spot:
# GTK's GDK-Wayland backend (gdksurface-wayland.c) is ONLY exercised on
# Wayland, never under Xvfb, so Wayland-specific bugs (e.g. the DnD
# drag-icon-surface frame_callback abort) slip through an X11-only CI.
#
# Usage:  tools/wayland/weston-run.sh <command> [args...]
# Example: tools/wayland/weston-run.sh uv run --group test pytest tools/roosttest \
#            --roost-target gtk --roost-fresh -v
#
# Sets GDK_BACKEND=wayland + WAYLAND_DISPLAY for the child and unsets
# DISPLAY so GTK can't silently fall back to X11. GSK_RENDERER defaults to
# cairo (no GPU on headless runners). Honors an existing XDG_RUNTIME_DIR;
# otherwise mints a private one so the compositor + roost sockets agree.
#
# Caveat: weston headless exercises GTK's *generic* Wayland path (where the
# crash lives), not cosmic-comp — COSMIC-specific quirks still need a real
# COSMIC box. And it has no input devices, so this drives the IPC suite, not
# pointer/gesture input (that needs ydotool + /dev/uinput).
set -euo pipefail

if [ "$#" -eq 0 ]; then
  echo "usage: $0 <command> [args...]" >&2
  exit 2
fi

# A Wayland socket needs a private 0700 dir. Reuse an inherited
# XDG_RUNTIME_DIR only if it's actually writable (GitHub runners usually
# leave it unset, or pointed at a root-owned /run/user we can't mkdir into);
# otherwise mint a throwaway one so both the compositor and roost sockets
# agree on the same path.
if [ -z "${XDG_RUNTIME_DIR:-}" ] || [ ! -w "${XDG_RUNTIME_DIR}" ]; then
  XDG_RUNTIME_DIR="$(mktemp -d)"
fi
export XDG_RUNTIME_DIR
chmod 700 "$XDG_RUNTIME_DIR" 2>/dev/null || true

socket="wayland-roost-$$"
log="${ROOST_WL_LOG:-${XDG_RUNTIME_DIR}/weston-headless.log}"

weston --backend=headless-backend.so --socket="$socket" \
  --width="${ROOST_WL_WIDTH:-2560}" --height="${ROOST_WL_HEIGHT:-1440}" \
  --idle-time=0 >"$log" 2>&1 &
weston_pid=$!

cleanup() { kill "$weston_pid" 2>/dev/null || true; }
trap cleanup EXIT

# Wait for the compositor socket to appear (≈10s budget).
for _ in $(seq 1 50); do
  [ -S "${XDG_RUNTIME_DIR}/${socket}" ] && break
  sleep 0.2
done
if [ ! -S "${XDG_RUNTIME_DIR}/${socket}" ]; then
  echo "weston-run: compositor socket never appeared; weston log:" >&2
  cat "$log" >&2 || true
  exit 1
fi

export WAYLAND_DISPLAY="$socket"
export GDK_BACKEND=wayland
export GSK_RENDERER="${GSK_RENDERER:-cairo}"
unset DISPLAY

"$@"
status=$?
cleanup
trap - EXIT
exit "$status"
