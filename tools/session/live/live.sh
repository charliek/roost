#!/usr/bin/env bash
# The live SSH lanes: a real `sshd`, a real severance, and every claim
# read back through `host.status`.
#
#   ./live.sh L1   recover:  sever, watch the ladder, restore, come back unaided
#   ./live.sh L2   settle:   sever and leave it down, watch the give-up (~8 min)
#   ./live.sh L3   hostkey:  a changed host key is never retried
#   ./live.sh L4a  quit while a retry is ARMED
#   ./live.sh L4b  quit while an establish is IN FLIGHT
#   ./live.sh L5   refused: stop sshd instead of black-holing the route
#
# "Sever" is two things on purpose. iptables black-holes the route in
# both directions, so every *retry* faces a hung TCP that has to run out
# ConnectTimeout — the closed-laptop shape. Dropping packets does not
# promptly kill the connection that is already up (roost sets no
# ServerAliveInterval), so the live mux and its execs are killed too;
# that is what a radio going away does to them, and it is what makes the
# drop land now instead of whenever TCP gives up.
#
# EVERY claim a lane makes is asserted, and `PASS` is the last line a
# lane prints. This harness reported three false passes before that rule
# existed: a lane that *echoes* "expect 0" next to a number nobody
# compares is a lane that passes while the feature is broken. Its own
# assertions are kept honest by `mutate.sh selftest`.
#
# State comes from the op that owns it (`roostctl host status --json`),
# never from what the app printed: a string in a diagnostic stream is not
# the surface a user reads, and one innocent refactor away from silence.
# ssh processes and scratch directories are still counted directly —
# those are OS facts.
#
# Parameters (defaults match tools/shed/build-in-shed.sh and a
# roost-dev-shaped shed):
#   ROOST_RT          $HOME/rt/debug   where roostctl + roost-iced are
#   ROOST_LIVE_TARGET ssh://shed@localhost   the host this drives
#   ROOST_LIVE_ART    <repo>/target/live-ssh  logs and captures
set -euo pipefail

MODE="${1:?usage: live.sh L1|L2|L3|L4a|L4b|L5}"
TAG="$MODE"
# shellcheck source=tools/session/live/lib.sh
. "$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)/lib.sh"

case "$MODE" in
  # Without this an unknown lane name runs the whole setup, asserts
  # nothing and exits 0 — silence that reads as success in a scrollback.
  L1|L2|L3|L4a|L4b|L5) ;;
  *) fail "unknown lane '$MODE' (L1|L2|L3|L4a|L4b|L5)" ;;
esac

# L1 is where §8's V5 pair is taken: the band and the op at one armed rung.
[ "$MODE" = L1 ] && LIVE_CAPTURE=1

UI_OUT="$ART/$MODE-ui.out"
trap 'say "ABORTED: line $LINENO exited $?"' ERR
trap live_cleanup EXIT

preflight
# Said here rather than at the end: PASS is the last line a lane prints.
say "artifacts: $ART"
start_app "$UI_OUT"
ensure_host

if [ "$MODE" = L3 ]; then
  say "backing up $KNOWN_HOSTS before poisoning the pinned key"
  cp "$KNOWN_HOSTS" "$KNOWN_HOSTS_BAK" || fail "could not back up $KNOWN_HOSTS"
fi

connect_and_wait || fail "never connected (the rows above are the op's own answer)"
# Every later claim is relative to this: `generation` counts attempts
# *started*, so a lane can say "the ladder spent exactly N rungs" without
# having to catch each one mid-air.
ROW=$(read_row) || fail "host.status unreadable right after connecting"
GEN0=$(row_generation "$ROW")
say "connected at generation $GEN0"

if [ "$MODE" = L3 ]; then
  # A key that no longer matches what is pinned. The retry's establish is
  # what discovers it, exactly as a real key rotation would.
  #
  # Stale material from an earlier run is removed first: `ssh-keygen`
  # refuses to overwrite, so without this the lane would either abort or
  # silently re-pin the key the *previous* run already taught ssh about.
  say "pinning a bogus host key so the next establish reads the key as CHANGED"
  rm -f "$BOGUS_KEY" "$BOGUS_KEY.pub"
  ssh-keygen -f "$KNOWN_HOSTS" -R localhost >/dev/null 2>&1 || true
  ssh-keygen -f "$KNOWN_HOSTS" -R "[localhost]:22" >/dev/null 2>&1 || true
  # `-R` exits non-zero when there was nothing pinned, which is a fine
  # outcome; the effect is what must hold, so assert the effect.
  ! ssh-keygen -f "$KNOWN_HOSTS" -F localhost >/dev/null 2>&1 ||
    fail "localhost is still pinned after ssh-keygen -R"
  ssh-keygen -t ed25519 -N "" -f "$BOGUS_KEY" -q 2>/dev/null ||
    fail "could not generate the bogus host key"
  BOGUS=$(cut -d' ' -f1,2 "$BOGUS_KEY.pub")
  [ -n "$BOGUS" ] || fail "the bogus host key is empty"
  printf 'localhost %s\n' "$BOGUS" >>"$KNOWN_HOSTS" || fail "could not pin the bogus key"
  ssh-keygen -f "$KNOWN_HOSTS" -F localhost >/dev/null 2>&1 ||
    fail "the bogus key did not land in $KNOWN_HOSTS"
fi

say "severing the link"
case "$MODE" in
  L5)
    # The other shape asked for: a port that *refuses* rather than one
    # that black-holes. ECONNREFUSED comes back at once, so the ladder
    # climbs on its backoff alone instead of on ConnectTimeout — a
    # different timing regime over the same retryable family.
    sshd_stop || fail "could not stop sshd"
    probe_severed refused || fail "the port does not refuse, so this lane would prove nothing"
    ;;
  L3)
    # No black hole here, deliberately: L3's claim is about what the
    # *establish* meets, and a rule set that is still installed when the
    # first rung fires would spend that rung on a timeout instead of on
    # the changed key. The drop is the mux dying — a bare EOF, which is
    # exactly the retryable shape that arms the rung that then reads the
    # key.
    say "dropping the live mux only, so the retry's establish reaches a real sshd"
    ;;
  *)
    blackhole_rules_on both || fail "could not install the DROP rules"
    probe_severed timeout || fail "the route is not black-holed, so this lane would prove nothing"
    ;;
esac
pkill -f 'roost-ssh-' 2>/dev/null || true
SEVERED=$(date +%s)
# Before any "it never reconnected while severed" window opens.
wait_for_drop || fail "the link was cut but the app still reports it connected"

case "$MODE" in
  L1)
    # Three rungs, not one: the drop's own retry is armed
    # unconditionally, so a single attempt proves nothing about a ladder
    # that keeps climbing — and the third rung is the first whose armed
    # window (delay 4s, jittered) is comfortably wider than the poll, so
    # `retry.attempt` is reliably *seen* climbing rather than merely
    # inferred from `generation`. `watch_ladder` also holds the "never
    # connected while severed" claim across the whole window — a single
    # sampled state after the fact could not.
    watch_ladder $((GEN0 + 3)) 360 "a route that stays black-holed" ||
      fail "the ladder did not climb as it must (the lines above say which claim broke)"
    expect_ge "$LADDER_MAX_ATTEMPT" 2 "no armed rung ever reported attempt 2"
    say "restoring the link"
    blackhole_rules_off
    wait_for_reconnect 240 || fail "never came back"
    ROW=$RECONNECT_ROW
    case "$(row_rollup "$ROW")" in
      "$GAVE_UP_BAND"*) fail "the ladder gave up on an outage it recovered from: $ROW" ;;
    esac
    say "PASS: came back unaided after $(( $(date +%s) - SEVERED ))s, generation $(row_generation "$ROW")"
    ;;

  L2)
    say "leaving it down; waiting for the ladder to spend its budget"
    wait_for_the_give_up 500 || fail "never gave up"
    BAND=$(row_rollup "$GIVEUP_ROW")
    say "    band: $BAND"
    # The budget off the band's own copy, so the lane does not hardcode
    # the production number to check that the attempts add up to it.
    TRIES=$(grep -oE 'after [0-9]+ tries' <<<"$BAND" | grep -oE '[0-9]+' | head -1 || true)
    [ -n "$TRIES" ] || fail "the give-up band carries no attempt count: $GIVEUP_ROW"
    # The lane unsets both ladder seams on purpose, so anything under the
    # production budget means a test knob leaked into the run.
    expect_ge "$TRIES" 10 "gave up short of the production attempt budget"
    # Two independent counts of the same thing: the band's copy, and the
    # attempts the op counted. A ladder that settled early would agree
    # with itself and disagree with `generation`.
    expect_eq "$(row_generation "$GIVEUP_ROW")" "$((GEN0 + TRIES))" \
      "the attempts started do not add up to the budget the band says it spent"
    expect_eq "$(ssh_count)" 0 "ssh processes survived the give-up"
    say "restoring the route: a settled host must not dial even once it is back"
    blackhole_rules_off
    # 45s, not 20s: the ladder's backoff caps at 30s, so a shorter window
    # is flat whether or not a timer nobody reported is still armed — it
    # would simply fire after the window closed. 45s is wider than the
    # widest gap the ladder can leave, so an unreported rung has to show
    # up inside it.
    assert_flat "$(row_generation "$GIVEUP_ROW")" 45 "a settled ladder" ||
      fail "the ladder kept going after it gave up"
    expect_eq "$(ssh_count)" 0 "a settled ladder dialed again once the route came back"
    say "PASS: settled after $TRIES attempts, $(( $(date +%s) - SEVERED ))s, and stayed settled"
    ;;

  L3)
    # The settle, not a fixed sleep: the reason that matters is the one
    # an attempt *started after the drop* produced, and `generation` is
    # what tells the two apart.
    SETTLED=""
    for _ in $(seq 1 90); do
      ROW=$(read_row) || { sleep 1; continue; }
      if [ "$(row_generation "$ROW")" -gt "$GEN0" ] &&
         [ "$(row_state "$ROW")" = disconnected ] &&
         ! row_has_retry "$ROW" && [ -n "$(row_reason "$ROW")" ]; then
        SETTLED=$ROW; break
      fi
      sleep 1
    done
    [ -n "$SETTLED" ] || { dump_status; fail "nothing settled after the drop"; }
    REASON=$(row_reason "$SETTLED")
    say "    reason: $REASON"
    # Asserted on `reason`, the band's untruncated input: `rollup` is
    # capped at 60 characters and would cut this copy in half.
    case "$REASON" in
      *"$CHANGED_KEY_COPY"*) ;;
      *) fail "the changed-key copy never appeared: $REASON" ;;
    esac
    case "$REASON" in
      *"$CHANGED_KEY_WARNING"*) ;;
      *) fail "the changed-key warning is missing: $REASON" ;;
    esac
    # The *unknown*-key remedy. Seeing it means the failure was
    # classified as an unpinned key, not a changed one — a different
    # story, and the one case where accepting is exactly wrong advice.
    case "$REASON" in
      *"$UNKNOWN_KEY_REMEDY"*) fail "the accept-this-key remedy appeared: classified unknown, not CHANGED" ;;
    esac
    GEN1=$(row_generation "$SETTLED")
    # Exactly one rung past the drop. "Never retried" is asserted where
    # it lives: the changed key is what the retry's own establish
    # discovers, so one attempt is expected — a second must never be.
    expect_eq "$GEN1" "$((GEN0 + 1))" "the ladder spent more than one rung past the changed key"
    # 120s: measured rung-to-rung gaps here are ~40s against a
    # black-holed route, so a 25s window is flat whether or not the
    # ladder is climbing. The negative control (mutate.sh M3) is what
    # proves this window bites.
    assert_flat "$GEN1" 120 "a changed host key" ||
      fail "the ladder advanced past a changed host key"
    say "PASS: exactly one attempt past the changed key, flat for 120s (generation $GEN1)"
    ;;

  L5)
    # Against a refused port the backoff is the only thing pacing the
    # ladder, so a few rungs land in seconds. One would be the drop's own
    # retry and would say nothing about climbing.
    #
    # Stated rather than hidden: the classified copy ("connecting to …
    # Connection refused") is not observable here. The app folds the
    # family in and arms the rung in the same update, so `reason` is
    # already the armed-rung line by the time any op can read it — the
    # copy surfaces only in a give-up, which this lane recovers before.
    # `probe_severed refused` above is what carries the claim that the
    # port refused rather than black-holed.
    watch_ladder $((GEN0 + 3)) 180 "a port that refuses" ||
      fail "the ladder did not climb against a refused port as it must (see the lines above)"
    expect_ge "$LADDER_MAX_ATTEMPT" 2 "no armed rung ever reported attempt 2"
    say "restarting sshd"
    sshd_start
    # Exit codes are not the proof here: on a socket-activated box,
    # starting the service beside its own socket unit can fail while the
    # port is perfectly back. The probe is the proof.
    probe_live "restored port" || fail "sshd did not come back"
    wait_for_reconnect 150 || fail "never recovered"
    ROW=$RECONNECT_ROW
    case "$(row_rollup "$ROW")" in
      "$GAVE_UP_BAND"*) fail "the ladder gave up on an outage it recovered from: $ROW" ;;
    esac
    say "PASS: recovered unaided from a refused port after $(( $(date +%s) - SEVERED ))s"
    ;;

  L4a|L4b)
    if [ "$MODE" = L4a ]; then
      # An armed rung is a timer that has been *created* and has not yet
      # fired. The first delay is ~1s, so a slow poll can easily notice
      # the ladder only after the timer fired and the establish is away —
      # which is L4b's case, not this one. So wait for the state this
      # lane is named for on the cheap OS facts (nothing dialing, nothing
      # on disk) and confirm it on the op before signalling. The window
      # widens with every rung (the delay doubles to a 30s cap), so it is
      # reliably catchable.
      say "waiting for the ladder to be between attempts"
      ARMED=0
      for _ in $(seq 1 600); do
        if [ "$(ssh_count)" -eq 0 ] && [ "$(scratch_count)" -eq 0 ]; then
          if ROW=$(read_row) && row_has_retry "$ROW"; then ARMED=1; break; fi
        fi
        sleep 0.2
      done
      [ "$ARMED" -eq 1 ] ||
        fail "never caught the ladder between attempts (ssh=$(ssh_count), scratch=$(scratch_count))"
      say "SIGTERM while a retry is ARMED: $(row_rollup "$ROW")"
      expect_eq "$(ssh_count)" 0 "an establish is in flight: this is L4b's case, not L4a's"
      expect_eq "$(scratch_count)" 0 "a scratch dir exists, so an establish has already started"
    else
      say "SIGTERM while an establish is IN FLIGHT (black-holed, so it sits in ConnectTimeout)"
      wait_for_ssh_inflight 60
      # Without this the lane degrades silently: with no ssh in flight it
      # is testing an ordinary shutdown, and "zero leftovers" is then
      # true by construction.
      expect_ge "$(ssh_count)" 1 "no establish in flight at signal time"
      say "in-flight ssh processes at signal time: $(ssh_count)"
    fi
    # The app, not the X server and not a wrapper around it. The harness
    # started roost-iced as its own child precisely so this signal lands
    # on the process under test and its exit status is readable here;
    # signalling anything else tears the display out from under it, and
    # the teardown under test never runs. `mutate.sh M2` is that control.
    say "SIGTERM -> roost-iced pid $APP"
    kill -TERM "$APP"
    wait_for_app_exit 60 || fail "the UI did not exit on SIGTERM"
    say "the app exited with status $APP_RC"
    blackhole_rules_off
    sleep 8
    # The graceful path is what is under test, and this is the whole
    # proof of it: status 0 from the app's own quit path, and nothing it
    # owned left running or on disk.
    assert_clean_exit "$APP_RC" "after a graceful SIGTERM" ||
      fail "the app did not leave through its own quit path"
    say "PASS: exited 0 and left nothing behind after a graceful SIGTERM"
    ;;
esac
