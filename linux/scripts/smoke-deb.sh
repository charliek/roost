#!/usr/bin/env bash
# Smoke the PAYLOAD of an already-built Roost .deb: check the metadata and the
# staged contents, then launch the real packaged binary and prove it binds the
# production `roost` IPC namespace.
#
# This is the only check that actually launches the artifact being shipped.
# Nothing upstream (cargo build, nfpm pkg) can catch "wrong binary staged" or
# "linux-package silently didn't compile in" — both fail *only* by the running
# binary binding the wrong IPC namespace, which is exactly what this checks.
#
# Smoke what nfpm actually produced, not what build-deb.sh staged: running
# ./dist/roost would prove the binary works while leaving every packaging-layer
# mistake — a wrong `contents:` destination, a dropped entry, a lost exec bit —
# to be discovered by users.
#
# What it does NOT check is the dependency closure — see verify-deb-closure.sh.
#
# Prerequisites: dpkg-deb, dpkg, xvfb-run (Ubuntu: dpkg-dev is already there;
# `apt-get install -y xvfb`).
#
# Usage:
#   ./linux/scripts/smoke-deb.sh out/roost_0.0.18_amd64.deb
#   ./linux/scripts/smoke-deb.sh "$deb" --work-dir /tmp/roost-smoke --expect-version 0.0.18~rc1
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=linux/scripts/_common.sh
. "${SCRIPT_DIR}/_common.sh"

USAGE="usage: $(basename "$0") <deb-path> [--work-dir <dir>] [--expect-version <v>]"
usage() { printf '%s\n' "${USAGE}"; }

deb=""
work_dir=""
expect_version=""
while [ "$#" -gt 0 ]; do
  case "$1" in
    -h|--help)
      usage
      exit 0
      ;;
    --work-dir)
      [ "$#" -ge 2 ] || { usage >&2; die "--work-dir requires a value"; }
      work_dir="$2"
      shift 2
      ;;
    --expect-version)
      [ "$#" -ge 2 ] || { usage >&2; die "--expect-version requires a value"; }
      expect_version="$2"
      shift 2
      ;;
    -*)
      usage >&2
      die "unknown flag: $1"
      ;;
    *)
      # A bare second positional would let a glob that expanded to two files
      # be read as `<deb> <work-dir>` and silently smoke the wrong one.
      [ -z "${deb}" ] || { usage >&2; die "unexpected extra argument: $1"; }
      deb="$1"
      shift
      ;;
  esac
done

[ -n "${deb}" ] || { usage >&2; die "missing <deb-path>"; }
[ -f "${deb}" ] || die "no such .deb: ${deb}"
require_tools dpkg-deb dpkg xvfb-run
deb="$(abspath "${deb}")"

# ---------------------------------------------------------------- work dir
work=""
created_work=0
if [ -n "${work_dir}" ]; then
  mkdir -p "${work_dir}"
  work="$(cd "${work_dir}" && pwd)"
else
  work="$(mktemp -d "${TMPDIR:-/tmp}/roost-smoke.XXXXXX")"
  created_work=1
fi

APP=""
cleanup() {
  if [ -n "${APP}" ]; then
    # TERM the whole process group, not just $APP: $APP is the xvfb-run
    # wrapper, and reaping it does not necessarily reap the Xvfb + roost
    # children it started.
    kill -TERM -- "-${APP}" 2>/dev/null || true
    # Poll for a clean exit on a deadline rather than a bare `wait`. An
    # unbounded wait here would block the EXIT trap forever if anything in
    # the group ignored SIGTERM — the same hang this script's sibling was
    # rewritten to prevent, and on the same release-gating path. The KILL
    # sweep has to be reachable, so it cannot sit behind the wait.
    for _ in $(seq 1 20); do
      kill -0 "${APP}" 2>/dev/null || break
      sleep 0.5
    done
    kill -KILL -- "-${APP}" 2>/dev/null || true
    wait "${APP}" 2>/dev/null || true
  fi
  # Only ever delete a directory this script created. A caller-supplied
  # --work-dir keeps its contents — after a failure the app log in there is
  # the thing you want to read.
  if [ "${created_work}" -eq 1 ]; then
    rm -rf "${work}"
  fi
}
trap cleanup EXIT

# ---------------------------------------------------------------- metadata
pkg_name="$(dpkg-deb -f "${deb}" Package)"
[ "${pkg_name}" = "roost" ] \
  || die "${deb} declares Package '${pkg_name}', expected 'roost' (apt-charliek globs roost_*.deb and apt keys off the control name)."

host_arch="$(dpkg --print-architecture)"
deb_arch="$(dpkg-deb -f "${deb}" Architecture)"
[ "${deb_arch}" = "${host_arch}" ] \
  || die "${deb} declares Architecture '${deb_arch}' but this host is '${host_arch}' — the package was built for the wrong target (there is no cross-compile in build-deb.sh)."

if [ -n "${expect_version}" ]; then
  # nfpm rewrites `-` to `~` in Debian versions (0.0.18-rc1 -> 0.0.18~rc1);
  # callers pass the already-normalized string, and this is what makes that
  # normalization checked rather than assumed.
  deb_version="$(dpkg-deb -f "${deb}" Version)"
  [ "${deb_version}" = "${expect_version}" ] \
    || die "${deb} declares Version '${deb_version}', expected '${expect_version}'."
fi

# ---------------------------------------------------------------- payload
payload="${work}/payload"
rm -rf "${payload}"
mkdir -p "${payload}"
dpkg-deb -x "${deb}" "${payload}"

# Every `dst:` in packaging/nfpm.yaml — that file is the source of truth, this
# list is kept in sync by hand (parsing the YAML would mean a yq dependency in
# the release path for seven constants).
expect_exec=(
  usr/bin/roost
  usr/bin/roostctl
)
expect_file=(
  usr/share/applications/ai.stridelabs.Roost.gtk.desktop
  usr/share/icons/hicolor/256x256/apps/roost.png
  usr/share/icons/hicolor/512x512/apps/roost.png
  usr/share/doc/roost/copyright
  usr/share/doc/roost/README.md
)
for f in "${expect_exec[@]}"; do
  [ -x "${payload}/${f}" ] || die "${f} missing or not executable inside ${deb}"
done
for f in "${expect_file[@]}"; do
  [ -f "${payload}/${f}" ] || die "${f} missing inside ${deb}"
done
echo "payload: all $(( ${#expect_exec[@]} + ${#expect_file[@]} )) packaged destinations present."

UI="${payload}/usr/bin/roost"
ROOSTCTL="${payload}/usr/bin/roostctl"

# ---------------------------------------------------------------- launch
# Its own XDG_* sandbox, so this can't touch anything else on the machine —
# and, load-bearing in CI, so a still-running instance left behind by an
# EARLIER step can't answer our `identify` and make a broken package look
# fine. ROOST_BUNDLE_PROFILE is deliberately UNSET: the whole point is
# proving the *compiled-in default* lands on the production `roost` namespace
# (what existing GTK users already have on disk), not an env override
# papering over a build that defaults to the dev `roost-iced` namespace.
unset ROOST_BUNDLE_PROFILE
export XDG_RUNTIME_DIR="${work}/runtime"
export XDG_DATA_HOME="${work}/data"
export XDG_STATE_HOME="${work}/state"
rm -rf "${XDG_RUNTIME_DIR}" "${XDG_DATA_HOME}" "${XDG_STATE_HOME}"
mkdir -p "${XDG_RUNTIME_DIR}" "${XDG_DATA_HOME}" "${XDG_STATE_HOME}"
chmod 700 "${XDG_RUNTIME_DIR}"

log="${work}/app.log"
# `set -m` puts the background job in its own process group (pgid == $!), which
# is what lets cleanup() signal the whole xvfb-run + Xvfb + roost tree.
set -m
xvfb-run -a --server-args="-screen 0 1280x800x24" "${UI}" > "${log}" 2>&1 &
APP=$!
set +m

fail_with_log() {
  tail -50 "${log}" || true
  die "$*"
}

# Poll a successful `identify` round-trip, NOT socket-file existence: a stale
# socket file can sit there with nothing listening, so a file-existence check
# races and can pass on a dead UI.
identify_out=""
ok=0
for _ in $(seq 1 60); do
  if identify_out="$("${ROOSTCTL}" identify 2>/dev/null)"; then
    ok=1
    break
  fi
  sleep 0.5
done
if [ "${ok}" -ne 1 ]; then
  fail_with_log "roostctl identify never succeeded against the packaged /usr/bin/roost after 30s — the UI never came up (or never opened its IPC socket)."
fi
printf '%s\n' "${identify_out}"

socket_path="$(printf '%s\n' "${identify_out}" | awk -F= '/^socket=/{print $2}')"
if [ -z "${socket_path}" ]; then
  fail_with_log "identify succeeded but printed no socket= line: ${identify_out}"
fi

case "${socket_path}" in
  "${XDG_RUNTIME_DIR}/roost/"*)
    echo "socket ${socket_path} is under the production roost/ namespace — ok."
    ;;
  *)
    fail_with_log "packaged roost bound socket '${socket_path}', expected it under ${XDG_RUNTIME_DIR}/roost/ (the production GTK/roost namespace shared with existing users). The linux-package feature may be missing from this build."
    ;;
esac

if [ -d "${XDG_RUNTIME_DIR}/roost-iced" ]; then
  fail_with_log "${XDG_RUNTIME_DIR}/roost-iced exists — the packaged binary created the DEV iced namespace instead of defaulting to production roost."
fi

echo "smoke ok: ${deb}"
