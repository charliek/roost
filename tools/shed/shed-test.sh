#!/usr/bin/env bash
# Run roost's Linux tests in a shed (Apple VZ Linux microVM) from a Mac.
#
# Why a shed and not Docker: a shed boots a real Ubuntu kernel with /dev/uinput,
# so it runs the FULL suite — including the cage+uinput Wayland pointer-drag
# guard that Docker Desktop fundamentally can't (its LinuxKit kernel has no
# uinput). The repo is mounted via --local-dir (edit on the Mac, build+test in
# the VM); .shed/provision.yaml installs deps (install hook) and opens
# /dev/uinput + the seatd socket each boot (startup hook).
#
# Box model: a long-lived `roost-dev` shed + a `roost-base` snapshot cache.
# Treat both as a CACHE — on a shed upgrade, run with --reprovision (or
# `shed delete roost-dev -f; shed snapshot delete roost-base -f`) and re-run.
# The build goes to a shed-local CARGO_TARGET_DIR so it never touches your Mac
# target/ (different arch).
#
# Usage:
#   tools/shed/shed-test.sh                 # ensure box, build, run the drag guard
#   tools/shed/shed-test.sh --build-only    # just build roost-linux in the shed
#   tools/shed/shed-test.sh --shell         # ensure box + drop into a shell
#   tools/shed/shed-test.sh --snapshot-base # cache the provisioned box as roost-base
#   tools/shed/shed-test.sh --reprovision   # delete box + snapshot, rebuild from scratch
#   tools/shed/shed-test.sh --stop          # stop the dev box (frees the VM)
set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
SHED=roost-dev
SNAP=roost-base
RT='$HOME/rt'   # shed-local CARGO_TARGET_DIR (literal $HOME — expanded in-guest)
# The default ~5G upper layer is too small for apt + ghostty (zig) + the cargo
# target. Give the build room.
UPPER="${ROOST_SHED_UPPER:-30G}"
log() { printf '\033[36m[shed-test]\033[0m %s\n' "$*"; }

have_shed() { shed list 2>/dev/null | awk '{print $1}' | grep -qx "$SHED"; }
shed_status() { shed list 2>/dev/null | awk -v s="$SHED" '$1==s {print $NF}'; }
have_snap() { shed snapshot list 2>/dev/null | awk '{print $1}' | grep -qx "$SNAP"; }
in_shed() { shed exec "$SHED" -- bash -lc "$1"; }

ensure_box() {
  if have_shed; then
    case "$(shed_status)" in
      *stopped*|*Stopped*) log "starting existing $SHED"; shed start "$SHED" >/dev/null ;;
      *) log "reusing running $SHED" ;;
    esac
  elif have_snap; then
    log "spawning $SHED from cached snapshot $SNAP (+ mounting repo)"
    shed create "$SHED" --from-snapshot "$SNAP" --local-dir "$REPO" --upper-size "$UPPER" >/dev/null
  else
    log "no box or snapshot — provisioning fresh (install hook installs deps; first run is slow)"
    shed create "$SHED" --local-dir "$REPO" --upper-size "$UPPER" >/dev/null
    log "TIP: run '$0 --snapshot-base' once to cache this for fast future boots"
  fi
}

build() {
  log "building roost-linux + roostctl in the shed (all artifacts shed-local; Mac target/ + ghostty untouched)"
  in_shed "chmod +x ~/roost/tools/shed/build-in-shed.sh; ~/roost/tools/shed/build-in-shed.sh"
}

run_drag() {
  log "running the cage+uinput Wayland pointer-drag guard"
  # The startup hook already opened /dev/uinput + the seatd socket this boot.
  # SCALE=5: a just-booted-from-snapshot VM is cold — the first tab spawns can
  # be slow enough to trip wait_tab_attached at the CI default (3); the extra
  # headroom only matters on a cold box and never slows a passing run.
  in_shed "cd ~/roost && \
    ROOST_BIN=$RT/debug/roost ROOSTCTL=$RT/debug/roostctl \
    ROOST_TEST_MODE=1 ROOST_REQUIRE_REAL_INPUT=1 ROOST_TEST_TIMEOUT_SCALE=5 \
    python3 tools/input/linux/wayland_drag_check.py"
}

case "${1:-}" in
  --reprovision)
    log "tearing down $SHED + $SNAP"
    have_shed && shed delete "$SHED" -f || true
    have_snap && shed snapshot delete "$SNAP" -f || true
    ensure_box; build; run_drag ;;
  --snapshot-base)
    have_shed || { log "no $SHED to snapshot — run with no args first"; exit 1; }
    log "stopping $SHED to snapshot it (the dev box restarts after)"
    shed stop "$SHED" >/dev/null
    have_snap && shed snapshot delete "$SNAP" -f >/dev/null || true
    shed snapshot create "$SHED" "$SNAP" --comment "roost linux test base ($(date +%F))"
    shed start "$SHED" >/dev/null
    log "cached as snapshot $SNAP" ;;
  --stop)
    have_shed && shed stop "$SHED" && log "stopped $SHED (start again with any command)" ;;
  --shell)
    ensure_box; log "dropping into $SHED (repo at ~/roost)"; shed console "$SHED" ;;
  --build-only)
    ensure_box; build ;;
  ""|--run)
    ensure_box; build; run_drag ;;
  *)
    grep '^#' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
esac
