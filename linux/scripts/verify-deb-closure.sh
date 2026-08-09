#!/usr/bin/env bash
# Verify the Roost .deb's dependency closure by installing and launching it in
# a clean container.
#
# smoke-deb.sh extracts the .deb, so it validates the payload (file list, exec
# bits, destinations) but NOT the dependency closure: a build/CI runner already
# carries the whole graphics stack, so a `Depends:` line that forgot a library
# would still launch there. Missing runtime dependencies are the failure mode
# users actually hit — the package installs cleanly and then won't start — so
# the closure gets checked where nothing is pre-installed: a clean ubuntu:24.04
# container with --no-install-recommends, which also proves the
# Recommends-vs-Depends split (no Vulkan loader in there at all).
#
# Prerequisites: docker (the image is pulled on first run).
#
# Usage:
#   ./linux/scripts/verify-deb-closure.sh out/roost_0.0.18_amd64.deb
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=linux/scripts/_common.sh
. "${SCRIPT_DIR}/_common.sh"

USAGE="usage: $(basename "$0") <deb-path>"
usage() { printf '%s\n' "${USAGE}"; }

deb=""
while [ "$#" -gt 0 ]; do
  case "$1" in
    -h|--help)
      usage
      exit 0
      ;;
    -*)
      usage >&2
      die "unknown flag: $1"
      ;;
    *)
      [ -z "${deb}" ] || { usage >&2; die "unexpected extra argument: $1"; }
      deb="$1"
      shift
      ;;
  esac
done

[ -n "${deb}" ] || { usage >&2; die "missing <deb-path>"; }
[ -f "${deb}" ] || die "no such .deb: ${deb}"
require_tools docker
deb="$(abspath "${deb}")"

# Every wait is bounded. An unbounded version of this ran for 31 minutes in a
# shed before it was killed by hand — a release-gating step that can hang is a
# defect, because `release.yml`'s only backstop is the job's 90-minute timeout,
# burned after `create-release` has already published the Release. `timeout`
# also runs xvfb-run in its own process group, which is what stops the
# backgrounded UI from keeping the container's streams open after the inner
# shell is done.
LAUNCH_TIMEOUT="${ROOST_CLOSURE_LAUNCH_TIMEOUT:-120}"
DOCKER_TIMEOUT="${ROOST_CLOSURE_DOCKER_TIMEOUT:-900}"
require_tools timeout

set +e
# shellcheck disable=SC2016  # $LAUNCH_TIMEOUT/$XDG_RUNTIME_DIR are expanded by
# the shell INSIDE the container (the value arrives via `-e`), not out here.
timeout "${DOCKER_TIMEOUT}" docker run --rm \
  -e "LAUNCH_TIMEOUT=${LAUNCH_TIMEOUT}" \
  -v "${deb}:/tmp/roost.deb:ro" ubuntu:24.04 bash -eu -c '
  apt-get update -qq
  # --no-install-recommends is the strict case: only what Depends
  # actually names gets installed.
  apt-get install -y -qq --no-install-recommends /tmp/roost.deb
  # xvfb/xauth are the harness, not the package under test.
  apt-get install -y -qq --no-install-recommends xvfb xauth
  export XDG_RUNTIME_DIR=/tmp/rt
  mkdir -p "$XDG_RUNTIME_DIR"; chmod 700 "$XDG_RUNTIME_DIR"
  timeout "${LAUNCH_TIMEOUT}" xvfb-run -a --server-args="-screen 0 1280x800x24" bash -c "
    /usr/bin/roost >/tmp/app.log 2>&1 &
    ui=\$!
    ok=no
    for _ in \$(seq 1 60); do
      if /usr/bin/roostctl identify >/dev/null 2>&1; then ok=yes; break; fi
      sleep 0.5
    done
    if [ \"\$ok\" != yes ]; then
      echo \"the installed package never answered roostctl identify within 30s\"
      tail -40 /tmp/app.log || true
      kill \"\$ui\" 2>/dev/null || true
      exit 1
    fi
    /usr/bin/roostctl identify
    # Reap the UI rather than leaving it to the container teardown: a live
    # background process still holding the streams of the container is what
    # turns a finished check into a hung one.
    kill \"\$ui\" 2>/dev/null || true
    wait \"\$ui\" 2>/dev/null || true
  " || {
    inner=$?
    # Re-map the inner timeout off 124. Both timeouts exit 124, and the inner
    # one propagates out as the container status, so leaving it would make the
    # launch timeout report itself as the outer docker budget — the same
    # misdiagnosis this script was rewritten to stop making, one layer down.
    [ "$inner" -eq 124 ] && { tail -40 /tmp/app.log || true; exit 125; }
    exit "$inner"
  }
'
rc=$?
set -e

# Do NOT assert a cause. The original inline version reported every failure as
# "its Depends: list is incomplete", which misdiagnosed a hang as a dependency
# bug and would have sent someone hunting a package that was actually fine.
case "${rc}" in
  0) ;;
  124)
    die "the closure check exceeded its overall ${DOCKER_TIMEOUT}s budget before the UI launch was reached (docker pull or apt wedged). This is a harness/environment failure, NOT evidence about the Depends: list."
    ;;
  125)
    die "the installed package did not answer roostctl identify within ${LAUNCH_TIMEOUT}s and the launch was killed. Distinct from the ${DOCKER_TIMEOUT}s budget above: the package DID install, so this points at the app or its runtime dependencies, not at docker or apt."
    ;;
  *)
    die "the .deb installed but did not come up in a clean container (exit ${rc}). The most likely cause is an incomplete Depends: list — the container has nothing preinstalled — but check the log above before concluding that: a docker or apt-mirror failure lands here too."
    ;;
esac

echo "closure ok: ${deb}"
