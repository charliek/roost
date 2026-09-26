#!/usr/bin/env bash
# Fetch a released `roost-session` for the old-session lane (plan 072 D13):
# `gh release download`, verified against the release's own `.sha256`,
# cached under target/old-session/<version>/.
#
# Usage:
#   fetch.sh <version|latest>     the binary, from the cache or GitHub
#   fetch.sh --resolve latest     only the version "latest" names
#
# stdout is key=value lines, and both outcomes a run can build on exit 0:
#   version=<x.y.z> binary=<path>   verified against its .sha256
#   skip=<reason>                   GitHub could not serve it and nothing is
#                                   cached; a GitHub ::warning:: says so first
# A broken release (no such release, no asset for this arch, no .sha256
# beside it) and a binary that does not match its .sha256 — cached or just
# downloaded — exit 1. A missing .sha256 is a failure, not a skip: both
# pinned releases ship one, so its absence is a release that is broken.
#
# Environment:
#   ROOST_OLD_SESSION_VERSION   stands in for "latest"
#   ROOST_OLD_SESSION_CACHE     the cache root (default target/old-session)

set -euo pipefail

REPO="charliek/roost"
ATTEMPTS=3
ATTEMPT_SECONDS=30

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
CACHE="${ROOST_OLD_SESSION_CACHE:-${ROOT}/target/old-session}"

resolve_only=0
if [ "${1:-}" = "--resolve" ]; then
  resolve_only=1
  shift
fi
if [ "$#" -ne 1 ]; then
  echo "usage: $0 [--resolve] <version|latest>" >&2
  exit 2
fi
requested="$1"

skip() {
  echo "::warning::old-session lane skipped: $1"
  echo "skip=$1"
  exit 0
}

fail() {
  echo "fetch.sh: $1" >&2
  exit 1
}

# attempt ARGS... — `gh ARGS...`, up to ATTEMPTS tries. Leaves stdout in
# $out, and the last failed try's stderr in $last_error.
out=""
last_error=""
attempt() {
  local n errfile
  errfile="$(mktemp)"
  for n in $(seq 1 "${ATTEMPTS}"); do
    if out="$(timeout -k 5 "${ATTEMPT_SECONDS}" gh "$@" 2>"${errfile}")"; then
      rm -f "${errfile}"
      return 0
    fi
    last_error="$(tr '\n' ' ' <"${errfile}" | sed 's/ *$//')"
    if [ "${n}" -lt "${ATTEMPTS}" ]; then
      sleep 1
    fi
  done
  rm -f "${errfile}"
  return 1
}

have_gh=1
command -v gh >/dev/null 2>&1 || have_gh=0

if [ "${requested}" = "latest" ] && [ -n "${ROOST_OLD_SESSION_VERSION:-}" ]; then
  requested="${ROOST_OLD_SESSION_VERSION}"
fi
if [ "${requested}" = "latest" ]; then
  [ "${have_gh}" -eq 1 ] || skip "gh is not installed, so the latest release cannot be resolved"
  if ! attempt api "repos/${REPO}/releases/latest" --jq .tag_name; then
    skip "GitHub could not resolve the latest release: ${last_error}"
  fi
  requested="${out}"
fi
version="${requested#v}"
[[ "${version}" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || fail "not a release version: ${requested}"

if [ "${resolve_only}" -eq 1 ]; then
  echo "version=${version}"
  exit 0
fi

[ "$(uname -s)" = "Linux" ] || skip "releases ship roost-session for Linux only, not $(uname -s)"
case "$(uname -m)" in
  x86_64 | amd64) arch=amd64 ;;
  aarch64 | arm64) arch=arm64 ;;
  *) skip "releases ship no roost-session for $(uname -m)" ;;
esac

asset="roost-session-${version}-linux-${arch}"
dir="${CACHE}/${version}"
# Named `roost-session`, not after the asset: the harness fences every
# signal it sends a daemon on the process name being exactly that.
binary="${dir}/roost-session"
sidecar="${dir}/roost-session.sha256"

# verify BINARY SIDECAR — the sidecar's first field is the digest.
verify() {
  local want got
  want="$(awk 'NR == 1 { print $1 }' "$2")"
  [[ "${want}" =~ ^[0-9a-f]{64}$ ]] || fail "$2 holds no sha256 digest"
  got="$(sha256sum "$1" | awk '{ print $1 }')"
  [ "${got}" = "${want}" ] || fail "$1 does not match its .sha256 (want ${want}, got ${got})"
}

if [ -f "${binary}" ] && [ -f "${sidecar}" ]; then
  verify "${binary}" "${sidecar}"
  echo "version=${version}"
  echo "binary=${binary}"
  exit 0
fi

[ "${have_gh}" -eq 1 ] || skip "gh is not installed and v${version} is not cached"
if ! attempt release view "v${version}" --repo "${REPO}" --json assets --jq '.assets[].name'; then
  case "${last_error}" in
    *"release not found"* | *"HTTP 404"*) fail "there is no release v${version}" ;;
  esac
  skip "GitHub could not list release v${version}: ${last_error}"
fi
grep -qxF "${asset}" <<<"${out}" || fail "release v${version} has no ${asset}"
grep -qxF "${asset}.sha256" <<<"${out}" || fail "release v${version} has ${asset} but no ${asset}.sha256"

mkdir -p "${dir}"
partial="$(mktemp -d "${dir}/.partial.XXXXXX")"
cleanup_partial() {
  rm -f "${partial}/${asset}" "${partial}/${asset}.sha256"
  rmdir "${partial}" 2>/dev/null || true
}
trap cleanup_partial EXIT
if ! attempt release download "v${version}" --repo "${REPO}" \
  --pattern "${asset}" --pattern "${asset}.sha256" --dir "${partial}" --clobber; then
  skip "GitHub could not serve ${asset}: ${last_error}"
fi
verify "${partial}/${asset}" "${partial}/${asset}.sha256"
chmod +x "${partial}/${asset}"
mv "${partial}/${asset}.sha256" "${sidecar}"
mv "${partial}/${asset}" "${binary}"

echo "version=${version}"
echo "binary=${binary}"
