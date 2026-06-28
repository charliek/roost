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

# First-launch note for the ad-hoc / non-notarized interim (issue #83). It sits
# beside Roost.app in the mounted DMG so the Gatekeeper-bypass step is visible
# before the user hits the wall. Gated on ROOST_DEVELOPER_ID_IDENTITY (the same
# signal bundle.sh uses to pick ad-hoc vs Developer ID): once a real identity is
# present the build is on the notarization path and the note is omitted.
if [ -z "${ROOST_DEVELOPER_ID_IDENTITY:-}" ]; then
  cat > "${STAGING}/FIRST-LAUNCH.txt" <<'EOF'
Roost — first launch on macOS

Roost is ad-hoc-signed but not yet notarized (pending an Apple Developer
account), so macOS Gatekeeper blocks the first launch. You only need to do
this once.

Easiest (works on every supported macOS): after dragging Roost into the
Applications folder, run this once in Terminal, then open Roost normally:

    xattr -dr com.apple.quarantine /Applications/Roost.app

Or via the GUI (macOS 15+): double-click Roost, dismiss the "Apple could not
verify…" warning, then open System Settings -> Privacy & Security, scroll to
the message about Roost, and click "Open Anyway". The older right-click -> Open
shortcut no longer bypasses Gatekeeper on macOS 15+ (Roost's minimum).

Once a notarized build ships, this goes away and Roost opens with a normal
double-click.
EOF
fi

make_with_hdiutil() {
  ln -s /Applications "${STAGING}/Applications"
  # hdiutil intermittently fails with "Resource busy" on CI runners (transient
  # device/Spotlight contention, often right after codesign touches the bundle —
  # not a real error). Retry a few times before giving up. (Proven needed by
  # shed-desktop's first notarized release.)
  local attempt
  for attempt in 1 2 3 4 5; do
    if hdiutil create \
         -volname "Roost ${VERSION}" \
         -srcfolder "${STAGING}" \
         -ov -format UDZO \
         "${DMG_OUT}" >/dev/null; then
      return 0
    fi
    if [ "${attempt}" -eq 5 ]; then
      echo "error: hdiutil create failed after ${attempt} attempts" >&2
      return 1
    fi
    echo "    hdiutil create failed (attempt ${attempt}); retrying in 3s…" >&2
    rm -f "${DMG_OUT}"
    sleep 3
  done
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
