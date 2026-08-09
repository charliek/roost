#!/usr/bin/env bash
# Print the absolute path of THE single .deb in a directory, or fail naming
# exactly what was found instead.
#
# Every caller (smoke, closure check, release upload) wants one concrete path:
# a bare glob would happily hand two files — or an unexpanded pattern — to the
# thing downstream. `--arch` narrows to `*_<arch>.deb`, which is what the
# release upload needs so an arm64 package can never be uploaded under the
# Intel/AMD label.
#
# Usage:
#   ./linux/scripts/resolve-one-deb.sh out
#   ./linux/scripts/resolve-one-deb.sh out --arch amd64
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=linux/scripts/_common.sh
. "${SCRIPT_DIR}/_common.sh"

USAGE="usage: $(basename "$0") <dir> [--arch <arch>]"
usage() { printf '%s\n' "${USAGE}"; }

dir=""
arch=""
while [ "$#" -gt 0 ]; do
  case "$1" in
    -h|--help)
      usage
      exit 0
      ;;
    --arch)
      [ "$#" -ge 2 ] || { usage >&2; die "--arch requires a value"; }
      arch="$2"
      shift 2
      ;;
    -*)
      usage >&2
      die "unknown flag: $1"
      ;;
    *)
      [ -z "${dir}" ] || { usage >&2; die "unexpected extra argument: $1"; }
      dir="$1"
      shift
      ;;
  esac
done

[ -n "${dir}" ] || { usage >&2; die "missing <dir>"; }
[ -d "${dir}" ] || die "not a directory: ${dir}"

abs_dir="$(cd "${dir}" && pwd)"

shopt -s nullglob
if [ -n "${arch}" ]; then
  debs=("${abs_dir}"/*_"${arch}".deb)
  what="*_${arch}.deb"
else
  debs=("${abs_dir}"/*.deb)
  what="*.deb"
fi

if [ "${#debs[@]}" -ne 1 ]; then
  die "expected exactly one ${what} in ${abs_dir}, found ${#debs[@]}: ${debs[*]:-none}"
fi

printf '%s\n' "${debs[0]}"
