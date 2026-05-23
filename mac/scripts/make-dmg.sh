#!/usr/bin/env bash
# Package mac/build/Roost.app into a drag-install DMG.
#
# Output: mac/build/Roost-<version>.dmg, containing Roost.app + an /Applications
# symlink (drag-to-install).
#
# Defaults to `hdiutil` — it's headless-safe and never hangs on Finder
# AppleScript, which matters on GitHub's GUI-less macOS runners. Set
# ROOST_DMG_FANCY=1 (local/manual builds) to use `create-dmg` for a styled
# window with positioned icons.
#
# Usage:
#   ./mac/scripts/make-dmg.sh 0.0.1
#   ROOST_VERSION=0.0.1 ./mac/scripts/make-dmg.sh
set -euo pipefail

VERSION="${1:-${ROOST_VERSION:-0.0.0}}"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
MAC_DIR="$(cd "${SCRIPT_DIR}/.." && pwd)"
APP_DIR="${MAC_DIR}/build/Roost.app"
OUT_DIR="${MAC_DIR}/build"
DMG_OUT="${OUT_DIR}/Roost-${VERSION}.dmg"

if [ ! -d "${APP_DIR}" ]; then
  echo "error: ${APP_DIR} not found — run mac/scripts/bundle.sh release first" >&2
  exit 1
fi

rm -f "${DMG_OUT}"
STAGING="$(mktemp -d)"
trap 'rm -rf "${STAGING}"' EXIT
cp -R "${APP_DIR}" "${STAGING}/Roost.app"

make_with_hdiutil() {
  ln -s /Applications "${STAGING}/Applications"
  hdiutil create \
    -volname "Roost ${VERSION}" \
    -srcfolder "${STAGING}" \
    -ov -format UDZO \
    "${DMG_OUT}" >/dev/null
}

if [ "${ROOST_DMG_FANCY:-0}" = "1" ] && command -v create-dmg >/dev/null 2>&1; then
  echo "==> create-dmg (fancy layout)"
  if ! create-dmg \
        --volname "Roost ${VERSION}" \
        --window-size 540 380 \
        --icon-size 110 \
        --icon "Roost.app" 140 190 \
        --app-drop-link 400 190 \
        --hide-extension "Roost.app" \
        --no-internet-enable \
        "${DMG_OUT}" "${STAGING}"; then
    echo "==> create-dmg failed; falling back to hdiutil"
    rm -f "${DMG_OUT}"
    make_with_hdiutil
  fi
else
  echo "==> hdiutil (headless-safe)"
  make_with_hdiutil
fi

echo "==> DMG: ${DMG_OUT}"
ls -lh "${DMG_OUT}"
