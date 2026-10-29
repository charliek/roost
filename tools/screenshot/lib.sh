# shellcheck shell=bash
# Shared helpers for the Roost UI test harness.
#
# Both UIs (Swift Mac and Iced) speak the same JSON IPC surface, so the
# driver is one `roostctl` parameterized by `--target {mac,iced}`. Only
# *launch* and *quit* differ per UI; this file isolates those so the
# scenario scripts stay UI-agnostic.
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
# macOS uses two `~/Library/Caches/Roost*` namespaces. Linux keeps the
# isolated iced dev profile on `roost-iced/`; the production `roost/`
# namespace (the packaged iced UI's default profile — formerly owned by
# the retired gtk profile, byte-identical since the rename) is pinned by
# the Rust golden-path tests in `crates/roost-ipc/tests/`, not by this
# harness.
ut_socket_for() {
  case "$1" in
    mac) echo "${HOME}/Library/Caches/Roost/roost.sock" ;;
    iced)
      if [[ "$(uname -s)" == "Darwin" ]]; then
        echo "${HOME}/Library/Caches/Roost-iced/roost.sock"
      elif [[ -n "${XDG_RUNTIME_DIR:-}" && "${XDG_RUNTIME_DIR}" == /* ]]; then
        echo "${XDG_RUNTIME_DIR}/roost-iced/roost.sock"
      else
        echo "/tmp/roost-iced-$(id -u)/roost.sock"
      fi
      ;;
    *) echo "error: unknown target '$1' (want mac|iced)" >&2; return 1 ;;
  esac
}

# --- init ---------------------------------------------------------------

# ut_init <mac|iced> — sets UT_TARGET, UT_RC (roostctl path), UT_SOCK.
ut_init() {
  UT_TARGET="${1:?usage: ut_init <mac|iced>}"
  case "${UT_TARGET}" in mac|iced) ;; *)
    echo "error: target must be mac or iced, got '${UT_TARGET}'" >&2; return 1 ;;
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

# ut_pinned_config — path to a config with `local-backend = in-process`
# and `agent-hooks = off`.
#
# This harness deliberately runs against the developer's own profile (no
# ROOST_STATE_DIR, no ROOST_CONFIG), which on a machine that has never
# run Roost is exactly plan 063 §D5's fresh-install predicate: the UI
# would come up on a `roost-session` daemon AND write `local-backend =
# session` into ~/.config/roost/config.conf. The screenshots are of the
# in-process product, and a smoke harness has no business changing the
# developer's config, so the key is pinned in a COPY — last-wins parsing
# means the copy's trailing line overrides whatever the original said.
#
# `agent-hooks` is pinned for a second reason on top of that one: an
# unanswered key is what plan 064's first-run consent card is raised on,
# so a developer who has never answered it would get that card over
# every single screenshot. `off` is also the only value that touches no
# agent file — this harness runs against the real `$HOME`.
#
# An explicit $ROOST_CONFIG is honoured untouched: a caller who set one
# is driving this on purpose.
ut_pinned_config() {
  if [[ -n "${ROOST_CONFIG:-}" ]]; then printf '%s\n' "${ROOST_CONFIG}"; return 0; fi
  local src="${HOME}/.config/roost/config.conf"
  # `mktemp`, not a fixed name: this lands in a world-writable directory,
  # where a predictable path is both a collision between two concurrent
  # runs and something another user can pre-create for the `>` below to
  # write through.
  local dst
  dst="$(mktemp "${TMPDIR:-/tmp}/roost-uitest-config.XXXXXX")" || return 1
  {
    if [[ -f "${src}" ]]; then cat "${src}"; fi
    printf '\nlocal-backend = in-process\nagent-hooks = off\n'
  } > "${dst}"
  printf '%s\n' "${dst}"
}

# The iced UI's own session socket directory (`ROOST_SESSION_SOCKET_DIR`),
# minted per launch. Without one, a UI whose config says `local-backend =
# session` dials — and can spawn a daemon at — this user's real session
# socket. Its path is recorded in a directory only this user can write, so
# `ut_quit` (a separate invocation) can stop what was started there and
# remove it. Cleanup acts only on a path that is provably one of these
# directories (`ut_session_dir_valid`); anything else is left alone.
UT_SESSION_DIR_LEAF_RE='^roost-uitest-ss\.[A-Za-z0-9]{6}$'

# The canonical temp dir: `/private/tmp` on macOS, where `/tmp` is a
# symlink the daemon would refuse as a socket-dir ancestor.
ut_tmp_root() { (cd /tmp && pwd -P); }

# ut_owner_mode <path> — "<uid> <octal mode>", GNU or BSD stat.
ut_owner_mode() {
  stat -c '%u %a' -- "$1" 2>/dev/null || stat -f '%u %Lp' -- "$1" 2>/dev/null
}

# ut_private_dir <path> — a real directory (not a symlink), owned by this
# user, mode 0700.
ut_private_dir() {
  [[ -n "$1" && -d "$1" && ! -L "$1" ]] || return 1
  [[ "$(ut_owner_mode "$1")" == "$(id -u) 700" ]]
}

# ut_session_dir_valid <path> — exactly one directory directly under the
# canonical temp dir, named by `ut_session_dir_new`'s own pattern, private
# to this user, and already canonical (no `..`, no symlinked component).
ut_session_dir_valid() {
  local dir="$1" root
  root="$(ut_tmp_root)" || return 1
  [[ -n "${dir}" && "${dir%/*}" == "${root}" ]] || return 1
  [[ "${dir##*/}" =~ ${UT_SESSION_DIR_LEAF_RE} ]] || return 1
  ut_private_dir "${dir}" || return 1
  [[ "$(cd "${dir}" && pwd -P)" == "${dir}" ]]
}

# ut_session_dir_record — where the path is kept: the runtime dir, else
# ~/.cache/roost-uitest, and only once validated private. Fails (printing
# why) when neither is.
ut_session_dir_record() {
  local base
  if [[ -n "${XDG_RUNTIME_DIR:-}" && "${XDG_RUNTIME_DIR}" == /* ]] && ut_private_dir "${XDG_RUNTIME_DIR}"; then
    base="${XDG_RUNTIME_DIR}"
  else
    base="${HOME}/.cache/roost-uitest"
    mkdir -p -m 700 "${base}" 2>/dev/null || true
    if ! ut_private_dir "${base}"; then
      echo "warning: neither \$XDG_RUNTIME_DIR nor ${base} is a directory only this user can write; the UI's session socket dir is not recorded and will not be cleaned up" >&2
      return 1
    fi
  fi
  echo "${base}/roost-uitest-${UT_TARGET}.session-dir"
}

# ut_session_dir_new — mint, record and print a private session socket
# dir. Fails closed: a launch with no dir of its own does not happen.
ut_session_dir_new() {
  local root dir record
  root="$(ut_tmp_root)" || { echo "error: cannot resolve /tmp" >&2; return 1; }
  dir="$(mktemp -d "${root}/roost-uitest-ss.XXXXXX")" || {
    echo "error: could not create a private session socket dir under ${root}" >&2
    return 1
  }
  if ! ut_session_dir_valid "${dir}"; then
    echo "error: '${dir}' is not a private session socket dir; refusing to launch on it" >&2
    return 1
  fi
  # A launch nobody recorded is a session no later quit can find and stop.
  if ! record="$(ut_session_dir_record)" || ! printf '%s\n' "${dir}" > "${record}"; then
    rmdir -- "${dir}"
    echo "error: no private place to record '${dir}'; not launching" >&2
    return 1
  fi
  printf '%s\n' "${dir}"
}

# ut_session_stop <socket> — `session.stop` straight to the socket, as raw
# NDJSON: no `roostctl` here, whose own resolution (an older build may
# ignore the override) is beside the point. Exits 0 stopped, 2 nothing
# listening, 1 anything else.
ut_session_stop() {
  python3 - "$1" <<'PY'
import json, socket, sys
s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
s.settimeout(10)
try:
    s.connect(sys.argv[1])
except (FileNotFoundError, ConnectionRefusedError):
    sys.exit(2)
except OSError:
    sys.exit(1)
try:
    s.sendall(b'{"id":"1","op":"session.stop","params":{}}\n')
    reply = json.loads(s.makefile("rb").readline() or b"null")
except (OSError, ValueError):
    sys.exit(1)
sys.exit(0 if isinstance(reply, dict) and "error" not in reply else 1)
PY
}

# ut_session_dir_cleanup — after the UI has stopped: stop any session it
# started in its recorded dir, then remove the dir and only then the
# record. Nothing that fails `ut_session_dir_valid` is ever signalled or
# removed. A stop that does not end in a supported outcome (0 stopped, 2
# nothing listening) keeps both, so a later quit can retry; it returns 1.
ut_session_dir_cleanup() {
  local record dir rc
  record="$(ut_session_dir_record 2>/dev/null)" || return 0
  [[ -f "${record}" && ! -L "${record}" ]] || return 0
  dir="$(head -n1 -- "${record}")"
  if ! ut_session_dir_valid "${dir}"; then
    echo "warning: not touching recorded session dir '${dir}': not one of this harness's private dirs" >&2
    rm -f -- "${record}"
    return 0
  fi
  if [[ -S "${dir}/roost.sock" && ! -L "${dir}/roost.sock" ]]; then
    rc=0
    ut_session_stop "${dir}/roost.sock" || rc=$?
    case "${rc}" in
      0|2) ;;
      *)
        echo "warning: the session at ${dir}/roost.sock did not stop (exit ${rc}); keeping ${dir} and its record for a later quit to retry" >&2
        return 1
        ;;
    esac
  fi
  rm -rf -- "${dir}"
  rm -f -- "${record}"
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
    iced)
      ut_session_dir_cleanup || {
        echo "error: an earlier launch's session is still running; not launching over its record" >&2
        return 1
      }
      local bin="${UT_REPO_ROOT}/target/debug/roost-iced"
      [[ -x "${bin}" ]] || { echo "==> building roost-iced"; ( cd "${UT_REPO_ROOT}" && cargo build -p roost-iced >/dev/null ); }
      echo "==> launching roost-iced"
      local cfg; cfg="$(ut_pinned_config)"
      local sessions
      sessions="$(ut_session_dir_new)" || return 1
      [[ -n "${sessions}" ]] || { echo "error: no session socket dir; not launching" >&2; return 1; }
      ( cd "${UT_REPO_ROOT}" && env -u ROOST_TEST_PASSWD_HOME -u ROOST_TEST_OLD_SESSION_DIR \
        ROOST_SESSION_SOCKET_DIR="${sessions}" ROOST_BUNDLE_PROFILE=iced ROOST_CONFIG="${cfg}" \
        RUST_LOG="${RUST_LOG:-info}" "${bin}" >/tmp/roost-iced-uitest.log 2>&1 & )
      ;;
  esac
  ut_wait_alive
  echo "==> ${UT_TARGET} UI up (pid $(rc identify 2>/dev/null | sed -n 's/^pid=//p'))"
}

# ut_quit — cleanly stop the target UI (exercises the fsync-on-exit path).
ut_quit() {
  if ! ut_alive; then
    [[ "${UT_TARGET}" != iced ]] || ut_session_dir_cleanup || true
    echo "==> ${UT_TARGET} UI not running"
    return 0
  fi
  case "${UT_TARGET}" in
    mac) osascript -e 'tell application "Roost" to quit' >/dev/null 2>&1 || true ;;
    iced)
      local pid; pid="$(rc identify 2>/dev/null | sed -n 's/^pid=//p')"
      [[ -n "${pid}" ]] && kill "${pid}" 2>/dev/null || true
      ;;
  esac
  local waited=0
  while ut_alive; do
    sleep 0.5; waited=$((waited + 1))
    (( waited > 20 )) && { echo "warning: ${UT_TARGET} UI still up after 10s" >&2; return 0; }
  done
  [[ "${UT_TARGET}" != iced ]] || ut_session_dir_cleanup || true
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
