#!/usr/bin/env bash
# Package a .app bundle into a drag-install DMG.
#
# Defaults to mac/build/Roost.app -> mac/build/Roost-<version>.dmg, containing
# Roost.app + an /Applications symlink (drag-to-install). Set
# ROOST_DMG_APP_DIR / ROOST_DMG_BASENAME to package a different bundle (e.g.
# the iced build) — every name-hardcoded site below derives from the app
# bundle's basename, so an override never contaminates the output with
# "Roost" naming (an iced DMG must contain Roost-Iced.app, never Roost.app).
#
# Defaults to `hdiutil` — it's headless-safe and never hangs on Finder
# AppleScript, which matters on GitHub's GUI-less macOS runners. Set
# ROOST_DMG_FANCY=1 (local/manual builds) to use `create-dmg` for a styled
# window with positioned icons.
#
# Usage:
#   ./mac/scripts/make-dmg.sh 0.0.1
#   ROOST_VERSION=0.0.1 ./mac/scripts/make-dmg.sh
#   ROOST_DMG_APP_DIR=mac/build/Roost-Iced.app ROOST_DMG_BASENAME=Roost-Iced-0.0.1 \
#     ./mac/scripts/make-dmg.sh 0.0.1
set -euo pipefail

VERSION="${1:-${ROOST_VERSION:-0.0.0}}"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
MAC_DIR="$(cd "${SCRIPT_DIR}/.." && pwd)"
APP_DIR="${ROOST_DMG_APP_DIR:-${MAC_DIR}/build/Roost.app}"
APP_BASE="$(basename "${APP_DIR}")"
APP_NAME="${APP_BASE%.app}"
OUT_DIR="${MAC_DIR}/build"
DMG_BASENAME="${ROOST_DMG_BASENAME:-${APP_NAME}-${VERSION}}"
DMG_OUT="${OUT_DIR}/${DMG_BASENAME}.dmg"

if [ ! -d "${APP_DIR}" ]; then
  echo "error: ${APP_DIR} not found — run mac/scripts/bundle.sh (or bundle-iced.sh) release first" >&2
  exit 1
fi

rm -f "${DMG_OUT}"
STAGING="$(mktemp -d)"
trap 'rm -rf "${STAGING}"' EXIT
cp -R "${APP_DIR}" "${STAGING}/${APP_BASE}"

# First-launch note for the ad-hoc / non-notarized interim (issue #83). It sits
# beside the app in the mounted DMG so the Gatekeeper-bypass step is visible
# before the user hits the wall. Gated on ROOST_DEVELOPER_ID_IDENTITY (the same
# signal bundle.sh uses to pick ad-hoc vs Developer ID): once a real identity is
# present the build is on the notarization path and the note is omitted.
#
# The heredoc is UNQUOTED so ${APP_NAME}/${APP_BASE} interpolate: any $,
# backtick, or backslash added to the prose below will expand at runtime.
if [ -z "${ROOST_DEVELOPER_ID_IDENTITY:-}" ]; then
  cat > "${STAGING}/FIRST-LAUNCH.txt" <<EOF
${APP_NAME} — first launch on macOS

${APP_NAME} is ad-hoc-signed but not yet notarized (pending an Apple Developer
account), so macOS Gatekeeper blocks the first launch. You only need to do
this once.

Easiest (works on every supported macOS): after dragging ${APP_NAME} into the
Applications folder, run this once in Terminal, then open ${APP_NAME} normally:

    xattr -dr com.apple.quarantine /Applications/${APP_BASE}

Or via the GUI (macOS 15+): double-click ${APP_NAME}, dismiss the "Apple could not
verify…" warning, then open System Settings -> Privacy & Security, scroll to
the message about ${APP_NAME}, and click "Open Anyway". The older right-click -> Open
shortcut no longer bypasses Gatekeeper on macOS 15+ (Roost's minimum).

Once a notarized build ships, this goes away and ${APP_NAME} opens with a normal
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
         -volname "${APP_NAME} ${VERSION}" \
         -srcfolder "${STAGING}" \
         -ov -format UDZO \
         "${DMG_OUT}" >/dev/null; then
      return 0
    fi
    # Drop any partial/corrupt image on every failure — notarize.sh accepts a
    # target by existence alone, so a leftover must not survive the final attempt.
    rm -f "${DMG_OUT}"
    if [ "${attempt}" -eq 5 ]; then
      echo "error: hdiutil create failed after ${attempt} attempts" >&2
      return 1
    fi
    echo "    hdiutil create failed (attempt ${attempt}); retrying in 3s…" >&2
    sleep 3
  done
}

if [ "${ROOST_DMG_FANCY:-0}" = "1" ] && command -v create-dmg >/dev/null 2>&1; then
  echo "==> create-dmg (fancy layout)"
  if ! create-dmg \
        --volname "${APP_NAME} ${VERSION}" \
        --window-size 540 380 \
        --icon-size 110 \
        --icon "${APP_BASE}" 140 190 \
        --app-drop-link 400 190 \
        --hide-extension "${APP_BASE}" \
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
