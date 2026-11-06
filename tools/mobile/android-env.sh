# shellcheck shell=sh
# Source me: exports the Android NDK toolchain env that cross-building the
# Rust crates for Android needs, for aarch64-linux-android and
# x86_64-linux-android.
#
#   . tools/mobile/android-env.sh
#
# NDK lookup: $ANDROID_NDK_HOME, else $ANDROID_NDK_ROOT, else the newest
# ndk/<version>/ under $ANDROID_HOME, $ANDROID_SDK_ROOT, ~/Android/Sdk or
# ~/Library/Android/sdk. ROOST_ANDROID_API (default 26) picks the API level
# of the clang wrappers used as compiler and linker.
#
# bindgen's sysroot is not exported: crates/roost-vt/build.rs derives it from
# ANDROID_NDK_HOME (or from CC_<triple>, which is all cargokit sets).

roost_android_env() {
  local ndk sdk host_tag bin api triple clang under upper
  ndk="${ANDROID_NDK_HOME:-${ANDROID_NDK_ROOT:-}}"
  if [ -z "${ndk}" ]; then
    for sdk in "${ANDROID_HOME:-}" "${ANDROID_SDK_ROOT:-}" \
      "${HOME}/Android/Sdk" "${HOME}/Library/Android/sdk"; do
      [ -n "${sdk}" ] && [ -d "${sdk}/ndk" ] || continue
      ndk="$(find "${sdk}/ndk" -mindepth 1 -maxdepth 1 -type d | sort -V | tail -n 1)"
      [ -n "${ndk}" ] && break
    done
  fi
  if [ -z "${ndk}" ] || [ ! -d "${ndk}" ]; then
    echo "android-env: no Android NDK found${ndk:+ at ${ndk}}; set ANDROID_NDK_HOME" >&2
    return 1
  fi

  case "$(uname -s)" in
    Linux)  host_tag=linux-x86_64 ;;
    Darwin) host_tag=darwin-x86_64 ;;
    *) echo "android-env: unsupported build host $(uname -s)" >&2; return 1 ;;
  esac
  bin="${ndk}/toolchains/llvm/prebuilt/${host_tag}/bin"
  if [ ! -d "${bin}" ]; then
    echo "android-env: ${bin} does not exist; is ${ndk} a complete NDK?" >&2
    return 1
  fi

  api="${ROOST_ANDROID_API:-26}"
  export ANDROID_NDK_HOME="${ndk}"
  for triple in aarch64-linux-android x86_64-linux-android; do
    clang="${bin}/${triple}${api}-clang"
    if [ ! -x "${clang}" ]; then
      echo "android-env: ${clang} not found (ROOST_ANDROID_API=${api})" >&2
      return 1
    fi
    under="$(printf '%s' "${triple}" | tr '-' '_')"
    upper="$(printf '%s' "${under}" | tr '[:lower:]' '[:upper:]')"
    export "CC_${under}=${clang}"
    export "CXX_${under}=${clang}++"
    export "AR_${under}=${bin}/llvm-ar"
    export "RANLIB_${under}=${bin}/llvm-ranlib"
    export "CARGO_TARGET_${upper}_LINKER=${clang}"
  done
}

roost_android_env
roost_android_env_rc=$?
unset -f roost_android_env
# shellcheck disable=SC2317
return "${roost_android_env_rc}" 2>/dev/null || exit "${roost_android_env_rc}"
