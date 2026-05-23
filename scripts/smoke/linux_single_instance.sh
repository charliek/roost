#!/usr/bin/env bash
# scripts/smoke/linux_single_instance.sh — manual smoke for the
# GTK Linux UI's single-instance enforcement.
#
# Pre-req:
#   cargo build -p roost-linux       # debug binary at target/debug/roost
#   DISPLAY=:0 (or any live X / Wayland session)
#
# What this checks:
#   1. First launch produces exactly one roost process.
#   2. Second launch returns quickly (does not stay running) — the
#      flock guard in single_instance.rs rejects it.
#
# Run after a clean state (no other roost running). Exits
# non-zero on assertion failure; the first launch is left running
# unless CLEANUP=1 is set.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
BIN="${REPO_ROOT}/target/debug/roost"

if [ ! -x "${BIN}" ]; then
    BIN="${REPO_ROOT}/target/release/roost"
fi
if [ ! -x "${BIN}" ]; then
    echo "FAIL: roost not built — run \`cargo build -p roost-linux\` first" >&2
    exit 1
fi

if [ -z "${DISPLAY:-}${WAYLAND_DISPLAY:-}" ]; then
    echo "SKIP: no DISPLAY or WAYLAND_DISPLAY set; GTK can't start" >&2
    exit 0
fi

cleanup() {
    if [ "${CLEANUP:-0}" = "1" ] && [ -n "${FIRST_PID:-}" ]; then
        kill "${FIRST_PID}" 2>/dev/null || true
    fi
}
trap cleanup EXIT

# Pre-flight.
if pgrep -f "${BIN}" >/dev/null 2>&1; then
    echo "FAIL: a roost process is already running; quit it first" >&2
    pgrep -af "${BIN}"
    exit 1
fi

echo "==> Launch #1"
"${BIN}" &
FIRST_PID=$!
sleep 2

if ! kill -0 "${FIRST_PID}" 2>/dev/null; then
    echo "FAIL: first launch died within 2s" >&2
    exit 1
fi
echo "ok: first launch alive (pid=${FIRST_PID})"

echo "==> Launch #2 (expect exit within 2s)"
"${BIN}" &
SECOND_PID=$!
sleep 2

if kill -0 "${SECOND_PID}" 2>/dev/null; then
    echo "FAIL: second launch still alive (pid=${SECOND_PID}) — flock guard failed" >&2
    kill "${SECOND_PID}" 2>/dev/null || true
    exit 1
fi
echo "ok: second launch exited"

echo
echo "PASS: GTK single-instance enforced"
echo "      (first instance still running at pid ${FIRST_PID}; set CLEANUP=1 to kill it)"
