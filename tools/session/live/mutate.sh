#!/usr/bin/env bash
# Negative controls for `live.sh` — the regression test for the harness
# itself. A lane whose failure mode has never been observed is a lane
# whose failure mode is assumed, not tested, which is what bit this
# harness three times.
#
#   ./mutate.sh M1        the ::1 trap: IPv4-only rules sever nothing, and
#                         `probe_severed timeout` must say so.
#   ./mutate.sh M2        the signal trap: kill the DISPLAY instead of the
#                         app, and the graceful-exit assertion must trip.
#   ./mutate.sh M3        L3/L2's flatness assertion, fed a ladder that
#                         DOES advance. Must trip.
#   ./mutate.sh M4b       L4b's leak assertion, with the app SIGKILLed so
#                         its exit teardown never runs. Must trip.
#   ./mutate.sh M5        the socket-activation trap: stopping ssh.service
#                         alone leaves sshd reachable, and
#                         `probe_severed refused` must say so.
#   ./mutate.sh selftest  all five; green only if every one trips.
#
# Two rules make these controls worth having:
#
#   * Each asserts its **precondition first**. A control that trips for
#     the wrong reason rots into vacuity — M1 against a box where
#     `localhost` is IPv4 would "pass" by severing for real, proving
#     nothing about the trap it is named for.
#   * Each drives the SAME predicate `live.sh` asserts on, out of
#     `lib.sh`. A control with its own copy of the check tests the copy.
#
# "PASS (control)" means the assertion correctly tripped.
set -euo pipefail

MODE="${1:?usage: mutate.sh M1|M2|M3|M4b|M5|selftest}"
TAG="$MODE"
# shellcheck source=tools/session/live/lib.sh
. "$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)/lib.sh"

# The three shapes a control can end in, said once so a scrollback reads
# the same way for all five.
tripped() { say "PASS (control): the assertion tripped, as it must — $1"; }
inert() { say "FAIL (control): the assertion held — it CANNOT detect $1"; }

# ── setup shared by the three controls that need a running app ───────
control_app_setup() {
  preflight
  start_app "$ART/$TAG-ui.out"
  ensure_host
  connect_and_wait || fail "setup failed: never connected"
}

# Sever for real, and prove it: the mutation in M2/M3/M4b is in what is
# then asserted, not in how the link dies.
control_sever() {
  say "severing the route for real (both address families)"
  blackhole_rules_on both || fail "could not install the DROP rules"
  probe_severed timeout || fail "precondition failed: the route is not black-holed"
  pkill -f 'roost-ssh-' 2>/dev/null || true
  wait_for_drop || fail "precondition failed: the app still reports connected after the link was cut"
}

# ── M1: localhost is ::1, so IPv4-only rules drop nothing ────────────
control_M1() {
  preflight
  say "precondition: localhost must resolve to ::1 first, or IPv4-only rules really would sever"
  local first
  first=$(getent ahosts localhost | head -1 | awk '{print $1}' || true)
  say "    getent ahosts localhost -> ${first:-<nothing>}"
  [ "$first" = "::1" ] ||
    fail "precondition failed: localhost resolves to '$first', so this box cannot exhibit the trap"
  say "installing IPv4-only DROP rules — the trap: they install cleanly and drop not one packet"
  blackhole_rules_on v4 || fail "could not install the IPv4-only rules"
  say "requiring probe_severed timeout to FAIL"
  if probe_severed timeout; then
    inert "a route that is still up over ::1"
    return 1
  fi
  tripped "an IPv4-only severance leaves the app's route untouched"
}

# ── M2: signal the display instead of the app ────────────────────────
control_M2() {
  control_app_setup
  control_sever
  say "precondition: something the app owns must be alive, or 'no leftovers' is true by construction"
  wait_for_ssh_inflight 60
  expect_ge "$(ssh_count)" 1 "precondition failed: no establish in flight"
  # The mutation's own precondition: a signal that lands on nothing is
  # not "signalling the display instead of the app", and the app dying of
  # something else would then trip this control for the wrong reason.
  kill -0 "$XVFB" 2>/dev/null ||
    fail "precondition failed: the harness's Xvfb (pid $XVFB) is already gone"
  kill -0 "$APP" 2>/dev/null ||
    fail "precondition failed: the app (pid $APP) is already gone, so nothing loses a display"
  say "killing the harness's own Xvfb — the DISPLAY, not the app (pid $XVFB, app is $APP)"
  kill -TERM "$XVFB"
  XVFB=""
  wait_for_app_exit 60 ||
    fail "the app outlived its display: this box cannot exhibit the trap (a finding, not a control)"
  say "the app exited with status $APP_RC"
  blackhole_rules_off
  sleep 8
  local left scratch
  left=$(ssh_count); scratch=$(scratch_count)
  say "ssh children left: $left ; scratch dirs left: $scratch"
  if assert_clean_exit "$APP_RC" "after the display was killed"; then
    inert "an app that died of a lost display instead of quitting"
    return 1
  fi
  # The specific signature §3.3 asks this control for. Tripping on the
  # exit status alone would leave "and nothing leaked" unproven, which is
  # the half L4 actually cares about.
  [ "$APP_RC" -ne 0 ] ||
    fail "the app exited 0 with its display gone — the exit-status half of the proof is inert"
  [ "$((left + scratch))" -gt 0 ] ||
    fail "nothing was left behind — the leak half of the proof is inert"
  tripped "status $APP_RC and $left ssh + $scratch scratch dirs left behind"
}

# ── M3: a ladder that DOES advance must break the flatness check ─────
control_M3() {
  control_app_setup
  local row gen
  row=$(read_row) || fail "setup failed: host.status unreadable"
  gen=$(row_generation "$row")
  control_sever
  say "precondition: a rung must actually fire, or there is no ladder to be flat about"
  watch_ladder $((gen + 1)) 180 "an advancing ladder" ||
    fail "precondition failed: the ladder never started an attempt"
  gen=$(row_generation "$LADDER_ROW")
  # 120s, not 25s. Measured rung-to-rung gaps on this box are ~40s
  # against a black-holed route, so a shorter window is flat whether or
  # not the ladder is climbing — which is exactly how an earlier L3
  # check managed to be inert.
  if assert_flat "$gen" 120 "an advancing ladder"; then
    inert "a ladder that is still spending attempts"
    return 1
  fi
  tripped "it sees a ladder advance past generation $gen"
}

# ── M4b: no exit path, so what the app owned must leak ───────────────
control_M4b() {
  control_app_setup
  control_sever
  say "precondition: an establish must be in flight, or there is nothing to leak"
  wait_for_ssh_inflight 60
  expect_ge "$(ssh_count)" 1 "precondition failed: no establish in flight at signal time"
  # The mutation's own precondition: SIGKILL is what denies the app its
  # exit path, so an app that had already died of something else would
  # leave leftovers this control did not cause and "trip" on them.
  kill -0 "$APP" 2>/dev/null ||
    fail "precondition failed: the app (pid $APP) is already dead, so SIGKILL is not what denied it its exit path"
  say "SIGKILL -> roost-iced pid $APP (no exit path, so its teardown never runs)"
  kill -9 "$APP"
  wait_for_app_exit 30 || fail "the app survived SIGKILL"
  say "the app exited with status $APP_RC"
  blackhole_rules_off
  sleep 8
  local left scratch
  left=$(ssh_count); scratch=$(scratch_count)
  say "ssh children left: $left ; scratch dirs left: $scratch"
  if assert_clean_exit "$APP_RC" "after a SIGKILL"; then
    inert "a leak in the establishing window"
    return 1
  fi
  [ "$((left + scratch))" -gt 0 ] ||
    fail "nothing leaked, so the assertion tripped on the exit status alone — the leak half is inert"
  tripped "it sees $left ssh + $scratch scratch dirs the exit path would have taken"
}

# ── M5: socket activation brings sshd straight back ──────────────────
control_M5() {
  preflight
  say "precondition: ssh.socket must be active, or stopping the service alone really would sever"
  local socket_state
  socket_state=$(systemctl is-active ssh.socket 2>/dev/null || true)
  say "    systemctl is-active ssh.socket -> ${socket_state:-<nothing>}"
  [ "$socket_state" = active ] ||
    fail "precondition failed: ssh.socket is '$socket_state', so this box cannot exhibit the trap"
  say "stopping ssh.service ONLY — the trap: the socket unit re-activates it on the next dial"
  sudo systemctl stop ssh 2>/dev/null || sudo systemctl stop sshd 2>/dev/null ||
    fail "could not stop the sshd service"
  # A unit relationship that takes the socket down with the service would
  # sever for real, and this control would then "fail" for having
  # detected a genuine severance. That is a fact about the box, not about
  # the assertion, so it is a precondition failure and says so.
  socket_state=$(systemctl is-active ssh.socket 2>/dev/null || true)
  say "    ssh.socket after stopping the service -> ${socket_state:-<nothing>}"
  [ "$socket_state" = active ] ||
    fail "precondition failed: stopping ssh.service took ssh.socket with it, so this box cannot exhibit the trap"
  say "requiring probe_severed refused to FAIL"
  if probe_severed refused; then
    inert "a port that socket activation brings straight back"
    return 1
  fi
  tripped "a service stopped beside a live socket unit is not a severance"
}

# ── driver ───────────────────────────────────────────────────────────
# Each control in its own subshell with its own cleanup trap, so one
# that "passes by tripping" — leaving DROP rules, a stopped sshd or a
# SIGKILLed app behind — cannot poison the next.
#
# The status comes back in a global, and the subshell is a plain command
# with `errexit` cleared around it, because bash's `-e` is *ignored*
# inside anything that sits in a condition context — an `if`, or the left
# of `||` — and an explicit `set -e` within does not restore it. Written
# the obvious way (`( ... ) || rc=$?`), every failing command inside a
# control would be shrugged off: M4b's `kill -9` against an app that had
# already crashed would "succeed", and the control would then trip on a
# leak nobody caused. So `run_control` itself always returns 0 and the
# caller reads `CONTROL_RC` — which keeps the call sites out of a
# condition context too, and that is the half that actually matters.
CONTROL_RC=0
run_control() {
  local name=$1
  set +e
  (
    set -e
    TAG="$name"
    trap 'say "ABORTED: line $LINENO exited $?"' ERR
    trap live_cleanup EXIT
    "control_$name"
  )
  CONTROL_RC=$?
  set -e
  return 0
}

case "$MODE" in
  M1|M2|M3|M4b|M5)
    run_control "$MODE"
    [ "$CONTROL_RC" -eq 0 ] ||
      fail "$MODE did not trip: the harness cannot see the failure it names"
    ;;
  selftest)
    say "running every control; green only if all five trip after their preconditions pass"
    RESULTS=()
    FAILED=0
    for c in M1 M2 M3 M4b M5; do
      say "───── $c ─────"
      run_control "$c"
      if [ "$CONTROL_RC" -eq 0 ]; then
        RESULTS+=("$c tripped")
      else
        RESULTS+=("$c DID NOT TRIP")
        FAILED=1
      fi
    done
    say "───── summary ─────"
    for r in "${RESULTS[@]}"; do say "  $r"; done
    [ "$FAILED" -eq 0 ] || fail "a control did not trip: the harness cannot see the failure it names"
    say "PASS: all five controls tripped"
    ;;
  *) fail "unknown control '$MODE' (M1|M2|M3|M4b|M5|selftest)" ;;
esac
