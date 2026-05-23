#!/usr/bin/env bash
# scripts/smoke/stale_socket_recovery.sh — manual smoke for M6
# stale-socket recovery on the Mac UI.
#
# What this checks:
#   1. A live Roost creates the socket at the canonical path.
#   2. `kill -9` of the Roost process leaves the socket file on
#      disk (no graceful unlink possible after SIGKILL).
#   3. A relaunch successfully binds the socket again — the M6
#      `bindWithRecovery` path in IPCServer.swift detects the
#      stale entry (via the connect() probe), unlinks it, and
#      rebinds.
#
# Pre-req: mac/scripts/bundle.sh debug.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
APP="${REPO_ROOT}/mac/build/Roost.app"
SOCKET="${HOME}/Library/Caches/Roost/roost.sock"
PGREP_PATTERN="Roost.app/Contents/MacOS/Roost"

if [ ! -d "${APP}" ]; then
    echo "FAIL: ${APP} not built" >&2
    exit 1
fi

cleanup() {
    if [ "${CLEANUP:-0}" = "1" ]; then
        pkill -f "${PGREP_PATTERN}" 2>/dev/null || true
    fi
}
trap cleanup EXIT

if [ -n "$(pgrep -f "${PGREP_PATTERN}" 2>/dev/null || true)" ]; then
    echo "FAIL: a Roost process is already running; quit it first" >&2
    exit 1
fi

echo "==> Launch first instance"
open -a "${APP}"
sleep 2

FIRST_PID=$(pgrep -f "${PGREP_PATTERN}" | head -1)
if [ -z "${FIRST_PID}" ]; then
    echo "FAIL: first launch didn't produce a Roost process" >&2
    exit 1
fi
echo "ok: first launch pid=${FIRST_PID}"

if [ ! -S "${SOCKET}" ]; then
    echo "FAIL: socket ${SOCKET} not created by first launch" >&2
    exit 1
fi
echo "ok: socket exists at ${SOCKET}"

echo "==> kill -9 first instance"
kill -9 "${FIRST_PID}"
sleep 1

# The socket file should still be on disk (kill -9 skips deinit).
if [ ! -S "${SOCKET}" ]; then
    echo "WARN: socket disappeared after SIGKILL (the OS sometimes" >&2
    echo "      cleans aborted UDS sockets; M6 recovery is then a" >&2
    echo "      no-op for this case but the relaunch must still" >&2
    echo "      succeed)" >&2
fi

echo "==> Relaunch"
open -a "${APP}"
sleep 2

SECOND_PID=$(pgrep -f "${PGREP_PATTERN}" | head -1)
if [ -z "${SECOND_PID}" ]; then
    echo "FAIL: relaunch didn't produce a Roost process" >&2
    exit 1
fi
if [ "${SECOND_PID}" = "${FIRST_PID}" ]; then
    echo "FAIL: relaunch picked up the original (already-dead) pid?!" >&2
    exit 1
fi
echo "ok: relaunch pid=${SECOND_PID}"

# Verify the socket is a fresh listener — connect should not
# block / refuse. Use `nc -U` with a 1s timeout for a no-op
# probe (the JSON IPC server's accept loop should pick it up).
if ! command -v nc >/dev/null 2>&1; then
    echo "SKIP: \`nc\` not on PATH; can't verify the socket is live"
else
    if echo "" | nc -U -w 1 "${SOCKET}" >/dev/null 2>&1; then
        echo "ok: socket is live (nc -U succeeded)"
    else
        echo "FAIL: socket present but not accepting connections" >&2
        exit 1
    fi
fi

echo
echo "PASS: stale-socket recovery (kill -9 → relaunch → fresh listener)"
echo "      (Roost still running; set CLEANUP=1 to terminate after the test)"
