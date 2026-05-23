#!/usr/bin/env bash
# Roost.app bundling — Phase 6a M7.
#
# Wraps the SwiftPM executable output into a proper macOS .app bundle
# so the binary can be Finder-launched / Dock-pinned / referenced by
# its bundle identifier instead of being run from the SwiftPM build
# tree like a CLI.
#
# What this script does:
#   1. Builds the Roost executable in the requested configuration
#      (default: release).
#   2. Assembles `mac/build/Roost.app` with the standard macOS bundle
#      layout — Contents/MacOS/Roost, Contents/Info.plist,
#      Contents/Resources/.
#   3. Substitutes @VERSION@ in `mac/Resources/Info.plist.template`
#      with the value of $ROOST_VERSION (or the project default).
#   4. Copies the SwiftPM-generated `Roost_Roost.bundle` (the
#      themes resource bundle that `Bundle.module` reads from) and
#      any other bundles the swift build path produced into
#      Contents/Resources/.
#   5. Copies an .icns icon if present at
#      `mac/Resources/AppIcon.icns`. (M7 ships an empty
#      `AppIcon/` placeholder — `iconutil` can be run separately to
#      build the real .icns from a PNG.)
#
# What this script does NOT do (Phase 8 follow-ups, intentional):
#   * Code-sign with a Developer ID certificate.
#   * Notarize via `notarytool`.
#   * Build a DMG.
#   * Wire Sparkle's auto-update feed.
#
# Usage:
#   ./mac/scripts/bundle.sh                 # release build
#   ./mac/scripts/bundle.sh debug           # debug build
#   ROOST_VERSION=0.2.0 ./mac/scripts/bundle.sh
#
#   open mac/build/Roost.app                # launch the bundle

set -euo pipefail

CONFIG="${1:-release}"
case "${CONFIG}" in
  release|debug) ;;
  *)
    echo "error: configuration must be 'release' or 'debug', got '${CONFIG}'" >&2
    exit 1
    ;;
esac

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
MAC_DIR="$(cd "${SCRIPT_DIR}/.." && pwd)"
REPO_ROOT="$(cd "${MAC_DIR}/.." && pwd)"

VERSION="${ROOST_VERSION:-0.1.0}"
APP_NAME="Roost"
BUNDLE_ID="ai.stridelabs.Roost"
TEMPLATE_PLIST="${MAC_DIR}/Resources/Info.plist.template"
ICON_SRC="${MAC_DIR}/Resources/AppIcon.icns"

OUT_DIR="${MAC_DIR}/build"
APP_DIR="${OUT_DIR}/${APP_NAME}.app"

# Sanity check: the static libghostty-vt archive must exist or
# `swift build` will fail at the linker. The same precondition the
# Mac README documents.
if [ ! -f "${REPO_ROOT}/third_party/ghostty/out/lib/libghostty-vt.a" ]; then
  echo "error: libghostty-vt static archive not built." >&2
  echo "       Run: ${REPO_ROOT}/third_party/ghostty/build.sh" >&2
  exit 1
fi

echo "==> Building Roost (${CONFIG}) from SwiftPM"
pushd "${MAC_DIR}" >/dev/null
swift build -c "${CONFIG}" --product Roost
popd >/dev/null

# Discover the SwiftPM bin path dynamically (same call reused for the resource
# bundles below) rather than hardcoding `arm64-apple-macosx`, so the script
# works regardless of the toolchain's target triple.
SWIFT_BIN_DIR="$(cd "${MAC_DIR}" && swift build -c "${CONFIG}" --show-bin-path)"
SWIFT_BUILD_BIN="${SWIFT_BIN_DIR}/Roost"
if [ ! -x "${SWIFT_BUILD_BIN}" ]; then
  echo "error: swift build did not produce ${SWIFT_BUILD_BIN}" >&2
  exit 1
fi

echo "==> Assembling ${APP_DIR}"
rm -rf "${APP_DIR}"
mkdir -p "${APP_DIR}/Contents/MacOS"
mkdir -p "${APP_DIR}/Contents/Resources"

cp "${SWIFT_BUILD_BIN}" "${APP_DIR}/Contents/MacOS/${APP_NAME}"
chmod +x "${APP_DIR}/Contents/MacOS/${APP_NAME}"

# Info.plist with version substitution. `sed -e s/.../.../g` is
# portable across BSD + GNU sed; quoting `@VERSION@` and using a
# unique-enough sentinel keeps the substitution unambiguous.
echo "==> Stamping Info.plist (version=${VERSION})"
sed -e "s/@VERSION@/${VERSION}/g" "${TEMPLATE_PLIST}" \
  > "${APP_DIR}/Contents/Info.plist"

# Classic four-byte PkgInfo so Finder recognizes the bundle type
# without leaning on Info.plist alone. macOS tolerates a missing
# PkgInfo nowadays but Spotlight prefers it.
printf "APPL????" > "${APP_DIR}/Contents/PkgInfo"

# Resource bundles SwiftPM emits — Bundle.module reads from these,
# so the .app needs to ship them alongside the binary. The
# `Roost_Roost.bundle` carries our theme files (Resources/themes/).
#
# Discover the SwiftPM bin path dynamically. The prior hardcoded
# `arm64-apple-macosx` path failed on Intel macOS runners
# (Phase 8 release-CI matrix includes x86_64). `swift build
# --show-bin-path` prints the exact directory containing the
# built artifacts for the current toolchain + target triple +
# config.
echo "==> Copying SwiftPM resource bundles"
for bundle in "${SWIFT_BIN_DIR}"/*.bundle; do
  [ -d "${bundle}" ] || continue
  cp -R "${bundle}" "${APP_DIR}/Contents/Resources/"
done

# Optional .icns. The script doesn't synthesize one — drop a real
# icon at mac/Resources/AppIcon.icns (build via `iconutil -c icns`
# from an iconset directory) and rerun.
if [ -f "${ICON_SRC}" ]; then
  echo "==> Including AppIcon.icns"
  cp "${ICON_SRC}" "${APP_DIR}/Contents/Resources/AppIcon.icns"
else
  echo "==> No mac/Resources/AppIcon.icns; bundle ships without a custom icon"
fi

# M8: embed roostctl under Contents/Resources/bin/ so `claude
# install` invoked from inside Roost.app writes hook paths that
# point at the bundled binary, not a dev-machine target/ path.
# The CLI build is fast and tracked through the same Cargo cache as
# any cargo build invocation; rebuilding here keeps the bundle in
# lockstep with whatever roost-cli source the developer has
# checked out.
# Discover `cargo` on PATH instead of hardcoding ~/.cargo/bin/cargo.
# Phase 8 release runners may have cargo at a different prefix
# (toolchain managed by mise / rustup / system package). Falling
# back to the literal path preserves the prior behavior for the
# common dev case.
CARGO_BIN="$(command -v cargo || true)"
if [ -z "${CARGO_BIN}" ] && [ -x "${HOME}/.cargo/bin/cargo" ]; then
  CARGO_BIN="${HOME}/.cargo/bin/cargo"
fi
if [ -z "${CARGO_BIN}" ]; then
  echo "error: cargo not found on PATH or at ~/.cargo/bin/cargo" >&2
  exit 1
fi

CARGO_PROFILE_FLAG="--release"
CARGO_PROFILE_DIR="release"
if [ "${CONFIG}" = "debug" ]; then
  CARGO_PROFILE_FLAG=""
  CARGO_PROFILE_DIR="debug"
fi
echo "==> Building roostctl (cargo build -p roost-cli --${CARGO_PROFILE_DIR})"
(
  cd "${REPO_ROOT}"
  # shellcheck disable=SC2086
  "${CARGO_BIN}" build -p roost-cli ${CARGO_PROFILE_FLAG}
)

# Respect CARGO_TARGET_DIR for the artifact-discovery step. Shared
# caches (e.g. sccache + CI matrices that fan out across configs)
# routinely override the default `<repo>/target/` location.
CARGO_TARGET="${CARGO_TARGET_DIR:-${REPO_ROOT}/target}"
ROOSTCTL_SRC="${CARGO_TARGET}/${CARGO_PROFILE_DIR}/roostctl"
if [ ! -x "${ROOSTCTL_SRC}" ]; then
  echo "error: cargo build did not produce ${ROOSTCTL_SRC}" >&2
  exit 1
fi
mkdir -p "${APP_DIR}/Contents/Resources/bin"
cp "${ROOSTCTL_SRC}" "${APP_DIR}/Contents/Resources/bin/roostctl"
chmod +x "${APP_DIR}/Contents/Resources/bin/roostctl"
echo "    Embedded: ${APP_DIR}/Contents/Resources/bin/roostctl"

# Signing. When ROOST_DEVELOPER_ID_IDENTITY is set (release CI, or a dev who
# holds the cert) we sign with that Developer ID + a secure `--timestamp` so the
# bundle can be notarized. Otherwise we fall back to ad-hoc (`-`) signing: fine
# for local launch, but Gatekeeper will warn and notarization is impossible.
# The inner→outer order (embedded roostctl first, then the .app) is required —
# codesign seals nested code into the outer signature.
#
# Failure handling: a botched signature is release-blocking (Gatekeeper reject,
# notarization fail, quarantined installs). Default is fail hard; the
# `ROOST_ALLOW_UNSIGNED=1` env var bypasses for the rare dev case where Xcode
# CLT codesign is missing.
ENT_FILE="${MAC_DIR}/Resources/Roost.entitlements"
SIGN_IDENTITY="${ROOST_DEVELOPER_ID_IDENTITY:--}"
# `--timestamp` only with a real identity; ad-hoc signing can't be timestamped.
# Kept as a plain (unquoted-on-use) string so it expands to nothing when empty —
# bash 3.2-safe (no empty-array expansion under `set -u`).
TS_FLAG=""
if [ "${SIGN_IDENTITY}" != "-" ]; then
  TS_FLAG="--timestamp"
fi
if command -v codesign >/dev/null 2>&1 && [ -f "${ENT_FILE}" ]; then
  if [ "${SIGN_IDENTITY}" = "-" ]; then
    echo "==> Ad-hoc codesign (set ROOST_DEVELOPER_ID_IDENTITY for a notarizable build)"
  else
    echo "==> Developer ID codesign (identity: ${SIGN_IDENTITY})"
  fi
  codesign_or_die() {
    local target="$1"
    # shellcheck disable=SC2086  # TS_FLAG must word-split (empty => no flag)
    if codesign --force --sign "${SIGN_IDENTITY}" \
         --entitlements "${ENT_FILE}" \
         --options runtime \
         ${TS_FLAG} \
         "${target}"
    then
      return 0
    fi
    if [ "${ROOST_ALLOW_UNSIGNED:-0}" = "1" ]; then
      echo "    warn: codesign(${target}) failed; ROOST_ALLOW_UNSIGNED=1 set, continuing"
      return 0
    fi
    echo "    error: codesign(${target}) failed (set ROOST_ALLOW_UNSIGNED=1 to bypass)" >&2
    exit 1
  }
  codesign_or_die "${APP_DIR}/Contents/Resources/bin/roostctl"
  codesign_or_die "${APP_DIR}"
elif [ ! -f "${ENT_FILE}" ]; then
  echo "==> No entitlements file at ${ENT_FILE}; skipping codesign"
fi

echo "==> Bundled: ${APP_DIR}"
echo "    Bundle ID:    ${BUNDLE_ID}"
echo "    Version:      ${VERSION}"
echo "    Executable:   ${APP_DIR}/Contents/MacOS/${APP_NAME}"
echo "    Embedded CLI: ${APP_DIR}/Contents/Resources/bin/roostctl"
echo
echo "Launch with: open '${APP_DIR}'"
