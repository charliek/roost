#!/usr/bin/env bash
# Reproduce the single-instance lock flake (issue #324):
# `single_instance::tests::drop_releases_so_next_acquire_succeeds` panics with
# `AlreadyHeld(<our own pid>)` in roughly 1-in-10 CI runs, on both
# ubuntu-latest and macos-latest.
#
# flock(2) locks live on the open file description, not on the fd or the
# process. A fork() inherits a duplicate of the lock fd and keeps the lock
# alive until that fd closes at exec (CLOEXEC). Rust's `File` drop calls only
# close(2), never flock(LOCK_UN). So if a sibling test in the SAME test binary
# forks during the window in which this test holds the lock, the drop does not
# release the flock and the next acquire() gets WouldBlock.
#
# "Same test binary" is the load-bearing part, and it is why running
# `cargo test -p roost-engine single_instance` alone never fails: a filtered
# run has no forking siblings, and other crates' test binaries are separate
# processes that never inherited our fd. The forks that matter are the
# subprocess-spawning tests inside roost-engine's own lib test binary
# (`git_metrics`, `process`, ...) — which is what --scope engine loops.
#
# Usage:
#   tools/repro/single-instance-flake.sh                  # 200 engine runs, ~2 min
#   tools/repro/single-instance-flake.sh --scope workspace # what CI runs, ~40s/iter
#
# Options (env var equivalents in parens):
#   -s, --scope engine|workspace  what to loop           (SCOPE, default engine)
#                                 engine    = cargo test -p roost-engine --lib
#                                 workspace = cargo test --workspace (mirrors CI)
#   -n, --iterations N     iterations   (ITERATIONS, default 200 engine / 30 workspace)
#   -j, --test-threads N   --test-threads (TEST_THREADS, default 64 engine / 16 workspace)
#       --load N           N background CPU hogs        (LOAD, default 4)
#       --keep             keep the per-iteration logs even on success
#
# Higher --test-threads and --load both widen the fork->exec window the race
# needs. Exits non-zero if any iteration failed; failures are split into "the
# #324 lock flake" and "unrelated" so an unrelated red (e.g. pty exhaustion at
# a high thread count) can't be mistaken for a reproduction.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
# libtest prints this header only for a test that FAILED, so it can't be
# confused with the same test's name in the passing-test listing.
LOCK_FAILURE_MARKER='^---- single_instance::tests::drop_releases.* stdout ----'

USAGE="usage: $(basename "$0") [-s engine|workspace] [-n <iterations>] [-j <test-threads>] [--load <n>] [--keep]"
usage() { printf '%s\n' "${USAGE}"; }
die() { printf 'error: %s\n' "$*" >&2; exit 1; }

scope="${SCOPE:-engine}"
iterations="${ITERATIONS:-}"
test_threads="${TEST_THREADS:-}"
load="${LOAD:-4}"
keep=0

while [ "$#" -gt 0 ]; do
  case "$1" in
    -h|--help) usage; exit 0 ;;
    -s|--scope)
      [ "$#" -ge 2 ] || { usage >&2; die "$1 requires a value"; }
      scope="$2"; shift 2 ;;
    -n|--iterations)
      [ "$#" -ge 2 ] || { usage >&2; die "$1 requires a value"; }
      iterations="$2"; shift 2 ;;
    -j|--test-threads)
      [ "$#" -ge 2 ] || { usage >&2; die "$1 requires a value"; }
      test_threads="$2"; shift 2 ;;
    --load)
      [ "$#" -ge 2 ] || { usage >&2; die "$1 requires a value"; }
      load="$2"; shift 2 ;;
    --keep) keep=1; shift ;;
    *) usage >&2; die "unknown argument: $1" ;;
  esac
done

case "${scope}" in
  engine)
    cargo_args="test -p roost-engine --lib"
    iterations="${iterations:-200}"
    test_threads="${test_threads:-64}"
    ;;
  workspace)
    # Mirrors CI's rust job exactly (.github/workflows/ci.yml) — roost-linux
    # is excluded there because it needs GTK.
    cargo_args="test --workspace --exclude roost-linux"
    iterations="${iterations:-30}"
    # The whole workspace at a very high thread count exhausts the pty table
    # on macOS, which reds the run for reasons that have nothing to do with
    # the lock.
    test_threads="${test_threads:-16}"
    ;;
  *) usage >&2; die "--scope must be engine or workspace" ;;
esac

for n in "${iterations}" "${test_threads}" "${load}"; do
  case "${n}" in ''|*[!0-9]*) die "iterations/test-threads/load must be numbers" ;; esac
done
[ "${iterations}" -gt 0 ] || die "--iterations must be > 0"
[ "${test_threads}" -gt 0 ] || die "--test-threads must be > 0"

log_dir="$(mktemp -d "${TMPDIR:-/tmp}/roost-lock-flake.XXXXXX")"
load_pids=""
failures=0

cleanup() {
  for pid in ${load_pids}; do
    kill "${pid}" 2>/dev/null || true
  done
  if [ "${keep}" -eq 0 ] && [ "${failures}" -eq 0 ]; then
    rm -rf "${log_dir}"
  fi
}
trap cleanup EXIT

echo "==> repo:         ${REPO_ROOT}"
echo "==> scope:        ${scope} (cargo ${cargo_args})"
echo "==> iterations:   ${iterations}"
echo "==> test-threads: ${test_threads}"
echo "==> cpu load:     ${load}"
echo "==> logs:         ${log_dir}"

# Compile once so the loop measures the race, not rustc.
echo "==> building the test binaries (once)"
# shellcheck disable=SC2086 # cargo_args is a deliberate word list
(cd "${REPO_ROOT}" && cargo ${cargo_args} --no-run) >"${log_dir}/build.log" 2>&1 || {
  cat "${log_dir}/build.log" >&2
  die "test build failed"
}

i=0
while [ "${i}" -lt "${load}" ]; do
  bash -c 'while :; do :; done' &
  load_pids="${load_pids} $!"
  i=$((i + 1))
done

lock_failures=0
other_failures=0
first_lock_failure=""
first_other_failure=""
started="$(date +%s)"

i=1
while [ "${i}" -le "${iterations}" ]; do
  out="${log_dir}/iteration-${i}.log"
  # shellcheck disable=SC2086 # cargo_args is a deliberate word list
  if (cd "${REPO_ROOT}" && cargo ${cargo_args} -- --test-threads="${test_threads}") \
      >"${out}" 2>&1; then
    [ "${keep}" -eq 1 ] || rm -f "${out}"
  else
    failures=$((failures + 1))
    if grep -qE "${LOCK_FAILURE_MARKER}" "${out}"; then
      lock_failures=$((lock_failures + 1))
      [ -n "${first_lock_failure}" ] || first_lock_failure="${out}"
      printf '==> iteration %d/%d: #324 LOCK FLAKE\n' "${i}" "${iterations}"
    else
      other_failures=$((other_failures + 1))
      [ -n "${first_other_failure}" ] || first_other_failure="${out}"
      printf '==> iteration %d/%d: failed (unrelated)\n' "${i}" "${iterations}"
    fi
  fi
  i=$((i + 1))
done

elapsed=$(( $(date +%s) - started ))

if [ -n "${first_lock_failure}" ]; then
  echo
  echo "==> first #324 failure (${first_lock_failure}):"
  # The interesting part is the failures block, not the passing test lines.
  grep -E -A 8 '^(failures:|---- |test result: FAILED)' "${first_lock_failure}" ||
    tail -n 60 "${first_lock_failure}"
fi
if [ -n "${first_other_failure}" ]; then
  echo
  echo "==> first unrelated failure: ${first_other_failure}"
fi

echo
echo "==> ${failures}/${iterations} iterations failed in ${elapsed}s"
echo "==>   ${lock_failures} reproduced the #324 lock flake"
echo "==>   ${other_failures} failed for unrelated reasons"
if [ "${failures}" -gt 0 ]; then
  echo "==> logs kept in ${log_dir}"
  exit 1
fi
echo "==> no failures — try more iterations, a higher --test-threads, or more --load"
