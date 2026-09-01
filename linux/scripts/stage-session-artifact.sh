#!/usr/bin/env bash
# Stage the standalone `roost-session` binary as a release asset, plus its
# sha256 checksum sibling.
#
# linux/scripts/build-deb.sh already builds `roost-session` in the same
# cargo invocation as the iced UI and roostctl, and stages the result at
# dist/roost-session (0755) alongside them — this script builds nothing. It
# copies that existing product to the exact name the release-asset install
# rung of the client (`asset_name`/`checksum_name` in
# crates/roost-ipc/src/bootstrap.rs) requests, and writes the checksum
# sidecar that rung verifies before install.
#
# Factored out of release.yml (plan 039 §3.7) so it can be exercised by a
# shell test (stage-session-artifact_test.sh) without cutting a release.
#
# Naming is pinned in lockstep with crates/roost-ipc/src/bootstrap.rs's
# `asset_names_are_versioned_per_arch_and_github_safe` test — if either side
# changes the asset-name shape, update the other and re-run both.
#
# Usage:
#   ./linux/scripts/stage-session-artifact.sh <version> <arch> [--dist-dir <dir>]
#
# <version> is the CARGO_PKG_VERSION-shaped string (e.g. 0.0.19, or
# 0.0.19-rc1 for a prerelease tag) — this script does not judge whether it
# is stable; only the client's URL construction cares about that
# (bootstrap.rs `is_stable_version`). <arch> is "amd64" or "arm64", matching
# release.yml's Linux matrix. `--dist-dir` overrides where the source binary
# is read from and the outputs are written (default: <repo-root>/dist) — the
# test seam.
#
# Prints the two produced paths, one per line: the binary, then its .sha256
# sidecar.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=linux/scripts/_common.sh
. "${SCRIPT_DIR}/_common.sh"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"

USAGE="usage: $(basename "$0") <version> <arch> [--dist-dir <dir>]"
usage() { printf '%s\n' "${USAGE}"; }

version=""
arch=""
dist_dir=""
while [ "$#" -gt 0 ]; do
  case "$1" in
    -h|--help)
      usage
      exit 0
      ;;
    --dist-dir)
      [ "$#" -ge 2 ] || { usage >&2; die "--dist-dir requires a value"; }
      [ -n "$2" ] || { usage >&2; die "--dist-dir requires a non-empty value"; }
      dist_dir="$2"
      shift 2
      ;;
    -*)
      usage >&2
      die "unknown flag: $1"
      ;;
    *)
      if [ -z "${version}" ]; then
        version="$1"
      elif [ -z "${arch}" ]; then
        arch="$1"
      else
        usage >&2
        die "unexpected extra argument: $1"
      fi
      shift
      ;;
  esac
done

[ -n "${version}" ] || { usage >&2; die "missing <version>"; }
[ -n "${arch}" ] || { usage >&2; die "missing <arch>"; }
case "${arch}" in
  amd64|arm64) ;;
  *) die "unsupported <arch> '${arch}' — expected amd64 or arm64" ;;
esac

require_tools install

# Select a checksum tool at runtime rather than hard-requiring sha256sum:
# older macOS ships no sha256sum on PATH, but shasum -a 256 has always been
# there and emits the identical two-field `<hash>  <name>` format, so it's
# a safe drop-in fallback.
if command -v sha256sum >/dev/null 2>&1; then
  checksum_cmd=(sha256sum)
elif command -v shasum >/dev/null 2>&1; then
  checksum_cmd=(shasum -a 256)
else
  die "no checksum tool found on PATH — need sha256sum or shasum (shasum -a 256)."
fi

[ -n "${dist_dir}" ] || dist_dir="${REPO_ROOT}/dist"
[ -d "${dist_dir}" ] || die "dist dir not found: ${dist_dir} — run linux/scripts/build-deb.sh first (it stages roost-session there alongside the UI and roostctl)."
dist_dir="$(cd "${dist_dir}" && pwd)"

src="${dist_dir}/roost-session"
[ -f "${src}" ] || die "${src} not found — run linux/scripts/build-deb.sh first (it stages roost-session there alongside the UI and roostctl)."

asset_name="roost-session-${version}-linux-${arch}"
checksum_name="${asset_name}.sha256"
asset_path="${dist_dir}/${asset_name}"
checksum_path="${dist_dir}/${checksum_name}"

install -m 0755 -- "${src}" "${asset_path}"

# Computed with cwd = the asset's own directory so the checksum tool's
# filename field is the BARE asset name, no directory component. The
# client's parse_checksum_file (bootstrap.rs) requires that field to equal
# the requested asset name exactly.
( cd "${dist_dir}" && "${checksum_cmd[@]}" -- "${asset_name}" ) > "${checksum_path}"

printf '%s\n' "${asset_path}"
printf '%s\n' "${checksum_path}"
