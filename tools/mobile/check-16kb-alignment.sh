#!/usr/bin/env bash
# Google Play's 16 KB page-size rule: every ELF LOAD segment of every native
# library must be aligned to at least 0x4000, or the app does not load on a
# 16 KB-page device. Fails if any is not.
#
#   tools/mobile/check-16kb-alignment.sh <apk|aab|so>...
#
# An .apk or .aab is checked library by library (lib/<abi>/*.so, or
# base/lib/<abi>/*.so in a bundle), and must hold exactly the two ABIs Roost
# ships, arm64-v8a and x86_64 (libghostty-vt is built for no others); any other
# ABI directory is a packaging regression and fails too. Every library counts,
# not only ours: libflutter.so or a plugin's would fail Play's check the same.
# An .apk is also run through `zipalign -c -P 16` when zipalign is on PATH or
# under an SDK's build-tools, since an uncompressed library must sit at a
# 16 KB offset in the zip as well. A bare .so is checked as given.
set -euo pipefail

MIN_ALIGN=16384
ABIS=(arm64-v8a x86_64)

if [ "$#" -eq 0 ]; then
  echo "usage: $0 <apk|aab|so>..." >&2
  exit 2
fi

# The SDK roots tools/mobile/android-env.sh looks in, the Linux and macOS
# defaults last.
sdk_roots() {
  local root
  for root in "${ANDROID_HOME:-}" "${ANDROID_SDK_ROOT:-}" \
    "${HOME}/Android/Sdk" "${HOME}/Library/Android/sdk"; do
    if [ -n "${root}" ] && [ -d "${root}" ]; then echo "${root}"; fi
  done
}

# readelf from PATH, else the NDK's llvm-readelf: macOS has no readelf.
readelf_bin() {
  local tool ndk root found
  for tool in llvm-readelf readelf; do
    if command -v "${tool}" >/dev/null 2>&1; then
      command -v "${tool}"
      return
    fi
  done
  for ndk in "${ANDROID_NDK_HOME:-}" "${ANDROID_NDK_ROOT:-}"; do
    if [ -z "${ndk}" ] || [ ! -d "${ndk}/toolchains" ]; then continue; fi
    found="$(find "${ndk}/toolchains" -path '*/llvm/prebuilt/*/bin/llvm-readelf' | head -n 1)"
    if [ -n "${found}" ]; then
      echo "${found}"
      return
    fi
  done
  while read -r root; do
    [ -d "${root}/ndk" ] || continue
    found="$(find "${root}/ndk" -maxdepth 7 -path '*/toolchains/llvm/prebuilt/*/bin/llvm-readelf' |
      sort -V | tail -n 1)"
    if [ -n "${found}" ]; then
      echo "${found}"
      return
    fi
  done < <(sdk_roots)
}

readelf_path="$(readelf_bin)"
if [ -z "${readelf_path}" ]; then
  echo "FAIL: no llvm-readelf or readelf on PATH or in an Android NDK" >&2
  exit 2
fi
readelf=("${readelf_path}" -l -W)

zipalign_bin() {
  local root found
  if command -v zipalign >/dev/null 2>&1; then
    command -v zipalign
    return
  fi
  while read -r root; do
    [ -d "${root}/build-tools" ] || continue
    found="$(find "${root}/build-tools" -mindepth 2 -maxdepth 2 -name zipalign -type f | sort -V | tail -n 1)"
    if [ -n "${found}" ]; then
      echo "${found}"
      return
    fi
  done < <(sdk_roots)
}

failed=0

# One library: every LOAD segment's alignment on one line, failing it if any
# is below MIN_ALIGN.
check_so() {
  local so="$1" label="$2" aligns align low=0
  aligns="$("${readelf[@]}" "${so}" | awk '$1 == "LOAD" { print $NF }')" || {
    echo "FAIL ${label}: not an ELF file readelf can read" >&2
    failed=1
    return
  }
  if [ -z "${aligns}" ]; then
    echo "FAIL ${label}: no LOAD segments" >&2
    failed=1
    return
  fi
  while read -r align; do
    [ $((align)) -ge "${MIN_ALIGN}" ] || low=1
  done <<<"${aligns}"
  aligns="$(tr '\n' ' ' <<<"${aligns}")"
  aligns="${aligns% }"
  if [ "${low}" -eq 1 ]; then
    echo "FAIL ${label}: LOAD align ${aligns}; every one must be >= 0x4000" >&2
    failed=1
  else
    echo "ok   ${label}: LOAD align ${aligns}"
  fi
}

work="$(mktemp -d)"
trap 'rm -rf -- "${work}"' EXIT
extracted=0

check_archive() {
  local archive="$1" into libs lib_root abi so found zipalign
  extracted=$((extracted + 1))
  into="${work}/${extracted}"
  case "${archive}" in
    *.aab) libs="base/lib" ;;
    *) libs="lib" ;;
  esac
  lib_root="${into}/${libs}"
  # Exit 11 is "nothing matched": no libraries, reported just below.
  unzip -q "${archive}" "${libs}/*" -d "${into}" || [ "$?" -eq 11 ] || {
    echo "FAIL ${archive}: unzip could not read it" >&2
    failed=1
    return
  }
  if [ ! -d "${lib_root}" ]; then
    echo "FAIL ${archive}: no native libraries in it" >&2
    failed=1
    return
  fi
  for abi in "${ABIS[@]}"; do
    [ -d "${lib_root}/${abi}" ] || {
      echo "FAIL ${archive}: no ${abi} libraries" >&2
      failed=1
    }
  done
  for dir in "${lib_root}"/*/; do
    abi="$(basename "${dir}")"
    case " ${ABIS[*]} " in
      *" ${abi} "*) ;;
      *)
        echo "FAIL ${archive}: carries ${abi}, which Roost does not ship (abiFilters)" >&2
        failed=1
        continue
        ;;
    esac
    found=0
    for so in "${dir}"*.so; do
      [ -f "${so}" ] || continue
      found=1
      check_so "${so}" "$(basename "${archive}"):${abi}/$(basename "${so}")"
    done
    [ "${found}" -eq 1 ] || {
      echo "FAIL ${archive}: ${abi}/ holds no .so" >&2
      failed=1
    }
  done
  case "${archive}" in
    *.apk)
      zipalign="$(zipalign_bin)"
      if [ -z "${zipalign}" ]; then
        echo "note ${archive}: no zipalign found; zip offsets not checked"
      elif "${zipalign}" -c -P 16 4 "${archive}"; then
        echo "ok   ${archive}: zipalign -P 16"
      else
        echo "FAIL ${archive}: an uncompressed library is not 16 KB-aligned in the zip" >&2
        failed=1
      fi
      ;;
  esac
}

for arg in "$@"; do
  if [ ! -f "${arg}" ]; then
    echo "FAIL ${arg}: no such file" >&2
    failed=1
    continue
  fi
  case "${arg}" in
    *.apk | *.aab) check_archive "${arg}" ;;
    *) check_so "${arg}" "${arg}" ;;
  esac
done

if [ "${failed}" -ne 0 ]; then
  echo "FAIL: not every native library is 16 KB page-aligned" >&2
  exit 1
fi
echo "OK: every native library is 16 KB page-aligned"
