#!/usr/bin/env bash
# scripts/smoke/mac_single_instance.sh — manual smoke for M4c +
# M6 single-instance enforcement on the Mac UI.
#
# Pre-req: build the .app bundle first.
#   mac/scripts/bundle.sh debug
#
# What this checks:
#   1. Launching Roost.app once produces exactly one Roost process.
#   2. A second `open -a` returns without starting a second process.
#   3. `open -n` (Apple's "force new instance" flag) also produces
#      no second process — the flock guard wins.
#
# Run after a clean state (no other Roost running). Exits non-zero
# on any assertion failure; the running Roost is left alive for
# inspection unless `CLEANUP=1` is set.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
APP="${REPO_ROOT}/mac/build/Roost.app"
PGREP_PATTERN="Roost.app/Contents/MacOS/Roost"

if [ ! -d "${APP}" ]; then
    echo "FAIL: ${APP} not built — run mac/scripts/bundle.sh first" >&2
    exit 1
fi

pgrep_roost() {
    pgrep -f "${PGREP_PATTERN}" 2>/dev/null || true
}

cleanup() {
    if [ "${CLEANUP:-0}" = "1" ]; then
        pkill -f "${PGREP_PATTERN}" 2>/dev/null || true
    fi
}
trap cleanup EXIT

# Pre-flight: there should be no Roost running already.
if [ -n "$(pgrep_roost)" ]; then
    echo "FAIL: a Roost process is already running; quit it first" >&2
    pgrep_roost
    exit 1
fi

echo "==> Launch #1"
open -a "${APP}"
sleep 2

N1=$(pgrep_roost | wc -l | tr -d ' ')
if [ "${N1}" != "1" ]; then
    echo "FAIL: expected 1 Roost process after first launch, got ${N1}" >&2
    pgrep_roost
    exit 1
fi
echo "ok: first launch produced 1 process"

echo "==> Launch #2 via open -a (should attach to existing window)"
open -a "${APP}"
sleep 1
N2=$(pgrep_roost | wc -l | tr -d ' ')
if [ "${N2}" != "1" ]; then
    echo "FAIL: expected 1 Roost process after second open -a, got ${N2}" >&2
    pgrep_roost
    exit 1
fi
echo "ok: second launch did not spawn a second process"

echo "==> Launch #3 via open -n (force new instance)"
open -n "${APP}" || true
sleep 2
N3=$(pgrep_roost | wc -l | tr -d ' ')
if [ "${N3}" != "1" ]; then
    echo "FAIL: expected 1 Roost process after open -n, got ${N3} (flock should have rejected the second instance)" >&2
    pgrep_roost
    exit 1
fi
echo "ok: open -n second instance lost the flock race and exited"

echo
echo "PASS: single-instance enforced across open -a and open -n"
echo "      (Roost still running; set CLEANUP=1 to terminate after the test)"
