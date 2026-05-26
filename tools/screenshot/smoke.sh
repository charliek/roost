#!/usr/bin/env bash
# End-to-end smoke run against a live Roost UI, driven entirely through
# roostctl. Creates its own throwaway project + two tabs, walks the
# agent-state / notification / hook-lifecycle / focus surface, and drops
# a labeled PNG per step into an output dir alongside a manifest.md that
# says what each shot should show.
#
#   tools/uitest/smoke.sh mac                 # default outdir
#   tools/uitest/smoke.sh gtk /tmp/roost-gtk  # custom outdir
#
# The screenshots are the verification surface: an agent (or human)
# reads them against manifest.md. The script asserts the mechanical
# bits it can (state strings via `tab list`, project cascade-close) and
# leaves pixel-level checks to the reader. Exits non-zero if a
# mechanical assertion fails.
set -euo pipefail
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/lib.sh"

TARGET="${1:?usage: smoke.sh <mac|gtk> [outdir]}"
ut_init "${TARGET}"
OUT="${2:-/tmp/roost-uitest-${TARGET}-$(date +%Y%m%d-%H%M%S)}"
mkdir -p "${OUT}"
: > "${OUT}/manifest.md"
{
  echo "# Roost UI smoke run — target=${TARGET}"
  echo
  echo "Generated $(date). Read each shot against its expectation."
  echo
} >> "${OUT}/manifest.md"

fail() { echo "ASSERT FAILED: $*" >&2; exit 1; }
state_of() { rc tab list 2>/dev/null | sed -n "s/^  tab $1 \[\([a-z_]*\)\].*/\1/p"; }
assert_state() {
  local got; got="$(state_of "$1")"
  [[ "${got}" == "$2" ]] || fail "tab $1 state: want '$2' got '${got}'"
}

echo "==> ensuring ${TARGET} UI is up"
ut_launch

echo "==> creating throwaway project + 2 tabs"
rc project create --name "uitest" --cwd /tmp >/dev/null
sleep 0.5
PID="$(rc project list 2>/dev/null | sed -n 's/^project \([0-9]*\) — uitest .*/\1/p' | tail -1)"
[[ -n "${PID}" ]] || fail "could not find created project"
TAB_A="$(rc tab open --project-id "${PID}" --cwd /tmp --title A 2>/dev/null | sed -E 's/opened tab ([0-9]+).*/\1/')"
TAB_B="$(rc tab open --project-id "${PID}" --cwd /tmp --title B 2>/dev/null | sed -E 's/opened tab ([0-9]+).*/\1/')"
[[ -n "${TAB_A}" && -n "${TAB_B}" ]] || fail "could not open tabs (A=${TAB_A} B=${TAB_B})"
echo "    project=${PID} tabs: A=${TAB_A} B=${TAB_B}"
sleep 1

# --- T1: state colour progression + rollup -----------------------------
rc tab set-state --state running --tab "${TAB_A}" >/dev/null
rc tab set-state --state needs_input --tab "${TAB_B}" >/dev/null
sleep 1
assert_state "${TAB_A}" running
assert_state "${TAB_B}" needs_input
shot "${OUT}" 01-states >/dev/null
expect "${OUT}" 01-states "Tab A leading dot BLUE (running); Tab B leading dot AMBER (needs_input); the 'uitest' sidebar row stripe is AMBER (needs_input wins the rollup over running)."

# --- T2: notification badge on the inactive tab -------------------------
rc tab focus --tab "${TAB_A}" >/dev/null
rc notify --title "uitest" --body "ping" --tab "${TAB_B}" >/dev/null
sleep 1
shot "${OUT}" 02-notify >/dev/null
expect "${OUT}" 02-notify "Tab A active; Tab B (inactive) shows an AMBER leading dot AND a BLUE trailing notification badge; the 'uitest' sidebar row shows a BLUE trailing project badge."

# --- T2b: focus clears the badge (and switches the visible tab) ---------
rc tab focus --tab "${TAB_B}" >/dev/null
sleep 1
shot "${OUT}" 03-focus-clears >/dev/null
expect "${OUT}" 03-focus-clears "Focusing Tab B switched the visible tab to B and cleared its notification badge; the project's trailing badge is gone. (If the active tab/terminal did NOT change, external IPC focus is broken — see the tab.focus fix.)"

# --- T5: claude-hook lifecycle ------------------------------------------
HOOK_ENV=(env "ROOST_TAB_ID=${TAB_A}" "ROOST_SOCKET=${UT_SOCK}")
"${HOOK_ENV[@]}" "${UT_RC}" claude-hook session-start </dev/null >/dev/null
"${HOOK_ENV[@]}" "${UT_RC}" claude-hook prompt-submit </dev/null >/dev/null
assert_state "${TAB_A}" running
echo '{"message":"choose a path"}' | "${HOOK_ENV[@]}" "${UT_RC}" claude-hook notification >/dev/null
assert_state "${TAB_A}" needs_input
"${HOOK_ENV[@]}" "${UT_RC}" claude-hook stop </dev/null >/dev/null
assert_state "${TAB_A}" idle
sleep 1
shot "${OUT}" 04-hook-idle >/dev/null
expect "${OUT}" 04-hook-idle "After the claude-hook lifecycle, Tab A leading dot is GRAY (idle)."
"${HOOK_ENV[@]}" "${UT_RC}" claude-hook session-end </dev/null >/dev/null
assert_state "${TAB_A}" none

# --- lifecycle: closing the last tab cascade-closes the project ---------
rc tab close --tab "${TAB_A}" >/dev/null
sleep 0.3
rc tab close --tab "${TAB_B}" >/dev/null
sleep 1
if rc project list 2>/dev/null | grep -q "— uitest "; then
  fail "project ${PID} should have cascade-closed after its last tab closed"
fi
shot "${OUT}" 05-cascade-closed >/dev/null
expect "${OUT}" 05-cascade-closed "The 'uitest' project is gone from the sidebar (closing its last tab cascade-closed it)."

echo
echo "==> smoke run OK. Screenshots + manifest in: ${OUT}"
echo "    Read ${OUT}/manifest.md and inspect each PNG."
