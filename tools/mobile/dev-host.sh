#!/usr/bin/env bash
# A development host for the mobile app: a private debug roost-session with
# dev_sshd (crates/roost-ssh/examples/dev_sshd.rs) in front of it, standing in
# for Tailscale SSH and client-bridge. Ctrl-C stops both.
#
#   tools/mobile/dev-host.sh [--lan] [--port N] [--key PATH] [--keep]
#
# dev_sshd listens on 127.0.0.1 by default: the Android emulator reaches it as
# 10.0.2.2, a USB-connected phone as 127.0.0.1 after
# `adb reverse tcp:2222 tcp:2222`. --lan listens on every interface, for a
# phone on Wi-Fi, and so for anyone else on that network: dev_sshd takes any
# login with no password.
#
# The session runs under a fresh /tmp/rm-dev-XXXX (socket, state, log, the
# tabs' runtime dir), removed on exit unless --keep; nothing reaches your own
# session. Its tabs run your $SHELL; its first project opens in $HOME. The
# host key persists (default
# ${XDG_STATE_HOME:-~/.local/state}/roost-dev-sshd/ssh_host_ed25519_key), so a
# key the app pinned stays good across runs.
# shellcheck disable=SC2317 # stop and cleanup run from the EXIT trap
set -euo pipefail

usage() {
  echo "usage: $0 [--lan] [--port N] [--key PATH] [--keep]" >&2
  exit 2
}

listen_host=127.0.0.1
port=2222
key="${XDG_STATE_HOME:-${HOME}/.local/state}/roost-dev-sshd/ssh_host_ed25519_key"
keep=0
while [ "$#" -gt 0 ]; do
  case "$1" in
    --lan) listen_host=0.0.0.0 ;;
    --port) [ "$#" -ge 2 ] || usage; port="$2"; shift ;;
    --key) [ "$#" -ge 2 ] || usage; key="$2"; shift ;;
    --keep) keep=1 ;;
    *) usage ;;
  esac
  shift
done

# Nothing a Roost tab exported may reach these processes: ROOST_SOCKET and
# friends would point them at the session this shell may be a tab of.
for name in $(compgen -e); do
  case "${name}" in
    ROOST_* | HERDR_ENV) unset "${name}" ;;
  esac
done

repo="$(cd "$(dirname "$0")/../.." && pwd)"
cd "${repo}"

echo "==> Building roost-session and dev_sshd (debug)"
cargo build --quiet -p roost-session
cargo build --quiet -p roost-ssh --example dev_sshd
target_dir="$(cargo metadata --format-version 1 --no-deps |
  python3 -c 'import json, sys; print(json.load(sys.stdin)["target_directory"])')"
session_bin="${target_dir}/debug/roost-session"
sshd_bin="${target_dir}/debug/examples/dev_sshd"

made="$(mktemp -d /tmp/rm-dev-XXXX)"
# The session refuses a socket dir with a symlinked component (macOS /tmp).
root="$(cd "${made}" && pwd -P)"
session_pid=""
sshd_pid=""

# TERM, which both answer with a clean stop (the session's reaps its tabs),
# then KILL after ten seconds.
stop() {
  local pid="$1"
  [ -n "${pid}" ] || return 0
  kill "${pid}" 2>/dev/null || return 0
  for _ in $(seq 1 100); do
    kill -0 "${pid}" 2>/dev/null || break
    sleep 0.1
  done
  kill -KILL "${pid}" 2>/dev/null || true
  wait "${pid}" 2>/dev/null || true
}

cleanup() {
  trap - EXIT INT TERM
  stop "${sshd_pid}"
  stop "${session_pid}"
  if [ "${keep}" -eq 1 ]; then
    echo "==> Kept ${root}"
  else
    case "${made}" in
      /tmp/rm-dev-?*) rm -rf -- "${made}" ;;
    esac
  fi
}
trap cleanup EXIT
trap 'exit 130' INT TERM

mkdir -m 700 "${root}/ss" "${root}/state" "${root}/rt" "${root}/log"

echo "==> Starting a dev roost-session in ${root}"
(
  export ROOST_SESSION_SOCKET_DIR="${root}/ss"
  export ROOST_STATE_DIR="${root}/state"
  export ROOST_TEST_USER_RUNTIME_DIR="${root}/rt"
  export XDG_STATE_HOME="${root}/log"
  exec "${session_bin}" start --foreground >"${root}/session.out" 2>"${root}/session.err"
) &
session_pid=$!

verdict=""
for _ in $(seq 1 600); do
  IFS= read -r verdict 2>/dev/null <"${root}/session.out" && break
  verdict=""
  kill -0 "${session_pid}" 2>/dev/null || break
  sleep 0.1
done
case "${verdict}" in
  "ready pid="*) ;;
  *)
    echo "dev-host: the session did not start: ${verdict:-no word from it}" >&2
    tail -n 20 "${root}/session.err" >&2 || true
    exit 1
    ;;
esac

"${sshd_bin}" --listen "${listen_host}:${port}" --key "${key}" \
  --socket "${root}/ss/roost.sock" >"${root}/sshd.out" &
sshd_pid=$!
for _ in $(seq 1 100); do
  grep -q '^dev_sshd: every exec' "${root}/sshd.out" 2>/dev/null && break
  kill -0 "${sshd_pid}" 2>/dev/null || {
    echo "dev-host: dev_sshd exited" >&2
    exit 1
  }
  sleep 0.1
done
cat "${root}/sshd.out"
fingerprint="$(sed -n 's/^dev_sshd: host key \(SHA256:[^ ]*\).*/\1/p' "${root}/sshd.out")"

echo
echo "In the app add host: dev@10.0.2.2:${port} (emulator)"
echo "                  or dev@127.0.0.1:${port} (USB phone, after: adb reverse tcp:${port} tcp:${port})"
if [ "${listen_host}" = 0.0.0.0 ]; then
  lan_ip="$(hostname -I 2>/dev/null | awk '{ print $1 }' || true)"
  [ -n "${lan_ip}" ] || lan_ip="$(ipconfig getifaddr en0 2>/dev/null || true)"
  echo "                  or dev@${lan_ip:-<lan-ip>}:${port} (phone on this network)"
fi
echo "Trust the host key if the app shows ${fingerprint}."
echo "Session log: ${root}/log; roostctl --socket ${root}/ss/roost.sock tab list"
echo "Ctrl-C stops the session and dev_sshd."

while kill -0 "${session_pid}" 2>/dev/null && kill -0 "${sshd_pid}" 2>/dev/null; do
  sleep 1
done
echo "dev-host: $(kill -0 "${session_pid}" 2>/dev/null && echo dev_sshd || echo the session) exited" >&2
exit 1
