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

SWIFT_BUILD_BIN="${MAC_DIR}/.build/arm64-apple-macosx/${CONFIG}/Roost"
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
echo "==> Copying SwiftPM resource bundles"
BUILD_BUNDLES_DIR="${MAC_DIR}/.build/arm64-apple-macosx/${CONFIG}"
for bundle in "${BUILD_BUNDLES_DIR}"/*.bundle; do
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
echo "==> Building roostctl (cargo build -p roost-cli --${CARGO_PROFILE:-release})"
CARGO_PROFILE_FLAG="--release"
CARGO_PROFILE_DIR="release"
if [ "${CONFIG}" = "debug" ]; then
  CARGO_PROFILE_FLAG=""
  CARGO_PROFILE_DIR="debug"
fi
(
  cd "${REPO_ROOT}"
  # shellcheck disable=SC2086
  ~/.cargo/bin/cargo build -p roost-cli ${CARGO_PROFILE_FLAG}
)
ROOSTCTL_SRC="${REPO_ROOT}/target/${CARGO_PROFILE_DIR}/roostctl"
if [ ! -x "${ROOSTCTL_SRC}" ]; then
  echo "error: cargo build did not produce ${ROOSTCTL_SRC}" >&2
  exit 1
fi
mkdir -p "${APP_DIR}/Contents/Resources/bin"
cp "${ROOSTCTL_SRC}" "${APP_DIR}/Contents/Resources/bin/roostctl"
chmod +x "${APP_DIR}/Contents/Resources/bin/roostctl"
echo "    Embedded: ${APP_DIR}/Contents/Resources/bin/roostctl"

# Phase 8 (notarization) will replace these ad-hoc signatures
# with Developer-ID-signed binaries + a notarized DMG. Until then
# we ad-hoc sign with the embedded entitlements so the local
# launcher stack sees a consistent signature pair on the .app and
# the embedded helper.
#
# Failure handling: a botched signature is a release-blocking
# issue (Gatekeeper rejects the app, notarization will fail,
# user-installed copies can become quarantined). Default is fail
# hard; the `ROOST_ALLOW_UNSIGNED=1` env var bypasses for the
# rare dev case where Xcode CLT codesign is missing. Sub-agent
# review flagged that the prior `|| echo warn ... (continuing)`
# silently masked CI signature regressions.
ENT_FILE="${MAC_DIR}/Resources/Roost.entitlements"
if command -v codesign >/dev/null 2>&1 && [ -f "${ENT_FILE}" ]; then
  echo "==> Ad-hoc codesign (Phase 8 will replace with Developer ID)"
  codesign_or_die() {
    local target="$1"
    if codesign --force --sign - \
         --entitlements "${ENT_FILE}" \
         --options runtime \
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
