# shellcheck shell=bash
# Shared helpers for the Roost UI test harness.
#
# Both native UIs (Swift Mac, gtk4-rs Linux) speak the same JSON IPC
# surface, so the driver is one `roostctl` parameterized by
# `--target {mac,gtk}`. Only *launch* and *quit* differ per UI; this
# file isolates those so the scenario scripts stay UI-agnostic.
#
# Source it, then call `ut_init <target>` before anything else.

set -euo pipefail

# --- paths --------------------------------------------------------------

UT_LIB_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
UT_REPO_ROOT="$(cd "${UT_LIB_DIR}/../.." && pwd)"

# Resolve a freshly-built roostctl. The repo-root `./roost-cli` is a
# stale pre-port binary — never use it. Prefer release, then debug,
# then build debug on demand.
ut_resolve_roostctl() {
  if [[ -x "${UT_REPO_ROOT}/target/release/roostctl" ]]; then
    echo "${UT_REPO_ROOT}/target/release/roostctl"
  elif [[ -x "${UT_REPO_ROOT}/target/debug/roostctl" ]]; then
    echo "${UT_REPO_ROOT}/target/debug/roostctl"
  else
    echo "==> roostctl not built; building (cargo build -p roost-cli)" >&2
    ( cd "${UT_REPO_ROOT}" && cargo build -p roost-cli >&2 )
    echo "${UT_REPO_ROOT}/target/debug/roostctl"
  fi
}

# Per-target socket path (matches roost-ipc's BundleProfile resolver).
# macOS uses ~/Library/Caches/Roost{,-gtk}; Linux uses XDG (both
# profiles share one path there, so the value is informational on
# Linux — `roostctl --target` still disambiguates).
ut_socket_for() {
  case "$1" in
    mac) echo "${HOME}/Library/Caches/Roost/roost.sock" ;;
    gtk)
      if [[ "$(uname -s)" == "Darwin" ]]; then
        echo "${HOME}/Library/Caches/Roost-gtk/roost.sock"
      else
        echo "${XDG_RUNTIME_DIR:-/tmp/roost-$(id -u)}/roost/roost.sock"
      fi
      ;;
    *) echo "error: unknown target '$1' (want mac|gtk)" >&2; return 1 ;;
  esac
}

# --- init ---------------------------------------------------------------

# ut_init <mac|gtk> — sets UT_TARGET, UT_RC (roostctl path), UT_SOCK.
ut_init() {
  UT_TARGET="${1:?usage: ut_init <mac|gtk>}"
  case "${UT_TARGET}" in mac|gtk) ;; *)
    echo "error: target must be mac or gtk, got '${UT_TARGET}'" >&2; return 1 ;;
  esac
  UT_RC="$(ut_resolve_roostctl)"
  UT_SOCK="$(ut_socket_for "${UT_TARGET}")"
  export UT_TARGET UT_RC UT_SOCK
}

# rc … — run roostctl against the active target.
rc() { "${UT_RC}" --target "${UT_TARGET}" "$@"; }

# --- lifecycle ----------------------------------------------------------

# ut_alive — true when the target UI answers `identify`.
ut_alive() { rc identify >/dev/null 2>&1; }

# ut_wait_alive [timeout_s] — block until the UI is reachable.
ut_wait_alive() {
  local timeout="${1:-15}" waited=0
  until ut_alive; do
    sleep 0.5; waited=$((waited + 1))
    if (( waited > timeout * 2 )); then
      echo "error: ${UT_TARGET} UI did not come up within ${timeout}s" >&2
      return 1
    fi
  done
}

# ut_launch — start the target UI if it isn't already running.
ut_launch() {
  if ut_alive; then
    echo "==> ${UT_TARGET} UI already running (pid $(rc identify 2>/dev/null | sed -n 's/^pid=//p'))"
    return 0
  fi
  case "${UT_TARGET}" in
    mac)
      [[ "$(uname -s)" == "Darwin" ]] || { echo "error: mac target needs macOS" >&2; return 1; }
      local app="${UT_REPO_ROOT}/mac/build/Roost.app"
      [[ -d "${app}" ]] || { echo "==> bundling Roost.app"; ( cd "${UT_REPO_ROOT}/mac" && ./scripts/bundle.sh debug >/dev/null ); }
      echo "==> launching Roost.app"
      open "${app}"
      ;;
    gtk)
      local bin="${UT_REPO_ROOT}/target/debug/roost"
      [[ -x "${bin}" ]] || { echo "==> building roost (GTK)"; ( cd "${UT_REPO_ROOT}" && cargo build -p roost-linux >/dev/null ); }
      echo "==> launching roost (GTK)"
      ( cd "${UT_REPO_ROOT}" && RUST_LOG="${RUST_LOG:-info}" "${bin}" >/tmp/roost-gtk-uitest.log 2>&1 & )
      ;;
  esac
  ut_wait_alive
  echo "==> ${UT_TARGET} UI up (pid $(rc identify 2>/dev/null | sed -n 's/^pid=//p'))"
}

# ut_quit — cleanly stop the target UI (exercises the fsync-on-exit path).
ut_quit() {
  ut_alive || { echo "==> ${UT_TARGET} UI not running"; return 0; }
  case "${UT_TARGET}" in
    mac) osascript -e 'tell application "Roost" to quit' >/dev/null 2>&1 || true ;;
    gtk)
      local pid; pid="$(rc identify 2>/dev/null | sed -n 's/^pid=//p')"
      [[ -n "${pid}" ]] && kill "${pid}" 2>/dev/null || true
      ;;
  esac
  local waited=0
  while ut_alive; do
    sleep 0.5; waited=$((waited + 1))
    (( waited > 20 )) && { echo "warning: ${UT_TARGET} UI still up after 10s" >&2; break; }
  done
  echo "==> ${UT_TARGET} UI stopped"
}

# --- capture ------------------------------------------------------------

# shot <outdir> <name> — capture a 2x PNG named <name>.png into outdir
# and append a manifest row. Prints the path.
shot() {
  local outdir="$1" name="$2"
  mkdir -p "${outdir}"
  local path="${outdir}/${name}.png"
  rc screenshot --out "${path}" --scale 2 >/dev/null
  echo "${path}"
}

# expect <outdir> <name> <what-to-look-for> — record an expectation row
# in the run manifest so a human/agent can verify the matching shot.
expect() {
  local outdir="$1" name="$2"; shift 2
  printf -- '- **%s.png** — %s\n' "${name}" "$*" >> "${outdir}/manifest.md"
}

# ut_reset_states <tab...> — clear agent state + notification on tabs.
ut_reset_states() {
  local t
  for t in "$@"; do
    rc tab set-state --state none --tab "${t}" >/dev/null 2>&1 || true
    rc tab clear-notification --tab "${t}" >/dev/null 2>&1 || true
  done
}
