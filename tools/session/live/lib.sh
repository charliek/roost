#!/usr/bin/env bash
# shellcheck shell=bash disable=SC2034  # globals and copy constants are read by the two entry scripts
# Shared machinery for the live SSH lanes and their negative controls.
#
# Sourced by `live.sh` (which turns every predicate into an assertion)
# and by `mutate.sh` (which requires the SAME predicate to trip). That
# sharing is the point: a control that re-implements the assertion it is
# supposed to break proves nothing about the lane.
#
# Predicates in here RETURN non-zero and explain themselves; they never
# exit. `live.sh` wraps each one in `|| fail`, so a lane still stops at
# its first broken claim, and `mutate.sh` can ask "did it trip?".

# ── parameters ───────────────────────────────────────────────────────
# Defaults match `tools/shed/build-in-shed.sh` (CARGO_TARGET_DIR=$HOME/rt)
# and the in-VM ssh target of a `roost-dev`-shaped shed. Nothing here is
# tied to a worktree name: the repo is derived from this file's location.
LIVE_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
REPO=$(cd -- "$LIVE_DIR/../../.." && pwd)
RT="${ROOST_RT:-$HOME/rt/debug}"
HOST_TARGET="${ROOST_LIVE_TARGET:-ssh://shed@localhost}"
ART="${ROOST_LIVE_ART:-$REPO/target/live-ssh}"

# One place for the target, so the severance probes dial exactly what
# the app dials. Probing bare `localhost` while the app dials
# `shed@localhost` proves nothing about the app's route.
#
# Every ssh this harness runs is non-interactive, so `BatchMode` is part
# of the shared option list rather than repeated at each call: a prompt
# from one of them would hang the lane instead of failing it.
PROBE_TARGET=${HOST_TARGET#ssh://}
PROBE_SSH_OPTS=(-o BatchMode=yes)
case "$PROBE_TARGET" in
  *:*)
    PROBE_SSH_OPTS+=(-p "${PROBE_TARGET##*:}")
    PROBE_TARGET=${PROBE_TARGET%:*}
    ;;
esac
PROBE_CONNECT_TIMEOUT=5
PROBE_HARD_TIMEOUT=12

KNOWN_HOSTS=$HOME/.ssh/known_hosts
KNOWN_HOSTS_BAK=$HOME/.ssh/known_hosts.roost-live
BOGUS_KEY=/tmp/roost-live-bogus-hostkey

export XDG_RUNTIME_DIR=/tmp/xdgrt-roost-live
export ROOST_TEST_MODE=1 RUST_LOG=info
# Production ladder values: this is the live criterion, not a lane with
# a shortened budget. `ROOST_TEST_MODE` gates these seams, and inheriting
# one from an interactive shell would silently retune the ladder.
unset ROOST_SSH_RECONNECT_ATTEMPTS ROOST_SSH_RECONNECT_BASE_MS

# The band copy this harness reads, restated rather than imported —
# `host_conn.rs`'s `retry_line` and `gave_up_copy` through the sidebar
# reducer. It is what a user reads, so a change to it should have to be
# made twice, on purpose.
DISCONNECTED_BAND="disconnected — "
GAVE_UP_BAND="${DISCONNECTED_BAND}reconnect gave up after "
CHANGED_KEY_COPY="has CHANGED since it was last seen"
CHANGED_KEY_WARNING="Do not accept the new key"
UNKNOWN_KEY_REMEDY="review and accept it"

TAG="${TAG:-live}"
say() { printf '[%s] %s\n' "$TAG" "$*"; }
fail() { say "FAIL: $*"; exit 1; }
expect_eq() { [ "$1" = "$2" ] || fail "$3 (got '$1', want '$2')"; }
expect_ge() { [ "$1" -ge "$2" ] || fail "$3 (got '$1', want >= $2)"; }
ctl() { "$RT/roostctl" "$@"; }

# ── preconditions ────────────────────────────────────────────────────
require_tools() {
  local missing=()
  command -v jq >/dev/null 2>&1 || missing+=(jq)
  command -v Xvfb >/dev/null 2>&1 || missing+=(Xvfb)
  if [ "${#missing[@]}" -gt 0 ]; then
    say "missing: ${missing[*]}"
    fail "this harness needs jq (and Xvfb) — apt install jq by hand once on an existing shed"
  fi
  [ -x "$RT/roostctl" ] || fail "no roostctl at $RT/roostctl (set ROOST_RT, or build in the shed)"
  [ -x "$RT/roost-iced" ] || fail "no roost-iced at $RT/roost-iced (set ROOST_RT, or build in the shed)"
}

# ── the state surface ────────────────────────────────────────────────
# Every claim about the connection is read from `host.status`, the op
# that owns it. `--json` prints the op's result unaltered, so what is
# asserted here is the same contract the functional lane asserts.
HOST_ID=""

#
# An absent host row is a FAILED read, not an empty one: `jq -c` prints
# the four characters `null` for a missing `.hosts[0]`, and every field
# read off that string comes back empty — which reads as "not connected"
# to `wait_for_drop` and would report a drop the app never made. `//
# empty` turns it into nothing, and nothing is refused here.
status_row() {
  local out row rc=0
  out=$(ctl host status --id "$HOST_ID" --json 2>/dev/null) || rc=$?
  [ "$rc" -eq 0 ] || return 1
  row=$(jq -c '.hosts[0] // empty' <<<"$out") || return 1
  [ -n "$row" ] || return 1
  printf '%s' "$row"
}

# The op's own answer, indented — the diagnostic every lane prints when a
# claim about the connection does not hold.
dump_status() { ctl host status --json 2>&1 | sed 's/^/    /'; }

# field <row> <jq path> — empty string for an absent field, so a caller
# can compare without jq's `null` leaking into shell arithmetic.
field() { jq -r "$2 // empty" <<<"$1"; }

# The two reads a lane never gets to miss. `generation` counts attempts
# *started* (one per explicit connect, launch redial, or ladder rung), so
# it is the monotonic edge: two rungs can fail with byte-identical copy,
# and a poll can land between an armed timer and the next, but nothing
# starts an attempt without moving this number.
row_generation() { field "$1" .generation; }
row_state() { field "$1" .state; }
row_rollup() { field "$1" .rollup; }
row_reason() { field "$1" .reason; }
row_attempt() { field "$1" .retry.attempt; }
row_budget() { field "$1" .retry.budget; }
row_delay_ms() { field "$1" .retry.delay_ms; }
row_has_retry() { [ -n "$(field "$1" .retry)" ]; }

# One status read, retried briefly: a lane that mistakes a momentarily
# busy socket for a claim about the app would be reporting on itself.
# Non-zero means "no row" — a busy socket, a UI that is not answering, or
# a host the op does not know. Callers that poll past it must say so
# where they tolerate it; none of them may read a claim out of it.
read_row() {
  local row i
  for i in 1 2 3; do
    row=$(status_row) && [ -n "$row" ] && { printf '%s' "$row"; return 0; }
    sleep 0.5
  done
  return 1
}

# ── ssh processes and scratch dirs (OS facts, not log lines) ─────────
# Only this tunnel's own ssh processes: matched on the scratch-dir name,
# which never appears in this script's own command line.
tunnel_ssh() { pgrep -f 'roost-ssh-' | grep -v "^$$\$" || true; }
ssh_count() { tunnel_ssh | grep -c . || true; }

# BOTH roots, because the app takes the first that fits: roost-ipc's
# `pick_socket_dir` tries `$TMPDIR` and falls back to `/tmp`. Checking
# only `/tmp` lets a leak into `$TMPDIR` pass as zero.
scratch_dirs() {
  {
    for d in "${TMPDIR:-/tmp}" /tmp; do
      d=${d%/}
      for p in "$d"/roost-ssh-*; do
        [ -e "$p" ] && printf '%s\n' "$p"
      done
    done
  } | sort -u
}
scratch_count() { scratch_dirs | grep -c . || true; }

# A prior lane that was interrupted can leave a live mux behind, and a
# live mux answers for a route this lane believes it severed. Ask ssh to
# close its own master first (`-O exit`), then take the directory.
#
# Asking is best-effort on purpose: a control socket whose master is
# already dead makes `ssh -O exit` fail (255, "control socket connect:
# Connection refused"), and that stale socket is the very litter this
# reap exists to remove. Letting the failure escape would abort preflight
# under `set -e` in exactly the state preflight is for — so it is logged
# and the directory is taken anyway.
reap_ssh_masters() {
  local dir
  while IFS= read -r dir; do
    [ -n "$dir" ] || continue
    if [ -S "$dir/ctl" ]; then
      ssh -o ControlPath="$dir/ctl" "${PROBE_SSH_OPTS[@]}" \
        -O exit "$PROBE_TARGET" >/dev/null 2>&1 ||
        say "    stale control socket $dir/ctl (ssh -O exit failed); removing it"
    fi
    rm -rf "$dir"
  done < <(scratch_dirs)
  pkill -f 'roost-ssh-' >/dev/null 2>&1 || true
}

# ── severance, and proof of it ───────────────────────────────────────
# Every rule this harness inserts carries a comment tag, and every `-D`
# names that tag in the spec it deletes. That is the whole fence: a
# firewall is shared machinery, so a box may already carry a port-22
# DROP rule an administrator put there — and a cleanup that deleted "any
# rule matching this shape" would silently take it away and never put it
# back. Tagging makes removal exact: this harness takes back what it
# installed and cannot touch anything else.
LIVE_RULE_TAG="roost-live-harness"

# The two rules a severance is, written once so `-I` and `-D` cannot
# drift: a delete spec that differs from the insert by one token matches
# nothing, quietly leaving the route black-holed for the next lane.
#
# The shapes are deliberately asymmetric — outbound is filtered on the
# destination port, inbound on the source port — so what dies is a dial
# *to* an sshd, not an inbound ssh session into the box running this.
BLACKHOLE_OUT=(-p tcp --dport 22 -m comment --comment "$LIVE_RULE_TAG" -j DROP)
BLACKHOLE_IN=(-p tcp --sport 22 -m comment --comment "$LIVE_RULE_TAG" -j DROP)

# BOTH families: `localhost` resolves to ::1 here, so an IPv4-only rule
# set installs cleanly, reports nothing amiss, and drops not one packet.
# `mutate.sh M1` is the control that proves this harness can see that.
#
# `-I` stacks, and an interrupted lane never reaches its `-D`. One stale
# DROP silently black-holes the *next* lane — which then "proves" a
# severance it never performed. So both directions of every family are
# deleted until none matches, before installing and after removing. The
# loop is bounded because `-D` failing is the only way it ends, and a
# `sudo` that fails for some other reason would otherwise spin forever.
blackhole_rules_off() {
  local t n
  for t in iptables ip6tables; do
    n=0
    while sudo "$t" -D OUTPUT "${BLACKHOLE_OUT[@]}" 2>/dev/null; do
      n=$((n + 1)); [ "$n" -lt 32 ] || break
    done
    n=0
    while sudo "$t" -D INPUT "${BLACKHOLE_IN[@]}" 2>/dev/null; do
      n=$((n + 1)); [ "$n" -lt 32 ] || break
    done
  done
  return 0
}

# families: "both" (the lanes) or "v4" (M1's mutation).
blackhole_rules_on() {
  local families=${1:-both} t list
  blackhole_rules_off
  if [ "$families" = v4 ]; then list=(iptables); else list=(iptables ip6tables); fi
  for t in "${list[@]}"; do
    sudo "$t" -I OUTPUT "${BLACKHOLE_OUT[@]}" || return 1
    sudo "$t" -I INPUT "${BLACKHOLE_IN[@]}" || return 1
  done
  return 0
}

# The other half of the fence: a port-22 DROP rule this harness did not
# write. Reported, never deleted — removing somebody else's firewall rule
# is the failure this tagging exists to prevent, so the only thing the
# harness does about one is refuse to run. It has to refuse: a box that
# is already partly severed makes every "the route was live, then I cut
# it" claim below a claim about someone else's rule.
#
# Read from `-S` over the whole filter table (custom chains included),
# which prints rules in iptables' own normalized spelling
# (`-p tcp -m tcp --dport 22 …`) rather than the spelling used to install
# them. `|| true` closes each pipeline because *no match* is the normal
# outcome, and grep's exit 1 under the entry scripts' `pipefail` would
# abort preflight in the one case where it should pass.
untagged_port22_drops() {
  local t
  for t in iptables ip6tables; do
    { sudo "$t" -S 2>/dev/null || true; } |
      grep -E -- '-j DROP( |$)' |
      grep -E -- '--(d|s)port 22( |$)' |
      grep -Ev -- "--comment $LIVE_RULE_TAG( |$)" |
      sed "s|^|$t |" || true
  done
  return 0
}

# The positive control. A probe that only ever runs *after* the
# severance can be broken in a way that reads as success: wrong user,
# wrong key, no `true` on the far side. Run it while the route is known
# good and refuse to continue if it fails — a wrong environment is not
# evidence about the app.
probe_live() {
  local what=$1 out rc=0
  out=$(timeout "$PROBE_HARD_TIMEOUT" ssh \
    -o ConnectTimeout="$PROBE_CONNECT_TIMEOUT" "${PROBE_SSH_OPTS[@]}" \
    "$PROBE_TARGET" true 2>&1) || rc=$?
  if [ "$rc" -ne 0 ]; then
    say "$what: $PROBE_TARGET does not answer (rc=$rc: $(tail -1 <<<"$out"))"
    return 1
  fi
  return 0
}

# A probe that merely *fails* proves nothing: a bad key, a wrong user or
# a missing remote command all exit non-zero over a perfectly live
# route. Insist on the failure this lane's severance is supposed to
# produce.
#   timeout  the black hole: packets vanish, so the probe has to burn
#            its ConnectTimeout (ssh exits 255 saying so) or the outer
#            `timeout` has to kill it (124).
#   refused  sshd is down: the kernel answers RST at once. "Connection
#            refused" on stderr is the only accepted evidence.
probe_severed() {
  local want=$1 out rc=0 last
  out=$(timeout "$PROBE_HARD_TIMEOUT" ssh \
    -o ConnectTimeout="$PROBE_CONNECT_TIMEOUT" "${PROBE_SSH_OPTS[@]}" \
    "$PROBE_TARGET" true 2>&1) || rc=$?
  last=$(tail -1 <<<"$out")
  if [ "$rc" -eq 0 ]; then
    say "the route to $PROBE_TARGET is still up after severing it (wanted: $want)"
    return 1
  fi
  case "$want" in
    timeout)
      if [ "$rc" -eq 124 ] || grep -qiE 'timed out|timeout' <<<"$out"; then
        return 0
      fi
      say "wanted a black-holed (timing-out) route, got rc=$rc: $last"
      return 1
      ;;
    refused)
      if grep -qi 'connection refused' <<<"$out"; then
        return 0
      fi
      say "wanted a refused port, got rc=$rc: $last"
      return 1
      ;;
    *) say "probe_severed: unknown expectation '$want'"; return 1 ;;
  esac
}

sshd_stop() {
  # `ssh.socket` and not just `ssh.service`: this box socket-activates
  # sshd, so stopping the service alone leaves the socket unit listening
  # and the very next connection starts it right back up. The port never
  # refuses, the retry succeeds, and the lane reports a pass for a
  # severance that never happened. `mutate.sh M5` is that control.
  sudo systemctl stop ssh.socket || return 1
  sudo systemctl stop ssh 2>/dev/null || sudo systemctl stop sshd 2>/dev/null || return 1
  return 0
}
sshd_start() {
  sudo systemctl start ssh.socket >/dev/null 2>&1 || true
  sudo systemctl start ssh >/dev/null 2>&1 || sudo systemctl start sshd >/dev/null 2>&1 || true
  return 0
}

# ── the app, as a direct child ───────────────────────────────────────
# The harness owns the X server rather than borrowing `xvfb-run`'s, for
# two reasons that are the same reason: the app must be *this* shell's
# child, so its exit status is readable with `wait` (the graceful-exit
# proof), and a signal aimed at the app must not reach the X server
# first. `mutate.sh M2` is the control for the second half.
XVFB=""
APP=""
DISPLAY_NUM=""

# A lock file whose owner is still alive belongs to a running X server —
# which on a developer box is not necessarily an `Xvfb`, and deleting its
# lock is how a harness takes down somebody's desktop. So the only lock
# removed here is one this box can *prove* is litter: the pid it names is
# gone. Anything else — alive, or a lock too mangled to name a pid — is
# left alone and the search moves to the next display.
#
# Liveness is read from `/proc` first: `kill -0` fails with EPERM for a
# process owned by another user, and an X server running as root would
# then read as dead.
pid_alive() {
  local pid=$1
  [ -n "$pid" ] || return 1
  [ -d "/proc/$pid" ] && return 0
  kill -0 "$pid" 2>/dev/null
}

pick_display() {
  local n owner
  for n in $(seq 90 99); do
    if [ ! -e "/tmp/.X$n-lock" ]; then DISPLAY_NUM=$n; return 0; fi
    # An X lock file is the server's pid, space-padded.
    owner=$(tr -dc '0-9' <"/tmp/.X$n-lock" 2>/dev/null || true)
    if [ -z "$owner" ] || pid_alive "$owner"; then
      say "    :$n is taken (lock owner ${owner:-unreadable}); trying the next display"
      continue
    fi
    say "    :$n's lock names a dead pid $owner — an interrupted lane's litter; removing it"
    rm -f "/tmp/.X$n-lock" 2>/dev/null || true
    [ -e "/tmp/.X$n-lock" ] || { DISPLAY_NUM=$n; return 0; }
  done
  return 1
}

start_app() {
  local out=$1
  pick_display || fail "no free X display in :90-:99"
  say "starting Xvfb on :$DISPLAY_NUM"
  Xvfb ":$DISPLAY_NUM" -screen 0 1600x1000x24 >"$ART/$TAG-xvfb.out" 2>&1 &
  XVFB=$!
  # Both halves, and the process itself: a dead Xvfb leaves its socket in
  # `/tmp/.X11-unix` behind, so an old `X$N` there is not evidence that
  # *this* server came up — while the lock, which `pick_display` proved
  # absent, is created by the server that just started. The loop gives up
  # early when the process is gone rather than burning the whole wait.
  local i
  for i in $(seq 1 50); do
    kill -0 "$XVFB" 2>/dev/null || break
    [ -e "/tmp/.X$DISPLAY_NUM-lock" ] && [ -S "/tmp/.X11-unix/X$DISPLAY_NUM" ] && break
    sleep 0.2
  done
  kill -0 "$XVFB" 2>/dev/null ||
    fail "Xvfb died on :$DISPLAY_NUM (see $ART/$TAG-xvfb.out)"
  { [ -e "/tmp/.X$DISPLAY_NUM-lock" ] && [ -S "/tmp/.X11-unix/X$DISPLAY_NUM" ]; } ||
    fail "Xvfb is running but never opened :$DISPLAY_NUM (see $ART/$TAG-xvfb.out)"
  say "starting roost-iced as a direct child (stdout+stderr -> $out)"
  DISPLAY=":$DISPLAY_NUM" "$RT/roost-iced" >"$out" 2>&1 &
  APP=$!
  for i in $(seq 1 60); do
    ctl identify >/dev/null 2>&1 && break
    sleep 1
  done
  ctl identify >/dev/null 2>&1 || fail "the UI never answered on its socket (see $out)"
  say "UI up (pid $APP)"
}

# The saved host this harness drives, created once and reused.
find_host_id() {
  ctl host list 2>/dev/null | awk -v t="$PROBE_TARGET" '$0 ~ t {print $1; exit}' || true
}
ensure_host() {
  HOST_ID=$(find_host_id)
  if [ -z "$HOST_ID" ]; then
    say "saving a host for $HOST_TARGET"
    ctl host add --target "$HOST_TARGET" --label live >/dev/null 2>&1 || true
    HOST_ID=$(find_host_id)
  fi
  [ -n "$HOST_ID" ] || fail "no saved host for $HOST_TARGET"
  say "host $HOST_ID"
}

# Connect and wait for the op to say so. The diagnostic on failure is
# the op's own answer — the reason and detail a user would read — not a
# scrape of the app's output.
connect_and_wait() {
  local i row state
  say "connecting"
  ctl host connect --id "$HOST_ID" >/dev/null 2>&1 || true
  for i in $(seq 1 90); do
    row=$(read_row) || { sleep 1; continue; }
    state=$(row_state "$row")
    [ "$state" = connected ] && { say "connected over real ssh ($(ssh_count) ssh processes)"; return 0; }
    sleep 1
  done
  dump_status
  return 1
}

# The precondition three call sites share: a lane that signals before an
# establish is away is testing an ordinary shutdown, and "nothing leaked"
# is then true by construction. Deliberately silent and always 0 — the
# verdict belongs to the caller's own `expect_ge`, where the claim is
# made, not to the wait.
wait_for_ssh_inflight() {
  local limit=${1:-60}
  for _ in $(seq 1 "$limit"); do
    [ "$(ssh_count)" -ge 1 ] && break
    sleep 1
  done
  return 0
}

# ── the predicates the controls have to be able to break ─────────────

# Nothing advanced for `window` seconds: the same claim L3 and L2 make
# after a ladder is supposed to have stopped. `generation` flat is the
# strong half (a timer that fired and re-armed between two polls shows
# no `retry` at either, but cannot start an attempt without moving it);
# an armed rung is the visible half.
assert_flat() {
  local gen0=$1 window=$2 what=$3 row gen
  say "holding $window s: $what must not advance (generation $gen0)"
  sleep "$window"
  row=$(read_row) || { say "$what: host.status unreadable"; return 1; }
  gen=$(row_generation "$row")
  if [ "$gen" != "$gen0" ]; then
    say "$what advanced anyway: generation $gen0 -> $gen ($row)"
    return 1
  fi
  if row_has_retry "$row"; then
    say "$what armed another rung: $row"
    return 1
  fi
  return 0
}

# The band an armed rung must be showing, derived from the numbers in
# the SAME row it came from — §7 AC1's format agreement, made live. The
# functional lane asserts it against a fake ssh; here the delay, the
# attempt and the budget are a real ladder's, and `rollup` is what the
# sidebar reducer produced from them.
#
# The seconds mirror `host_conn.rs`'s `retry_line`
# (`delay.as_millis().div_ceil(1_000).max(1)`) — rounded UP, floored at
# one second so a jittered first rung reads `1s` and never `0s`. Integer
# arithmetic only: a shell that computed this in floating point could
# agree with a wrong band.
#
# An ssh ladder's rung always carries all three numbers (a localhost
# retry carries only a delay and never reaches these lanes), so a row
# missing one is itself the finding.
assert_armed_band() {
  local row=$1 what=$2 delay att budget secs want got
  delay=$(row_delay_ms "$row")
  att=$(row_attempt "$row")
  budget=$(row_budget "$row")
  if [ -z "$delay" ] || [ -z "$att" ] || [ -z "$budget" ]; then
    say "$what: band mismatch: an armed ssh rung must carry delay_ms, attempt and budget: $row"
    return 1
  fi
  secs=$(((delay + 999) / 1000))
  [ "$secs" -ge 1 ] || secs=1
  want="${DISCONNECTED_BAND}reconnecting in ${secs}s (${att}/${budget})"
  got=$(row_rollup "$row")
  if [ "$got" != "$want" ]; then
    say "$what: band mismatch: the rollup does not agree with the row it came from"
    say "    band: '$got'"
    say "    want: '$want'"
    say "    row:  $row"
    return 1
  fi
  return 0
}

# The graceful-exit proof, replacing a grep of what the app said on its
# way out: the app is this shell's child, so its own exit status is the
# evidence that it ran its quit path, and what it owned is gone.
assert_clean_exit() {
  local rc=$1 what=$2 left scratch ok=0
  left=$(ssh_count)
  scratch=$(scratch_count)
  if [ "$rc" -ne 0 ]; then
    say "$what: the app exited $rc, not 0 — it did not leave through its own quit path"
    ok=1
  fi
  if [ "$left" -ne 0 ]; then
    say "$what: $left ssh children outlived the app"
    tunnel_ssh | sed 's/^/    /'
    ok=1
  fi
  if [ "$scratch" -ne 0 ]; then
    say "$what: $scratch scratch dirs outlived the app"
    scratch_dirs | sed 's/^/    /'
    ok=1
  fi
  return "$ok"
}

# Wait for the app to be reaped and put its status in `APP_RC`. Bounded:
# a UI that never exits is a finding, not a reason to block forever. The
# status goes in a global rather than the return value so "still running"
# stays distinguishable from any exit code the app could produce.
APP_RC=""
wait_for_app_exit() {
  local limit=${1:-60} rc=0
  for _ in $(seq 1 "$limit"); do
    kill -0 "$APP" 2>/dev/null || break
    sleep 1
  done
  if kill -0 "$APP" 2>/dev/null; then
    say "the app is still alive ${limit}s after the signal"
    return 1
  fi
  wait "$APP" || rc=$?
  APP_RC=$rc
  APP=""
  return 0
}

# The drop has to land before "it never reconnected while severed" means
# anything: the link is cut by killing the mux, and a `state` read in the
# same second still sees the connection the severance is about to end —
# which every window below would report as "connected while severed".
#
# A poll that cannot read a row is skipped, deliberately and loudly: the
# UI can be momentarily busy, but "no row" is not a drop — only a row
# that says so is. The count of skipped polls goes in the diagnostic, so
# a lane that timed out because the op never answered is not mistaken for
# one whose app stayed connected.
wait_for_drop() {
  local limit=${1:-40} row state unreadable=0
  say "waiting for the drop to land"
  for _ in $(seq 1 "$limit"); do
    row=$(read_row) || { unreadable=$((unreadable + 1)); sleep 1; continue; }
    state=$(row_state "$row")
    if [ "$state" != connected ]; then
      say "    dropped: state is now $state"
      return 0
    fi
    sleep 1
  done
  say "no drop in ${limit}s ($unreadable of $limit polls could not read a host row)"
  return 1
}

# The mirror of `wait_for_drop`, and the claim L1 and L5 both end on: the
# route came back and the app redialed it *unaided*. The row that carried
# the `connected` is left in `RECONNECT_ROW` — the `LADDER_ROW`/
# `GIVEUP_ROW` convention — so the caller can go on to assert on the band
# that arrived with it rather than reading a second, later row.
RECONNECT_ROW=""
wait_for_reconnect() {
  local limit=$1 row
  say "waiting for the app to come back on its own"
  for _ in $(seq 1 "$limit"); do
    if row=$(read_row) && [ "$(row_state "$row")" = connected ]; then
      RECONNECT_ROW=$row
      return 0
    fi
    sleep 2
  done
  dump_status
  return 1
}

# ── watching the ladder through the op ───────────────────────────────
# Four claims in one poll, because they are about the same window and a
# second read would be a different one:
#   * the ladder advances — `generation` reaches `target`;
#   * it never reconnects while the route is severed — `state` is read
#     at every poll, not sampled once at the end;
#   * `retry.attempt` climbs monotonically — it is 1-based within an
#     outage and only ever goes up;
#   * every armed rung's band says what that same row's numbers say —
#     asserted at every poll that carries a `retry`, not logged.
#
# A poll that cannot read a row is skipped in silence: `read_row` already
# gave the op three chances, and the claims here are about rows that
# exist. A window with no readable row at all ends in the timeout below.
LADDER_MAX_ATTEMPT=0
LADDER_ROW=""
watch_ladder() {
  local target=$1 limit=$2 what=$3
  local row state att last=0
  LADDER_MAX_ATTEMPT=0
  say "watching the ladder for $what: generation must reach $target within ${limit}s"
  for _ in $(seq 1 "$limit"); do
    if row=$(read_row); then
      LADDER_ROW=$row
      state=$(row_state "$row")
      if [ "$state" = connected ]; then
        say "connected while the route was severed — the severance did not bite: $row"
        return 1
      fi
      if row_has_retry "$row"; then
        assert_armed_band "$row" "$what" || return 1
        att=$(row_attempt "$row")
        if [ "$att" -lt "$last" ]; then
          say "retry.attempt went backwards ($last -> $att): $row"
          return 1
        fi
        last=$att
        if [ "$att" -gt "$LADDER_MAX_ATTEMPT" ]; then
          LADDER_MAX_ATTEMPT=$att
          say "    armed rung $(row_rollup "$row")"
        fi
        maybe_capture_band "$att"
      fi
      if [ "$(row_generation "$row")" -ge "$target" ]; then
        say "generation reached $target (highest armed rung seen: $LADDER_MAX_ATTEMPT)"
        return 0
      fi
    fi
    sleep 1
  done
  say "$what: generation never reached $target (last row: $LADDER_ROW)"
  return 1
}

# The settle: no rung armed and the band saying what stopped. Both
# halves matter and neither implies the other — a rung that has just
# fired shows no `retry` either.
GIVEUP_ROW=""
wait_for_the_give_up() {
  local limit=$1 row rollup
  say "waiting for the give-up (up to ${limit}s)"
  for _ in $(seq 1 "$limit"); do
    if row=$(read_row); then
      if [ "$(row_state "$row")" = connected ]; then
        say "connected while the route was severed — the severance did not bite: $row"
        return 1
      fi
      if ! row_has_retry "$row"; then
        rollup=$(row_rollup "$row")
        case "$rollup" in
          "$GAVE_UP_BAND"*) GIVEUP_ROW=$row; return 0 ;;
        esac
      fi
    fi
    sleep 2
  done
  say "no give-up in ${limit}s (last row: $row)"
  return 1
}

# Plan 042 §8 V5: the band a user reads beside the op that claims to
# describe it, captured at an armed rung. An artifact, not a lane claim —
# a missing screenshot never turns a green lane red.
LIVE_CAPTURE="${LIVE_CAPTURE:-0}"
CAPTURED=0
maybe_capture_band() {
  local att=$1
  [ "$LIVE_CAPTURE" = 1 ] || return 0
  [ "$CAPTURED" -eq 0 ] || return 0
  [ "$att" -ge 2 ] || return 0
  say "capturing the band and the op side by side at rung $att"
  ctl host status --json >"$ART/v5-status.json" 2>/dev/null || true
  ctl screenshot --out "$ART/v5-band.png" >/dev/null 2>&1 ||
    say "    (no screenshot: roostctl screenshot did not answer)"
  CAPTURED=1
  return 0
}

# ── teardown ─────────────────────────────────────────────────────────
# Runs on every exit path, including a tripped control, so the next lane
# starts from the same place. Silent on stdout unless it has something
# to say: `PASS` is the last line a green lane prints.
live_cleanup() {
  # Teardown runs from an EXIT trap and must never decide a verdict: a
  # `pkill` with nothing to kill would otherwise abort the trap under
  # `set -e` and hand the shell a non-zero status that has nothing to do
  # with what the lane found. The shell is on its way out anyway.
  set +e
  blackhole_rules_off
  sshd_start
  [ -f "$KNOWN_HOSTS_BAK" ] && mv "$KNOWN_HOSTS_BAK" "$KNOWN_HOSTS"
  [ -n "$APP" ] && kill "$APP" 2>/dev/null
  sleep 1
  [ -n "$APP" ] && kill -9 "$APP" 2>/dev/null
  # A surviving roost-iced keeps the IPC socket bound, and the next
  # lane's `identify` then answers from the stale instance.
  pkill -x roost-iced 2>/dev/null || true
  [ -n "$XVFB" ] && kill "$XVFB" 2>/dev/null
  sleep 1
  reap_ssh_masters
  rm -f "$BOGUS_KEY" "$BOGUS_KEY.pub"
  return 0
}

# Whatever the last (possibly interrupted) run left: a stale DROP would
# sever this run before it starts, a stale mux would answer for a route
# it believes severed, and a stale scratch dir would read as a leak.
#
# "Whatever the last run left" is scoped by the rule tag, so the reap
# cannot reach past this harness. What it finds outside its own tag it
# reports and refuses on instead.
preflight() {
  require_tools
  mkdir -p "$ART" "$XDG_RUNTIME_DIR"
  chmod 700 "$XDG_RUNTIME_DIR"
  say "reaping anything a previous run left behind"
  pkill -x roost-iced 2>/dev/null || true
  sleep 1
  blackhole_rules_off
  reap_ssh_masters
  # After the reap, so this harness's own leftovers are already gone and
  # anything still standing is somebody else's. Before `probe_live`,
  # because an untagged OUTPUT DROP would fail the probe first and the
  # lane would then blame the route instead of naming the rule.
  local foreign
  foreign=$(untagged_port22_drops)
  if [ -n "$foreign" ]; then
    say "port-22 DROP rules this harness did not install (no '$LIVE_RULE_TAG' tag):"
    printf '%s\n' "$foreign" | sed 's/^/    /'
    fail "refusing to start: this box is already partly severed by $(printf '%s\n' "$foreign" | grep -c .) rule(s) the harness does not own — remove them (or tag them) yourself; the harness will not delete a rule it did not write"
  fi
  local left scratch
  left=$(ssh_count)
  scratch=$(scratch_count)
  if [ "$left" -ne 0 ] || [ "$scratch" -ne 0 ]; then
    tunnel_ssh | sed 's/^/    /'
    scratch_dirs | sed 's/^/    /'
    fail "refusing to start: $left ssh processes and $scratch scratch dirs survived the reap"
  fi
  probe_live "positive control" || fail "the route is not usable before the lane even starts"
  # `roost-session` is the far side of the bridge; the ssh exec finds it
  # on the remote's PATH. Starting it here is what makes localhost a
  # realistic remote.
  ctl session start >/dev/null 2>&1 || true
}
