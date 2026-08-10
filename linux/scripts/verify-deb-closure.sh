#!/usr/bin/env bash
# Verify the Roost .deb's dependency closure by installing and launching it in
# a clean container, under BOTH display servers.
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
# Why the display server is now a parameter, and why the compositor runs in a
# SEPARATE container (issue #325):
#
#   * The old check was X11-only, and wgpu's GLES backend reaches
#     `wl_egl_window_create` only on the Wayland EGL platform — the X11 EGL
#     platform never needs libwayland-egl. So an X11-only check is structurally
#     incapable of noticing a missing Wayland dependency, which is exactly how
#     libwayland-egl1 went missing from Depends:.
#   * Ubuntu's `weston` depends on libwayland-egl1, and `xvfb`/`xauth` drag in
#     X libraries. Installing either beside the package under test satisfies the
#     very dependency being tested, and the negative control would pass on a
#     broken package. So the compositor lives in its own container and only its
#     socket is shared. The package container installs NOTHING but the .deb.
#
# Why the ldconfig assertions, and not just "did it launch" (#325, measured):
#
#   In a --no-install-recommends container there is no Vulkan ICD, so wgpu
#   enumerates zero adapters and iced falls back to its tiny-skia CPU
#   compositor — which needs neither libEGL nor libwayland-egl. The app
#   therefore STARTS AND RENDERS with those dependencies missing, and a launch-
#   only check stays green on this bug forever. The crash (SIGABRT inside
#   `Surface::configure`) only appears once libEGL is reachable, which on a real
#   desktop it always is and in this container it is not. The resolvability
#   assertions below are the load-bearing part of the Wayland leg; the launch is
#   the backstop, not the detector.
#
# Prerequisites: docker (images are pulled on first run).
#
# Usage:
#   ./linux/scripts/verify-deb-closure.sh out/roost_0.0.18_amd64.deb
#   ./linux/scripts/verify-deb-closure.sh --display wayland out/roost.deb
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=linux/scripts/_common.sh
. "${SCRIPT_DIR}/_common.sh"

USAGE="usage: $(basename "$0") [--display x11|wayland|both] <deb-path>"
usage() { printf '%s\n' "${USAGE}"; }

deb=""
display="both"
while [ "$#" -gt 0 ]; do
  case "$1" in
    -h|--help)
      usage
      exit 0
      ;;
    --display)
      [ "$#" -ge 2 ] || { usage >&2; die "--display requires a value"; }
      display="$2"
      shift 2
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

case "${display}" in
  x11|wayland|both) ;;
  *) usage >&2; die "--display must be x11, wayland or both" ;;
esac

[ -n "${deb}" ] || { usage >&2; die "missing <deb-path>"; }
[ -f "${deb}" ] || die "no such .deb: ${deb}"
require_tools docker timeout
deb="$(abspath "${deb}")"

# Every wait is bounded. An unbounded version of this ran for 31 minutes in a
# shed before it was killed by hand — a release-gating step that can hang is a
# defect, because `release.yml`'s only backstop is the job's 90-minute timeout,
# burned after `create-release` has already published the Release.
LAUNCH_TIMEOUT="${ROOST_CLOSURE_LAUNCH_TIMEOUT:-120}"
DOCKER_TIMEOUT="${ROOST_CLOSURE_DOCKER_TIMEOUT:-900}"
COMPOSITOR_TIMEOUT="${ROOST_CLOSURE_COMPOSITOR_TIMEOUT:-300}"
IMAGE="${ROOST_CLOSURE_IMAGE:-ubuntu:24.04}"

# How long the app must still be alive and answering AFTER its first successful
# identify. `App::bootstrap` binds the IPC socket before iced creates the
# window, so a renderer or EGL failure that kills the process a moment later can
# still answer one identify inside the poll window. Without this second look the
# check cannot tell "launched" from "bound the socket and then died".
LIVENESS_SECONDS="${ROOST_CLOSURE_LIVENESS_SECONDS:-8}"

run_id="roost-closure-$$"
compositor_cid=""
share_dir=""

cleanup() {
  if [ -n "${compositor_cid}" ]; then
    docker rm -f "${compositor_cid}" >/dev/null 2>&1 || true
    compositor_cid=""
  fi
  if [ -n "${share_dir}" ] && [ -d "${share_dir}" ]; then
    # The compositor container runs as root, so its socket is root-owned and a
    # plain rm leaves the directory behind on any non-root host. Borrow root
    # from a throwaway container to empty it; best-effort either way.
    rm -rf "${share_dir}" 2>/dev/null || {
      docker run --rm -v "${share_dir}:/share" "${IMAGE}" \
        find /share -mindepth 1 -delete >/dev/null 2>&1 || true
      rmdir "${share_dir}" 2>/dev/null || true
    }
    share_dir=""
  fi
}
trap cleanup EXIT

# Start the display server in its own container and wait for its socket to show
# up in the shared directory. `$1` is the leg name; sets `compositor_cid`.
start_compositor() {
  local leg="$1" name sock
  name="${run_id}-${leg}-compositor"

  case "${leg}" in
    wayland)
      # weston's headless backend needs no GPU and no seat. `--idle-time=0` so
      # it never suspends the output under a long apt-get in the other
      # container.
      compositor_cid="$(timeout "${COMPOSITOR_TIMEOUT}" docker run -d --rm --name "${name}" \
        -v "${share_dir}:/share" \
        -e XDG_RUNTIME_DIR=/share \
        "${IMAGE}" bash -eu -c '
          apt-get update -qq
          apt-get install -y -qq --no-install-recommends weston
          chmod 700 /share
          exec weston --backend=headless-backend.so --socket=wayland-closure \
            --width=1280 --height=800 --idle-time=0
        ')"
      sock="${share_dir}/wayland-closure"
      ;;
    x11)
      # `-ac` disables access control so the package container needs no xauth —
      # xauth is an X client library dependency we must not install beside the
      # package under test.
      compositor_cid="$(timeout "${COMPOSITOR_TIMEOUT}" docker run -d --rm --name "${name}" \
        -v "${share_dir}:/tmp/.X11-unix" \
        "${IMAGE}" bash -eu -c '
          apt-get update -qq
          apt-get install -y -qq --no-install-recommends xvfb
          exec Xvfb :99 -screen 0 1280x800x24 -ac -nolisten tcp
        ')"
      sock="${share_dir}/X99"
      ;;
  esac

  # `docker run -d` returns as soon as the container is created, but an image
  # pull happens first and is unbounded without this — the readiness loop below
  # would never even start, leaving the CI job timeout as the only backstop.
  [ -n "${compositor_cid}" ] || die "could not start the ${leg} compositor container within ${COMPOSITOR_TIMEOUT}s"

  local waited=0
  while [ "${waited}" -lt "${COMPOSITOR_TIMEOUT}" ]; do
    [ -S "${sock}" ] && return 0
    if ! docker inspect -f '{{.State.Running}}' "${compositor_cid}" 2>/dev/null | grep -q true; then
      echo "--- ${leg} compositor container log ---" >&2
      docker logs "${compositor_cid}" 2>&1 | tail -40 >&2 || true
      die "the ${leg} compositor container exited before its socket appeared"
    fi
    sleep 1
    waited=$((waited + 1))
  done
  echo "--- ${leg} compositor container log ---" >&2
  docker logs "${compositor_cid}" 2>&1 | tail -40 >&2 || true
  die "the ${leg} compositor socket never appeared within ${COMPOSITOR_TIMEOUT}s"
}

# The body that runs inside the package container. Identical for both legs; the
# display environment is the only difference and arrives via `-e`.
#
# Split into two phases on purpose. `LAUNCH_TIMEOUT` must bound the LAUNCH and
# nothing else: when it also covered `apt-get`, a slow mirror timed out and got
# reported as "the package installed but did not come up", which is a
# diagnosis about the Depends: list drawn from an apt failure.
#
# shellcheck disable=SC2016  # every $VAR here is expanded INSIDE the container.
INSTALL_SCRIPT='
  apt-get update -qq
  # --no-install-recommends is the strict case: only what Depends actually
  # names gets installed.
  apt-get install -y -qq --no-install-recommends /tmp/roost.deb

  # Snapshot immediately after the package under test and BEFORE anything else,
  # so "the harness supplied it" is a provable claim rather than an assumption.
  # Nothing else is installed in this container at all — the display server runs
  # in a separate one — so this snapshot IS the final state.
  dpkg-query -W -f "\${binary:Package}\n" | sort > /tmp/pkgs-after-deb.txt
  echo "packages present after installing the deb: $(wc -l < /tmp/pkgs-after-deb.txt)"

  # Every soname the binary dlopens must be resolvable from the .deb'"'"'s own
  # closure. Nothing but the .deb is installed in this container, so a miss here
  # is a Depends: gap with no other candidate.
  #
  # libwayland-egl.so.1 is asserted on the WAYLAND leg only, because that is the
  # truth: the X11 EGL platform never calls wl_egl_window_create. Stripping
  # libwayland-egl1 from Depends: therefore reds the Wayland leg and leaves the
  # X11 leg green — which is the whole point, and precisely what the previous
  # X11-only check could not express.
  sonames="libxkbcommon.so.0 libxkbcommon-x11.so.0 libwayland-client.so.0
           libX11.so.6 libX11-xcb.so.1 libxcb.so.1 libXcursor.so.1 libXi.so.6
           libEGL.so.1"
  if [ "${ROOST_LEG}" = wayland ]; then
    sonames="${sonames} libwayland-egl.so.1"
  fi

  # Exact first-field match, not `grep`: in `ldconfig -p` output the soname is
  # the first field, and an unanchored grep with unescaped dots would let
  # libEGL.so.10 satisfy a libEGL.so.1 check.
  missing=""
  for soname in ${sonames}; do
    ldconfig -p | awk -v s="${soname}" '"'"'$1 == s { hit = 1 } END { exit !hit }'"'"' \
      || missing="${missing} ${soname}"
  done
  if [ -n "${missing}" ]; then
    echo "::MISSING::${missing}"
    exit 3
  fi
'

# shellcheck disable=SC2016  # every $VAR here is expanded INSIDE the container.
LAUNCH_SCRIPT='
  /usr/bin/roost >/tmp/app.log 2>&1 &
  ui=$!

  ok=no
  for _ in $(seq 1 60); do
    if /usr/bin/roostctl identify >/dev/null 2>&1; then ok=yes; break; fi
    # If the process is already gone there is nothing left to wait for.
    if ! kill -0 "$ui" 2>/dev/null; then break; fi
    sleep 0.5
  done
  if [ "$ok" != yes ]; then
    echo "the installed package never answered roostctl identify within 30s"
    tail -60 /tmp/app.log || true
    kill "$ui" 2>/dev/null || true
    exit 1
  fi
  /usr/bin/roostctl identify

  # Liveness. The socket is bound before the window exists, so one successful
  # identify only proves bootstrap got that far.
  sleep "${LIVENESS_SECONDS}"
  if ! kill -0 "$ui" 2>/dev/null; then
    # `set -e` would abort on a non-zero `wait`, taking the diagnostic with it.
    status=0
    wait "$ui" 2>/dev/null || status=$?
    echo "the UI answered identify and then exited (status ${status}) — it bound the socket but did not stay up"
    tail -60 /tmp/app.log || true
    exit 4
  fi
  if ! /usr/bin/roostctl identify >/dev/null 2>&1; then
    echo "the UI is still running but stopped answering identify after ${LIVENESS_SECONDS}s"
    tail -60 /tmp/app.log || true
    kill "$ui" 2>/dev/null || true
    exit 4
  fi

  # Reap the UI rather than leaving it to the container teardown: a live
  # background process still holding the container'"'"'s streams is what turns a
  # finished check into a hung one. SIGTERM is expected, so 143 is the success
  # status here; anything else means it was already dying.
  kill "$ui" 2>/dev/null || true
  status=0
  wait "$ui" 2>/dev/null || status=$?
  case "${status}" in
    0|143) ;;
    *)
      echo "the UI exited with an unexpected status (${status}) on shutdown"
      tail -60 /tmp/app.log || true
      exit 4
      ;;
  esac

  echo "leg ok: ${ROOST_LEG}"
'

run_leg() {
  local leg="$1"
  echo "==> closure leg: ${leg}"

  share_dir="$(mktemp -d)"
  chmod 700 "${share_dir}"
  start_compositor "${leg}"

  local -a display_env
  case "${leg}" in
    wayland)
      # No DISPLAY at all, so winit cannot silently fall back to X11 and quietly
      # turn this into a second X11 leg.
      display_env=(
        -v "${share_dir}:/share"
        -e XDG_RUNTIME_DIR=/share
        -e WAYLAND_DISPLAY=wayland-closure
        -e WINIT_UNIX_BACKEND=wayland
      )
      ;;
    x11)
      display_env=(
        -v "${share_dir}:/tmp/.X11-unix"
        -e DISPLAY=:99
        -e WINIT_UNIX_BACKEND=x11
      )
      ;;
  esac

  set +e
  timeout "${DOCKER_TIMEOUT}" docker run --rm \
    --name "${run_id}-${leg}-package" \
    "${display_env[@]}" \
    -e "ROOST_LEG=${leg}" \
    -e "LIVENESS_SECONDS=${LIVENESS_SECONDS}" \
    -v "${deb}:/tmp/roost.deb:ro" \
    "${IMAGE}" bash -eu -c "
      export XDG_RUNTIME_DIR=\${XDG_RUNTIME_DIR:-/tmp/rt}
      mkdir -p \"\$XDG_RUNTIME_DIR\"; chmod 700 \"\$XDG_RUNTIME_DIR\"
      # The two scripts arrive as \$1 and \$2, separate argv words. Interpolating
      # them into a quoted command string instead lets any apostrophe in their
      # own comments close that quote and spill the remainder into this shell.
      bash -eu -c \"\$1\" install-phase
      timeout ${LAUNCH_TIMEOUT} bash -eu -c \"\$2\" launch-phase || {
        inner=\$?
        # Re-map the inner timeout off 124: both timeouts exit 124 and the inner
        # status propagates out as the container status, so leaving it would
        # make the launch timeout report itself as the outer docker budget.
        # 122 rather than 125 because docker reserves 125/126/127 for its own
        # failures, and reusing one would let a docker daemon error masquerade
        # as an app-launch timeout.
        [ \"\$inner\" -eq 124 ] && exit 122
        exit \"\$inner\"
      }
    " _ "${INSTALL_SCRIPT}" "${LAUNCH_SCRIPT}"
  local rc=$?
  set -e

  cleanup

  # Do NOT assert a cause for the generic failures. The original inline version
  # reported every failure as "its Depends: list is incomplete", which
  # misdiagnosed a hang as a dependency bug.
  case "${rc}" in
    0) ;;
    3)
      die "[${leg}] a library the app dlopens is not present after installing the .deb with --no-install-recommends. Nothing but the .deb was installed in that container — the display server runs in its own — so this is a Depends: gap, not harness contamination."
      ;;
    4)
      die "[${leg}] the package installed and answered roostctl identify, then failed the liveness re-check. The IPC socket is bound before the window exists, so this is the shape a renderer/EGL failure takes: bootstrap succeeds, the window does not."
      ;;
    122)
      die "[${leg}] the installed package did not come up within ${LAUNCH_TIMEOUT}s and the launch was killed. Distinct from the ${DOCKER_TIMEOUT}s budget below: the package DID install and its dlopen closure DID resolve, so this points at the app, not at docker or apt."
      ;;
    124)
      die "[${leg}] the closure check exceeded its overall ${DOCKER_TIMEOUT}s budget before the UI launch was reached (docker pull or apt wedged). This is a harness/environment failure, NOT evidence about the Depends: list."
      ;;
    125|126|127)
      die "[${leg}] docker itself failed (exit ${rc}: container could not be created, the command could not be invoked, or it was not found). This is a harness/environment failure, NOT evidence about the Depends: list."
      ;;
    *)
      die "[${leg}] the .deb installed but did not come up in a clean container (exit ${rc}). The most likely cause is an incomplete Depends: list — the container has nothing preinstalled — but check the log above before concluding that: a docker or apt-mirror failure lands here too."
      ;;
  esac
}

case "${display}" in
  both)
    run_leg x11
    run_leg wayland
    ;;
  *)
    run_leg "${display}"
    ;;
esac

echo "closure ok: ${deb} (${display})"
