#!/usr/bin/env bash
# Run a cargo test binary N times to prove a race is fixed (or reproduce
# one): plan 047 §1.1 forbids widening a budget or trusting one green
# run as proof, so this is how "0/100 after" gets to mean something.
#
# Usage (env vars, no flags — see the Makefile's `flake-stress` target):
#   TEST='-p roost-ipc --test bootstrap_test' tools/ci/flake-stress.sh
#   TEST='-p roost-engine --features server-vt --test tab_task_test' \
#     FILTER=a_query_flood_against_a_full_writer_never_blocks_the_task \
#     EXACT=1 N=100 tools/ci/flake-stress.sh
#
#   TEST        cargo test selector, e.g. '-p roost-ipc --test bootstrap_test'
#               (features ride inside TEST; there is no separate knob)
#   FILTER      cargo's substring test filter (optional; default: whole binary)
#   EXACT       1 adds --exact to FILTER                        (default 0)
#   N           iterations, 1..1000                             (default 100)
#   SCALE       ROOST_TEST_TIMEOUT_SCALE for the test process    (default 1)
#   HOGS        busy-loop processes competing for CPU, 0..16     (default 0)
#   RUN_TIMEOUT per-iteration wall-clock cap, seconds, >=1       (default 600)
#
# Builds the test binary ONCE (`cargo test $TEST --no-run
# --message-format=json-render-diagnostics`) and runs the resulting
# executable directly on every iteration — no cargo invocation, and so no
# cargo target-dir lock, in the loop, which is what lets two stress runs
# share a target dir.
#
# RUN_TIMEOUT is enforced by backgrounding the binary and killing its
# process group if it outruns the cap: macOS has no GNU `timeout(1)`, and
# a hang counts as a failure exactly like a nonzero exit, logged the same
# way. HOGS busy loops run in their own process group too, so Ctrl-C
# during a long run can't leave them behind.
#
# Always writes target/flake-stress/summary.txt (commit, OS/kernel, arch,
# rustc, the exact selector, per-iteration durations, pass/fail/timeout
# tally) and target/flake-stress/run-<n>.log for every failed or
# timed-out iteration. Exits non-zero if any iteration failed or timed out.
#
# OUT_DIR is the fixed path target/flake-stress, and this script `rm -rf`s
# it on every start: two *concurrent* runs on the same checkout will
# clobber each other's logs and summary (sharing the target dir's build
# cache is fine; sharing OUT_DIR is not). Copy out a run's summary.txt
# before starting the next one if you need to keep it.
#
# A TEST/FILTER selection that runs zero tests (a typo in FILTER, a
# filter that only matches an #[ignore]d test, or a FILTER value libtest
# parses as its own flag, e.g. --list) is a fatal misconfiguration, not a
# counted result — this tool exists to make "0/N failed" mean something,
# and "0/N because nothing ran" must not look the same. Iteration 1's log
# is checked for libtest's "running N tests" line and the whole run
# aborts non-zero if N is 0 (or the line is missing), instead of
# reporting N counted passes that proved nothing.
#
# `kill -9` of this script itself cannot run its traps, so HOGS
# processes already started will survive it — there is no script-level
# guard against that. Ctrl-C (INT) and a plain `kill` (TERM) both clean
# up hogs and the in-flight test process group correctly.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "${REPO_ROOT}"

die() { printf 'error: %s\n' "$*" >&2; exit 1; }
require_int() { case "$2" in ''|*[!0-9]*) die "$1 must be an integer (got '$2')" ;; esac; }

TEST="${TEST:-}"
[ -n "${TEST}" ] || die "TEST is required, e.g. TEST='-p roost-ipc --test bootstrap_test'"
FILTER="${FILTER:-}"
EXACT="${EXACT:-0}"
N="${N:-100}"
SCALE="${SCALE:-1}"
HOGS="${HOGS:-0}"
RUN_TIMEOUT="${RUN_TIMEOUT:-600}"

require_int N "${N}"
# Force base 10: a leading zero (e.g. N=010) would otherwise be read as
# octal by bash arithmetic — 010 * 5 is 40, not 50 — and an invalid
# octal digit (N=08) throws outright.
N=$((10#${N}))
[ "${N}" -ge 1 ] && [ "${N}" -le 1000 ] || die "N must be in 1..=1000 (got ${N})"

if ! [[ "${SCALE}" =~ ^[0-9]+(\.[0-9]+)?$ ]]; then
  die "SCALE must be a positive number with at most one decimal point (got '${SCALE}')"
fi
awk -v s="${SCALE}" 'BEGIN { exit !(s > 0) }' || die "SCALE must be a positive number (got ${SCALE})"

require_int HOGS "${HOGS}"
HOGS=$((10#${HOGS}))
[ "${HOGS}" -ge 0 ] && [ "${HOGS}" -le 16 ] || die "HOGS must be in 0..=16 (got ${HOGS})"

require_int RUN_TIMEOUT "${RUN_TIMEOUT}"
RUN_TIMEOUT=$((10#${RUN_TIMEOUT}))
[ "${RUN_TIMEOUT}" -ge 1 ] || die "RUN_TIMEOUT must be >= 1 (got ${RUN_TIMEOUT})"

case "${EXACT}" in
  0|1) ;;
  *) die "EXACT must be 0 or 1 (got '${EXACT}')" ;;
esac

OUT_DIR="target/flake-stress"
rm -rf "${OUT_DIR}"
mkdir -p "${OUT_DIR}"

# --- build once -----------------------------------------------------------

echo "==> building test binary (once): cargo test ${TEST} --no-run" >&2
BUILD_JSON="${OUT_DIR}/build.json"
build_status=0
# TEST is deliberately word-split below (not quoted); set -f stops that
# split from also globbing, e.g. TEST='-p roost-ipc --test *' expanding
# against the repo's files.
set -f
# shellcheck disable=SC2086 # TEST is a deliberate cargo argument word list
cargo test ${TEST} --no-run --message-format=json-render-diagnostics \
  | tee "${BUILD_JSON}" >/dev/null || build_status=$?
set +f

if [ "${build_status}" -ne 0 ]; then
  echo "error: build failed (cargo test ${TEST} --no-run), exit ${build_status}" >&2
  jq -r 'select(.reason=="compiler-message") | .message.rendered' "${BUILD_JSON}" >&2
  die "see ${BUILD_JSON} for the full build log"
fi

EXECS=()
while IFS= read -r exe; do
  [ -n "${exe}" ] && EXECS+=("${exe}")
done < <(jq -r 'select(.reason=="compiler-artifact" and .profile.test==true and .executable!=null) | .executable' "${BUILD_JSON}" | sort -u)

if [ "${#EXECS[@]}" -ne 1 ]; then
  echo "error: expected exactly one test binary from TEST='${TEST}', found ${#EXECS[@]}:" >&2
  printf '  %s\n' "${EXECS[@]:-(none)}" >&2
  die "narrow TEST with --test <name> (and -p <crate>) to select a single binary"
fi
BIN="${EXECS[0]}"
echo "==> test binary: ${BIN}" >&2

RUN_ARGS=()
[ -z "${FILTER}" ] || RUN_ARGS+=("${FILTER}")
[ "${EXACT}" != "1" ] || RUN_ARGS+=("--exact")

# --- hogs -------------------------------------------------------------

HOG_PGIDS=()
# The pgid of the iteration currently in flight (set -m makes it equal
# the leader's pid); empty whenever no test process is running. Global
# so on_signal can reach it — it's the only way a signal handler can
# find "the thing run_one is blocked on" without a subshell round trip.
CURRENT_PID=""

start_hogs() {
  local i=0
  while [ "${i}" -lt "${HOGS}" ]; do
    set -m
    sh -c 'while :; do :; done' &
    HOG_PGIDS+=("$!")
    set +m
    i=$((i + 1))
  done
  [ "${HOGS}" -eq 0 ] || echo "==> ${HOGS} CPU hog(s) started: ${HOG_PGIDS[*]}" >&2
}

# Idempotent: safe to call twice (on_signal, then the EXIT trap that
# always runs after it) because it clears HOG_PGIDS once the group is
# gone, so a second call has nothing left to (mis)signal.
stop_hogs() {
  local pgid
  for pgid in "${HOG_PGIDS[@]:-}"; do
    [ -n "${pgid}" ] || continue
    kill -TERM -- "-${pgid}" 2>/dev/null || true
  done
  sleep 0.2
  for pgid in "${HOG_PGIDS[@]:-}"; do
    [ -n "${pgid}" ] || continue
    kill -KILL -- "-${pgid}" 2>/dev/null || true
    wait "${pgid}" 2>/dev/null || true
  done
  HOG_PGIDS=()
}

# Shared by the EXIT trap and INT/TERM: also reaches into whatever
# iteration is currently running, not just the hogs, so Ctrl-C during a
# long stress run can't leave the test process (or its fake-ssh/
# fake-session children, same process group) behind.
cleanup_all() {
  if [ -n "${CURRENT_PID}" ]; then
    kill -TERM -- "-${CURRENT_PID}" 2>/dev/null || kill -TERM "${CURRENT_PID}" 2>/dev/null || true
    CURRENT_PID=""
  fi
  stop_hogs
}

on_exit() { cleanup_all; }
on_signal() { cleanup_all; exit 130; }
trap on_exit EXIT
trap on_signal INT TERM

start_hogs

# --- run loop -----------------------------------------------------------

# Runs $BIN in its own process group and enforces RUN_TIMEOUT by killing
# that group; libtest's own child processes (the fake-ssh/fake-session
# scripts these binaries spawn) live in the same group, so a killed hang
# leaves nothing behind.
#
# Sets the global TIMED_OUT (not a magic exit code — libtest can itself
# exit 124, and overloading that as "we killed it" would misclassify a
# real failure as a timeout) and returns the subprocess's own exit
# status on a normal finish, or an arbitrary nonzero on a timeout (the
# caller checks TIMED_OUT first and ignores the return value then).
run_one() {
  local log="$1"
  local waited=0 rc=0
  # Poll every fifth of a second so a fast test doesn't pay a full
  # second of overhead per iteration; RUN_TIMEOUT stays whole seconds
  # as the documented unit, so the limit is counted in the same units.
  local limit=$((RUN_TIMEOUT * 5))
  TIMED_OUT=0
  set -m
  "${BIN}" "${RUN_ARGS[@]}" >"${log}" 2>&1 &
  CURRENT_PID=$!
  set +m
  while kill -0 "${CURRENT_PID}" 2>/dev/null; do
    if [ "${waited}" -ge "${limit}" ]; then
      TIMED_OUT=1
      kill -TERM -- "-${CURRENT_PID}" 2>/dev/null || kill -TERM "${CURRENT_PID}" 2>/dev/null || true
      sleep 1
      # Kept as a belt-and-suspenders fallback in case `set -m` somehow
      # didn't give the child its own group (job control disabled in
      # the environment); redundant with the group kill above otherwise.
      kill -KILL -- "-${CURRENT_PID}" 2>/dev/null || kill -KILL "${CURRENT_PID}" 2>/dev/null || true
      wait "${CURRENT_PID}" 2>/dev/null || true
      CURRENT_PID=""
      return 1
    fi
    sleep 0.2
    waited=$((waited + 1))
  done
  wait "${CURRENT_PID}" || rc=$?
  # The leader exited, but a descendant (e.g. a fake-ssh child) can
  # still be alive in the same group; sweep it so it doesn't compete
  # with the next iteration. Best-effort — the group is very likely
  # already empty.
  kill -TERM -- "-${CURRENT_PID}" 2>/dev/null || true
  CURRENT_PID=""
  return "${rc}"
}

passed=0
failed=0
timed_out=0
durations=()

echo "==> running ${N} iteration(s), SCALE=${SCALE} HOGS=${HOGS} RUN_TIMEOUT=${RUN_TIMEOUT}s" >&2
i=1
while [ "${i}" -le "${N}" ]; do
  iter_log="${OUT_DIR}/.iter.log"
  : > "${iter_log}"
  start=${SECONDS}
  status=0
  ROOST_TEST_TIMEOUT_SCALE="${SCALE}" run_one "${iter_log}" || status=$?
  elapsed=$((SECONDS - start))

  # A wrong TEST/FILTER (a typo, a filter that only matches an
  # #[ignore]d test, or a FILTER value libtest parses as its own flag
  # like --list) can make libtest run zero tests and still exit 0 — that
  # is the operator's selector being wrong, not a flake, so it aborts
  # the whole run instead of counting as a pass. Checked once, on the
  # first iteration, so a bad selector doesn't waste N-1 more. Skipped
  # on a timeout: a killed process's stdio buffer may never have been
  # flushed before the SIGTERM/SIGKILL (a slow-starting binary — e.g. a
  # first launch eating a Gatekeeper scan — can lose its "running N
  # tests" line entirely), so an absent line there means "we don't know
  # yet", not "zero tests selected".
  if [ "${i}" -eq 1 ] && [ "${TIMED_OUT}" -ne 1 ]; then
    ran_n=$(grep -oE '^running [0-9]+ tests?' "${iter_log}" 2>/dev/null | head -1 | grep -oE '[0-9]+' || true)
    if [ -z "${ran_n}" ] || [ "${ran_n}" -eq 0 ]; then
      saved="${OUT_DIR}/run-1-zero-tests.log"
      cp "${iter_log}" "${saved}"
      die "TEST/FILTER selected zero tests on iteration 1 (see ${saved}) — check FILTER for a typo, a filter matching only an #[ignore]d test, or a value libtest parsed as a flag; refusing to report ${N} passes that proved nothing"
    fi
  fi

  if [ "${TIMED_OUT}" -eq 1 ]; then
    timed_out=$((timed_out + 1))
    durations+=("${i}:${elapsed}s:timeout")
    mv "${iter_log}" "${OUT_DIR}/run-${i}.log"
    echo "==> iteration ${i}/${N}: TIMEOUT after ${elapsed}s" >&2
  elif [ "${status}" -eq 0 ]; then
    passed=$((passed + 1))
    durations+=("${i}:${elapsed}s:${status}")
  else
    failed=$((failed + 1))
    durations+=("${i}:${elapsed}s:${status}")
    mv "${iter_log}" "${OUT_DIR}/run-${i}.log"
    echo "==> iteration ${i}/${N}: FAILED (exit ${status}) after ${elapsed}s" >&2
  fi
  i=$((i + 1))
done
rm -f "${OUT_DIR}/.iter.log"

# --- summary --------------------------------------------------------------

filter_display="${FILTER:-<none>}"
[ "${EXACT}" != "1" ] || filter_display="${filter_display} (exact)"

{
  echo "flake-stress summary"
  echo "commit:      $(git rev-parse HEAD 2>/dev/null || echo unknown)"
  echo "os/kernel:   $(uname -s) $(uname -r)"
  echo "arch:        $(uname -m)"
  echo "rustc:       $(rustc -V)"
  echo "test:        ${TEST}"
  echo "filter:      ${filter_display}"
  echo "N:           ${N}"
  echo "scale:       ${SCALE}"
  echo "hogs:        ${HOGS}"
  echo "run_timeout: ${RUN_TIMEOUT}s"
  echo
  echo "per-iteration durations (iter:seconds:exit_status):"
  printf '  %s\n' "${durations[@]}"
  echo
  echo "passed:      ${passed}"
  echo "failed:      ${failed}"
  echo "timed_out:   ${timed_out}"
} > "${OUT_DIR}/summary.txt"

cat "${OUT_DIR}/summary.txt" >&2
echo "==> summary: ${OUT_DIR}/summary.txt" >&2

[ "$((failed + timed_out))" -eq 0 ]
