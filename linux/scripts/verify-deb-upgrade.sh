#!/usr/bin/env bash
# Verify the v0.0.17 -> new .deb UPGRADE transaction end to end, after the
# identity rename (plan 025, issue #320).
#
# Why this exists: the rename moves the packaged app id from
# `ai.stridelabs.Roost.gtk` (v0.0.17, the GTK UI) to `ai.stridelabs.Roost`
# (the iced UI), swaps which .desktop file is canonical, and keeps the legacy
# `ai.stridelabs.Roost.gtk.desktop` name alive as a NoDisplay alias. Both
# builds share one socket namespace ($XDG_RUNTIME_DIR/roost/) and one
# state.json, so the only thing standing between an existing Linux user and a
# broken upgrade is the transaction itself: dpkg replacing files, the shipped
# entries pointing at the new WM_CLASS, and the saved workspace layout
# surviving the swap of UI implementations. smoke-deb.sh proves ONE package is
# internally consistent; nothing proved the OLD -> NEW hop. This does, and it
# does it repeatably — the alternative is a hand-run session in a VM whose
# result lives only in a chat log.
#
# It is deliberately a script, not a pytest module: the thing under test is an
# apt transaction on a pristine root filesystem, which is a container's job.
#
# What it checks, in order:
#   1. Both .debs are `roost`, same architecture as the host, old < new.
#   2. Install OLD, launch it, author a deterministic workspace (projects +
#      tabs with distinct titles/cwds) through the OLD roostctl.
#   3. Stop OLD cleanly, snapshot a normalized layout projection.
#   4. `apt-get install ./new.deb` over the top.
#   5. Desktop entries: canonical + legacy-alias content, both valid.
#   6. Launch NEW, assert the socket namespace, `app_id=ai.stridelabs.Roost`,
#      no dev `roost-iced` namespace, and the real X11 WM_CLASS on the window.
#   7. Stop NEW, snapshot the same projection, require it byte-identical.
#
# Note: v0.0.17's roostctl predates the `app_id=` line in `identify`, so the
# app id is only asserted through the NEW CLI. The old side asserts what it
# can: that it answers at all, on the shared socket namespace.
#
# Container assumption: runs AS ROOT in a pristine `ubuntu:24.04` (it installs
# packages, and apt-get needs the network for the .debs' dependencies). The
# two .debs are INPUTS — this script never downloads a roost artifact.
#
#   docker run --rm -v "$PWD:/src" -w /src ubuntu:24.04 \
#     ./linux/scripts/verify-deb-upgrade.sh old/roost_0.0.17_amd64.deb out/roost_0.0.18_amd64.deb
#
# Usage:
#   ./linux/scripts/verify-deb-upgrade.sh <old.deb> <new.deb> [--work-dir <dir>]
#
# Honors ROOST_TEST_TIMEOUT_SCALE (positive integer) to stretch every bounded
# wait on a slow runner, same as the pytest harness.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=linux/scripts/_common.sh
. "${SCRIPT_DIR}/_common.sh"

USAGE="usage: $(basename "$0") <old-deb> <new-deb> [--work-dir <dir>]"
usage() { printf '%s\n' "${USAGE}"; }

old_deb=""
new_deb=""
work_dir=""
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
    -*)
      usage >&2
      die "unknown flag: $1"
      ;;
    *)
      # A bare third positional would let a glob that expanded to three files be
      # read as `<old> <new> <junk>`; and since the two paths are positional,
      # a mis-ordered pair would silently test the downgrade direction instead.
      if [ -z "${old_deb}" ]; then
        old_deb="$1"
      elif [ -z "${new_deb}" ]; then
        new_deb="$1"
      else
        usage >&2
        die "unexpected extra argument: $1"
      fi
      shift
      ;;
  esac
done

[ -n "${old_deb}" ] || { usage >&2; die "missing <old-deb>"; }
[ -n "${new_deb}" ] || { usage >&2; die "missing <new-deb>"; }
[ -f "${old_deb}" ] || die "no such .deb: ${old_deb}"
[ -f "${new_deb}" ] || die "no such .deb: ${new_deb}"
old_deb="$(abspath "${old_deb}")"
new_deb="$(abspath "${new_deb}")"

if [ "$(id -u)" -ne 0 ]; then
  die "must run as root — this script installs packages and runs an apt-get upgrade transaction (run it inside a container: docker run --rm -v \"\$PWD:/src\" -w /src ubuntu:24.04 $(basename "$0") …)."
fi

SCALE="${ROOST_TEST_TIMEOUT_SCALE:-1}"
case "${SCALE}" in
  ''|*[!0-9]*) die "ROOST_TEST_TIMEOUT_SCALE must be a positive integer (got '${SCALE}')" ;;
esac
[ "${SCALE}" -ge 1 ] || die "ROOST_TEST_TIMEOUT_SCALE must be >= 1 (got '${SCALE}')"

XREADY_TICKS=$(( 30 * SCALE ))     # 15s  — Xvfb answering xdpyinfo
IDENTIFY_TICKS=$(( 60 * SCALE ))   # 30s  — UI up + IPC socket accepting
WINDOW_TICKS=$(( 60 * SCALE ))     # 30s  — toplevel mapped with a WM_CLASS
EXIT_TICKS=$(( 40 * SCALE ))       # 20s  — clean shutdown after SIGTERM
XVFB_EXIT_TICKS=$(( 10 * SCALE ))  # 5s   — Xvfb teardown in the EXIT trap

# ---------------------------------------------------------------- prereqs
# Each argument is `<command>:<package>`. Only what is actually missing gets
# installed, so a pre-provisioned image pays nothing.
ensure_tools() {
  local spec cmd pkg
  local -a want_pkgs=() want_cmds=()
  for spec in "$@"; do
    cmd="${spec%%:*}"
    pkg="${spec##*:}"
    if ! command -v "${cmd}" >/dev/null 2>&1; then
      want_cmds+=("${cmd}")
      want_pkgs+=("${pkg}")
    fi
  done
  [ "${#want_pkgs[@]}" -gt 0 ] || return 0
  echo "prereqs: installing ${want_pkgs[*]} (for missing ${want_cmds[*]})"
  DEBIAN_FRONTEND=noninteractive apt-get update -qq \
    || die "apt-get update failed — this script needs the package index (for its own prerequisites and for the .debs' dependencies)."
  DEBIAN_FRONTEND=noninteractive apt-get install -y -qq "${want_pkgs[@]}" \
    || die "apt-get install of prerequisites (${want_pkgs[*]}) failed."
  require_tools "${want_cmds[@]}"
}

require_tools dpkg dpkg-deb apt-get diff find
ensure_tools \
  Xvfb:xvfb \
  dbus-launch:dbus-x11 \
  xdpyinfo:x11-utils \
  xprop:x11-utils \
  xdotool:xdotool \
  desktop-file-validate:desktop-file-utils \
  jq:jq

# ---------------------------------------------------------------- preflight
echo "=== step 1/7: checking both .debs are the same package + architecture ==="
for deb in "${old_deb}" "${new_deb}"; do
  pkg_name="$(dpkg-deb -f "${deb}" Package)"
  [ "${pkg_name}" = "roost" ] \
    || die "${deb} declares Package '${pkg_name}', expected 'roost' — an upgrade only happens between two packages of the same name."
done

host_arch="$(dpkg --print-architecture)"
old_arch="$(dpkg-deb -f "${old_deb}" Architecture)"
new_arch="$(dpkg-deb -f "${new_deb}" Architecture)"
[ "${old_arch}" = "${new_arch}" ] \
  || die "architecture mismatch between the two .debs: old is '${old_arch}', new is '${new_arch}' — that is not an upgrade, it is a cross-arch install."
[ "${old_arch}" = "${host_arch}" ] \
  || die "both .debs are '${old_arch}' but this host is '${host_arch}' — run this on (or in a container for) the matching architecture."

old_version="$(dpkg-deb -f "${old_deb}" Version)"
new_version="$(dpkg-deb -f "${new_deb}" Version)"
echo "upgrade under test: ${old_version} (${old_arch}) -> ${new_version} (${new_arch})"
if ! dpkg --compare-versions "${old_version}" lt "${new_version}"; then
  die "old version '${old_version}' is not strictly less than new version '${new_version}' — apt would treat this as a reinstall or a downgrade, and the upgrade path would go unexercised."
fi

# ---------------------------------------------------------------- work dir
work=""
created_work=0
if [ -n "${work_dir}" ]; then
  mkdir -p "${work_dir}"
  work="$(cd "${work_dir}" && pwd)"
else
  work="$(mktemp -d "${TMPDIR:-/tmp}/roost-upgrade.XXXXXX")"
  created_work=1
fi

UI_PID=""
XVFB_PID=""
DBUS_SESSION_BUS_PID=""
TERMINATED_STATUS=0

# True once the pid has stopped running — including the window where it is a
# not-yet-reaped zombie. `kill -0` cannot answer this: a zombie child still
# accepts signal 0, so a `kill -0` poll on our own child never terminates.
# /proc is fine to depend on here; the whole script is Linux-only.
proc_exited() {
  local pid="$1" stat state
  stat="$(cat "/proc/${pid}/stat" 2>/dev/null)" || return 0
  [ -n "${stat}" ] || return 0
  # Everything up to and including the last ") " is `pid (comm)`; the field
  # after it is the single-letter run state.
  state="${stat##*) }"
  state="${state%% *}"
  [ "${state}" = "Z" ]
}

# Poll `<cmd...>` on 0.5s ticks until it succeeds, at most <ticks> times;
# returns 1 if it never did. Every wait in this script goes through here, so
# they are all bounded and all scaled by ROOST_TEST_TIMEOUT_SCALE.
wait_for() {
  local ticks="$1" i
  shift
  for (( i = 0; i < ticks; i++ )); do
    if "$@"; then return 0; fi
    sleep 0.5
  done
  # One last probe at the deadline itself: without it, success arriving
  # during the final sleep would be reported as a timeout.
  "$@"
}

# SIGTERM, bounded wait, then SIGKILL, and reap either way. Returns 1 when the
# process had to be KILLed, so a caller that treats a dirty exit as a failure
# can say so. `--group` signals the whole process group, which is how the UI's
# PTY children get taken down with it.
terminate() {
  local pid="$1" ticks="$2" target="$1" rc=0
  [ "${3:-}" != "--group" ] || target="-${pid}"
  kill -TERM -- "${target}" 2>/dev/null || true
  if ! wait_for "${ticks}" proc_exited "${pid}"; then
    kill -KILL -- "${target}" 2>/dev/null || true
    rc=1
    # KILL is not instant either; bound the reap instead of blocking
    # forever on a child stuck in uninterruptible sleep.
    wait_for "${ticks}" proc_exited "${pid}" || return "${rc}"
  fi
  # Reap, keeping the child's exit status visible to callers: SIGTERM death
  # (128+15) is the expected shape for both UIs — neither installs a
  # handler — so callers log it rather than asserting zero.
  TERMINATED_STATUS=0
  wait "${pid}" 2>/dev/null || TERMINATED_STATUS=$?
  return "${rc}"
}

# Dump the tail of the relevant log before dying: inside the container it is
# the only artifact, and a caller-supplied --work-dir is what keeps it around.
fail_with_log() {
  local log="$1"
  shift
  tail -50 "${log}" 2>/dev/null || true
  die "$*"
}

cleanup() {
  if [ -n "${UI_PID}" ]; then
    terminate "${UI_PID}" "${EXIT_TICKS}" --group || true
  fi
  if [ -n "${DBUS_SESSION_BUS_PID}" ]; then
    kill -TERM "${DBUS_SESSION_BUS_PID}" 2>/dev/null || true
  fi
  if [ -n "${XVFB_PID}" ]; then
    terminate "${XVFB_PID}" "${XVFB_EXIT_TICKS}" || true
  fi
  # Only ever delete a directory this script created — a caller-supplied
  # --work-dir keeps the two UI logs and both layout snapshots, which is
  # exactly what you want to read after a failure.
  if [ "${created_work}" -eq 1 ]; then
    rm -rf "${work}"
  fi
}
trap cleanup EXIT

# ---------------------------------------------------------------- session
# One isolated XDG sandbox shared by BOTH UIs: sharing it is the point (the
# upgrade has to carry state across), isolating it from the machine keeps a
# stray instance from answering our `identify` and making a broken upgrade
# look fine. ROOST_BUNDLE_PROFILE stays UNSET so each binary lands on its own
# compiled-in default namespace, which is half of what is under test; the
# other three are inherited when this is run from inside a Roost tab, and
# would silently point the CLI (or state.json) at the developer's own live
# session instead of the sandbox.
unset ROOST_BUNDLE_PROFILE ROOST_SOCKET ROOST_TAB_ID ROOST_STATE_DIR
export XDG_RUNTIME_DIR="${work}/runtime"
export XDG_STATE_HOME="${work}/state"
export XDG_DATA_HOME="${work}/data"
export XDG_CONFIG_HOME="${work}/config"
rm -rf "${XDG_RUNTIME_DIR}" "${XDG_STATE_HOME}" "${XDG_DATA_HOME}" "${XDG_CONFIG_HOME}"
mkdir -p "${XDG_RUNTIME_DIR}" "${XDG_STATE_HOME}" "${XDG_DATA_HOME}" "${XDG_CONFIG_HOME}"
chmod 700 "${XDG_RUNTIME_DIR}"

# One persistent display for the whole run, rather than xvfb-run per launch:
# the WM_CLASS check needs a display that outlives a single UI process, and
# both UIs must see the same session.
pick_display() {
  local n
  for n in {99..120}; do
    if [ ! -e "/tmp/.X${n}-lock" ] && [ ! -e "/tmp/.X11-unix/X${n}" ]; then
      printf ':%s\n' "${n}"
      return 0
    fi
  done
  die "no free X display slot in :99-:120"
}

# Split, not `export DISPLAY="$(pick_display)"`: pick_display's `die` only
# exits the command-substitution subshell, so the failure is caught by the
# assignment's exit status — and `export` always returns 0, which would mask it.
export DISPLAY
DISPLAY="$(pick_display)"
Xvfb "${DISPLAY}" -screen 0 1280x800x24 >"${work}/xvfb.log" 2>&1 &
XVFB_PID=$!

x_answers() { xdpyinfo -display "${DISPLAY}" >/dev/null 2>&1; }
if ! wait_for "${XREADY_TICKS}" x_answers; then
  fail_with_log "${work}/xvfb.log" "Xvfb on ${DISPLAY} never answered xdpyinfo. Log tail above."
fi
# The GTK UI (old) needs an X11 backend explicitly; iced (new) ignores it.
export GDK_BACKEND=x11

dbus_env="$(dbus-launch --sh-syntax)" || die "dbus-launch failed — the GTK UI needs a session bus."
eval "${dbus_env}"
export DBUS_SESSION_BUS_ADDRESS
echo "session: DISPLAY=${DISPLAY}, session bus pid ${DBUS_SESSION_BUS_PID}, XDG sandbox under ${work}"

# ---------------------------------------------------------------- helpers
UI="/usr/bin/roost"
ROOSTCTL="/usr/bin/roostctl"
UI_LOG=""
IDENTIFY_OUT=""

# `set -m` puts the job in its own process group (pgid == $!), which is what
# lets a single kill reach the UI's PTY children too.
launch_ui() {
  UI_LOG="$1"
  set -m
  "${UI}" >"${UI_LOG}" 2>&1 &
  UI_PID=$!
  set +m
  echo "launched ${UI} (pid ${UI_PID}), log ${UI_LOG}"
}

try_identify() { IDENTIFY_OUT="$("${ROOSTCTL}" identify 2>/dev/null)"; }

# Poll a successful `identify` ROUND-TRIP, never the socket file: a stale
# socket inode can sit there with nothing listening, so a file-existence check
# races and can pass against a dead UI. (Repo lore; do not "simplify" this.)
wait_identify() {
  IDENTIFY_OUT=""
  if ! wait_for "${IDENTIFY_TICKS}" try_identify; then
    fail_with_log "${UI_LOG}" "roostctl identify never succeeded against ${UI} within $(( IDENTIFY_TICKS / 2 ))s — the UI never came up (or never opened its IPC socket). Log tail above."
  fi
  printf '%s\n' "${IDENTIFY_OUT}"
}

assert_socket_namespace() {
  local socket_path
  # sed, not `awk -F=`: the path itself may contain `=`, and -F= would
  # truncate it at the second delimiter.
  socket_path="$(sed -n 's/^socket=//p' <<<"${IDENTIFY_OUT}" | head -n1)"
  if [ -z "${socket_path}" ]; then
    die "identify succeeded but printed no socket= line. Full output:"$'\n'"${IDENTIFY_OUT}"
  fi
  case "${socket_path}" in
    "${XDG_RUNTIME_DIR}/roost/"*)
      echo "socket ${socket_path} is under the production roost/ namespace — ok."
      ;;
    *)
      die "bound socket '${socket_path}', expected it under ${XDG_RUNTIME_DIR}/roost/ (the one namespace both the old GTK build and the new packaged build must share, or an upgraded user's roostctl stops finding the UI). Full identify output:"$'\n'"${IDENTIFY_OUT}"
      ;;
  esac
}

# The one launch ritual, shared by the old and the new build.
start_ui() {
  launch_ui "$1"
  wait_identify
  assert_socket_namespace
}

# The layout snapshot has to happen after this: state.json is written through
# during the session, and fsync'd on clean exit.
stop_ui() {
  local pid="${UI_PID}"
  [ -n "${pid}" ] || return 0
  if ! terminate "${pid}" "${EXIT_TICKS}" --group; then
    UI_PID=""
    die "${UI} (pid ${pid}) did not exit within $(( EXIT_TICKS / 2 ))s of SIGTERM; SIGKILLed it. A dirty exit makes the layout snapshot untrustworthy, so this is a failure, not a warning."
  fi
  UI_PID=""
  # Reaped by terminate, so `kill -0` is now an honest liveness question.
  if kill -0 "${pid}" 2>/dev/null; then
    die "pid ${pid} still accepts signals after being reaped — refusing to treat the UI as stopped."
  fi
  # state.json is write-through during the session, so the snapshot below is
  # valid even though SIGTERM death skips the exit-path fsync (page cache is
  # coherent for same-machine reads).
  echo "stopped ${UI} (pid ${pid}); exit status ${TERMINATED_STATUS} (SIGTERM death expected — the UIs install no handler)."
}

# The stable projection of state.json: every project's (name, cwd) and its
# tabs' (title, cwd), in position order. Tab and project IDs are EXCLUDED —
# a relaunch re-mints them by design (tabs come back as fresh shells), so
# including them would make an intentional behavior look like a regression.
LAYOUT_JQ='[ .projects // [] | sort_by(.position)[] | { name, cwd, tabs: [ .tabs // [] | sort_by(.position)[] | { title, cwd } ] } ]'

# v0.0.17 put state.json in $XDG_DATA_HOME/roost (state_dir), with the log in
# $XDG_STATE_HOME/roost — an easy pair to mix up, so both trees are searched
# and the winner is printed instead of assumed. The path is discovered ONCE
# and pinned in STATE_JSON: if the second snapshot re-ran the search and the
# new UI persisted to a different location, the diff would compare the old
# file against itself and pass without the new UI restoring anything.
STATE_JSON=""
snapshot_layout() {
  local out="$1"
  if [ -z "${STATE_JSON}" ]; then
    STATE_JSON="$(find "${XDG_DATA_HOME}" "${XDG_STATE_HOME}" -name state.json -type f 2>/dev/null | sort | head -n1 || true)"
    if [ -z "${STATE_JSON}" ]; then
      find "${XDG_DATA_HOME}" "${XDG_STATE_HOME}" 2>/dev/null || true
      die "no state.json anywhere under ${XDG_DATA_HOME} or ${XDG_STATE_HOME} — the UI persisted nothing (tree listing above)."
    fi
    echo "layout source: ${STATE_JSON} (pinned for both snapshots)"
  elif [ ! -f "${STATE_JSON}" ]; then
    find "${XDG_DATA_HOME}" "${XDG_STATE_HOME}" 2>/dev/null || true
    die "pinned state.json ${STATE_JSON} is gone after the upgrade — the new UI abandoned the old state location (tree listing above)."
  fi
  jq -S "${LAYOUT_JQ}" "${STATE_JSON}" >"${out}" \
    || die "jq could not project ${STATE_JSON} into a layout snapshot — the state.json schema changed or the file is malformed."
}

# ---------------------------------------------------------------- install old
echo "=== step 2/7: installing the OLD package (${old_version}) ==="
DEBIAN_FRONTEND=noninteractive apt-get install -y "${old_deb}" \
  || die "apt-get install of the old .deb (${old_deb}) failed."
installed="$(dpkg-query -W -f '${Version}' roost)"
[ "${installed}" = "${old_version}" ] \
  || die "after installing ${old_deb}, dpkg reports roost ${installed}, expected ${old_version}."
echo "installed roost ${installed}."

start_ui "${work}/old-ui.log"

# ---------------------------------------------------------------- author state
# Only commands that exist in v0.0.17's roostctl are used here: `project
# create`, `tab open`, `tab list --json`, `set-title`, `identify`.
echo "--- authoring a deterministic workspace through the OLD roostctl ---"
create_project() {
  local name="$1" cwd="$2" out id
  out="$("${ROOSTCTL}" project create --name "${name}" --cwd "${cwd}")" \
    || die "roostctl project create --name ${name} failed against the old UI."
  id="$(printf '%s\n' "${out}" | sed -n 's/^created project \([0-9][0-9]*\).*/\1/p')"
  [ -n "${id}" ] || die "could not read a project id out of 'project create' output: ${out}"
  printf '%s\n' "${id}"
}

open_tab() {
  local project_id="$1" cwd="$2" title="$3" id
  id="$("${ROOSTCTL}" tab open --project-id "${project_id}" --cwd "${cwd}" --title "${title}")" \
    || die "roostctl tab open --project-id ${project_id} --cwd ${cwd} failed against the old UI."
  case "${id}" in
    ''|*[!0-9]*) die "'tab open' printed '${id}', expected a bare tab id." ;;
  esac
  printf '%s\n' "${id}"
}

alpha_id="$(create_project upgrade-alpha /tmp)"
beta_id="$(create_project upgrade-beta /root)"
open_tab "${alpha_id}" /tmp  alpha-one >/dev/null
open_tab "${alpha_id}" /root alpha-two >/dev/null
open_tab "${beta_id}"  /root beta-one  >/dev/null
echo "authored projects ${alpha_id} (upgrade-alpha) + ${beta_id} (upgrade-beta) with 3 explicit tabs."

# Give a tab to any project that has none — in practice the default project
# the UI creates for itself on first launch. A project persisted with an
# EMPTY tab list is re-seeded with one fresh tab by the next restore (see
# roost-engine/src/persistence.rs: "no saved tabs" -> the UI seeds a single
# tab on restore), so such a project's layout legitimately changes across
# any relaunch. Measured: an old -> old relaunch with no upgrade in it
# produces exactly the same delta. Leaving one in the fixture would fail the
# byte-identical diff below on something the upgrade did not cause.
empty_project_ids="$("${ROOSTCTL}" tab list --json | jq -r '.projects[] | select((.tabs // []) | length == 0) | .id')" \
  || die "roostctl tab list --json failed against the old UI."
while read -r empty_id; do
  [ -n "${empty_id}" ] || continue
  open_tab "${empty_id}" /tmp "seeded-${empty_id}" >/dev/null
  echo "gave project ${empty_id} a tab (it had none, and an empty project is re-seeded on every restore)."
done <<<"${empty_project_ids}"

# Lock EVERY tab's title, including the ones the UI seeded itself. `tab open
# --title` leaves the title a placeholder (`user_titled=false`), which the
# model is free to re-derive from the cwd on the first post-relaunch shell
# report — a legitimate behavior that would masquerade as upgrade data loss in
# the diff below. `set-title` claims the manual-rename lock, which persists.
tab_ids="$("${ROOSTCTL}" tab list --json | jq -r '.projects[] | .tabs[]? | .id')" \
  || die "roostctl tab list --json failed against the old UI."
[ -n "${tab_ids}" ] || die "the old UI reports no tabs at all — nothing to carry across the upgrade."
tab_count=0
while read -r tab_id; do
  [ -n "${tab_id}" ] || continue
  tab_count=$(( tab_count + 1 ))
  "${ROOSTCTL}" set-title --tab "${tab_id}" --title "upgrade-tab-${tab_count}" \
    || die "roostctl set-title --tab ${tab_id} failed against the old UI."
done <<<"${tab_ids}"
echo "locked ${tab_count} tab titles."

# ---------------------------------------------------------------- snapshot old
echo "=== step 3/7: stopping the OLD UI and snapshotting its layout ==="
stop_ui
snapshot_layout "${work}/layout-before.json"

# ---------------------------------------------------------------- upgrade
echo "=== step 4/7: upgrading to the NEW package (${new_version}) ==="
DEBIAN_FRONTEND=noninteractive apt-get install -y "${new_deb}" \
  || die "apt-get install of the new .deb over ${old_version} failed — the upgrade transaction itself is broken."
installed="$(dpkg-query -W -f '${Version}' roost)"
[ "${installed}" = "${new_version}" ] \
  || die "after upgrading, dpkg reports roost ${installed}, expected ${new_version}."
echo "upgraded roost ${old_version} -> ${installed}."

# ------------------------------------------------------- desktop entries
# Exact-line (`grep -qxF`) throughout: the legacy id `ai.stridelabs.Roost.gtk`
# CONTAINS the new id `ai.stridelabs.Roost` as a literal prefix, so a
# substring grep for the new value passes vacuously against the old content.
echo "=== step 5/7: checking the installed desktop entries ==="
apps=/usr/share/applications
canonical_desktop="${apps}/ai.stridelabs.Roost.desktop"
alias_desktop="${apps}/ai.stridelabs.Roost.gtk.desktop"

if [ ! -f "${canonical_desktop}" ]; then
  ls -la "${apps}" || true
  die "${canonical_desktop} is missing after the upgrade — the canonical desktop entry is what a fresh menu/launcher entry keys off. Directory listing above."
fi
if [ ! -f "${alias_desktop}" ]; then
  ls -la "${apps}" || true
  die "${alias_desktop} is missing after the upgrade — v0.0.17 installed the entry under that name, and any launcher pin made then references that desktop-file id forever. Directory listing above."
fi

grep -qxF "StartupWMClass=ai.stridelabs.Roost" "${canonical_desktop}" \
  || die "${canonical_desktop} has no exact line 'StartupWMClass=ai.stridelabs.Roost'; found: $(grep -n '^StartupWMClass=' "${canonical_desktop}" || echo '(no StartupWMClass line at all)')"
# `if`, not `&& die`: under `set -e` the desired no-match case would kill the
# script through the `&&` list with no message.
if grep -qxF "NoDisplay=true" "${canonical_desktop}"; then
  die "${canonical_desktop} is the canonical entry and must not carry NoDisplay=true — with it, the upgrade leaves the user no visible menu entry at all."
fi

grep -qxF "NoDisplay=true" "${alias_desktop}" \
  || die "${alias_desktop} has no exact line 'NoDisplay=true' — after the upgrade it is the legacy alias, and without NoDisplay the user gets a duplicate menu entry."
grep -qxF "StartupWMClass=ai.stridelabs.Roost" "${alias_desktop}" \
  || die "${alias_desktop} still has the old content: expected exact line 'StartupWMClass=ai.stridelabs.Roost' (the alias must point at the id the NEW binary announces, or a pinned launcher stops grouping with the running window); found: $(grep -n '^StartupWMClass=' "${alias_desktop}" || echo '(no StartupWMClass line at all)')"

desktop-file-validate "${canonical_desktop}" "${alias_desktop}" \
  || die "desktop-file-validate rejected an installed .desktop file after the upgrade."
echo "desktop entries: canonical + legacy alias both present, correctly identified, and valid."

# ---------------------------------------------------------------- launch new
echo "=== step 6/7: launching the NEW UI on the same display + XDG sandbox ==="
start_ui "${work}/new-ui.log"

grep -qxF "app_id=ai.stridelabs.Roost" <<<"${IDENTIFY_OUT}" \
  || die "the upgraded binary's identify has no exact 'app_id=ai.stridelabs.Roost' line — it is still announcing the legacy id (exact-line match, because the legacy id has the new one as a prefix). Full output:"$'\n'"${IDENTIFY_OUT}"
echo "app_id=ai.stridelabs.Roost — ok."

if [ -d "${XDG_RUNTIME_DIR}/roost-iced" ]; then
  die "${XDG_RUNTIME_DIR}/roost-iced exists — the upgraded binary created the DEV iced namespace instead of the production roost one, so an upgraded user's roostctl (and their Claude hooks) would dial an empty path."
fi

# ---------------------------------------------------------------- WM_CLASS
# The desktop entries claim a StartupWMClass; this is the other half — what
# the running window actually announces. A shell matches a pin to a window
# through exactly this pair.
win_ids=""
find_roost_windows() {
  win_ids="$(xdotool search --class Roost 2>/dev/null)" && [ -n "${win_ids}" ]
}
if ! wait_for "${WINDOW_TICKS}" find_roost_windows; then
  fail_with_log "${UI_LOG}" "no window matching class 'Roost' appeared on ${DISPLAY} within $(( WINDOW_TICKS / 2 ))s. Log tail above."
fi

wm_class_out=""
while read -r win; do
  [ -n "${win}" ] || continue
  wm_class_out+="$(xprop -id "${win}" WM_CLASS 2>/dev/null || true)"$'\n'
done <<<"${win_ids}"

# Match the full quoted token: `"ai.stridelabs.Roost"` cannot appear inside
# `"ai.stridelabs.Roost.gtk"`, because the closing quote is part of the
# needle. An unquoted needle would match both.
if ! grep -qF '"ai.stridelabs.Roost"' <<<"${wm_class_out}"; then
  die "no window announces WM_CLASS token \"ai.stridelabs.Roost\". xprop said:"$'\n'"${wm_class_out}"
fi
if grep -qF 'ai.stridelabs.Roost.gtk' <<<"${wm_class_out}"; then
  die "a window still announces the legacy WM_CLASS 'ai.stridelabs.Roost.gtk' after the upgrade. xprop said:"$'\n'"${wm_class_out}"
fi
echo "WM_CLASS: the mapped window announces ai.stridelabs.Roost — ok."

# ---------------------------------------------------------------- layout diff
echo "=== step 7/7: stopping the NEW UI and comparing layouts ==="
stop_ui
snapshot_layout "${work}/layout-after.json"

if ! diff -u "${work}/layout-before.json" "${work}/layout-after.json"; then
  die "the saved workspace layout did not survive the upgrade (diff above; '-' is pre-upgrade, '+' is post-upgrade). Projects, tab titles, tab cwds and their order must all carry across — tab and project IDs are excluded from the comparison because a relaunch re-mints them by design."
fi
# Counted from what was actually compared, not from the pre-upgrade loop.
surviving_tabs="$(jq '[.[].tabs[]] | length' "${work}/layout-after.json")"
echo "layout: ${surviving_tabs} tabs across their projects survived the upgrade byte-identically."

echo "upgrade-verify ok: ${old_version} -> ${new_version}"
