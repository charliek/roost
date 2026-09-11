#!/usr/bin/env bash
# Notarize + staple a Roost DMG or .app bundle. (notarytool needs an
# archive — a bare .app must be zipped first; the DMG form submits the
# image itself.)
#
# No-op (exit 0) when no credentials are configured, so the release pipeline
# still ships an UNSIGNED artifact until an Apple Developer account is
# available. Wire it up later by adding the secrets below — this script then
# activates with no other changes.
#
# Credentials (either form):
#   * ROOST_NOTARY_PROFILE  — a stored notarytool keychain profile
#       (local: `xcrun notarytool store-credentials <name> --apple-id … --team-id … --password …`)
#   * APPLE_ID + APPLE_TEAM_ID + APPLE_APP_SPECIFIC_PASSWORD  — CI secrets
#
# Usage:
#   ./mac/scripts/notarize.sh mac/build/Roost-0.0.1.dmg
#   ./mac/scripts/notarize.sh --app mac/build/Roost.app
set -euo pipefail

MODE="dmg"
if [ "${1:-}" = "--app" ]; then
  MODE="app"
  shift
fi

TARGET="${1:-}"
if [ -z "${TARGET}" ] || [ ! -e "${TARGET}" ]; then
  if [ "${MODE}" = "app" ]; then
    echo "usage: $0 --app <path-to.app>" >&2
  else
    echo "usage: $0 <path-to-dmg-or-archive>" >&2
  fi
  exit 1
fi

if [ -n "${ROOST_NOTARY_PROFILE:-}" ]; then
  AUTH=(--keychain-profile "${ROOST_NOTARY_PROFILE}")
elif [ -n "${APPLE_ID:-}" ] && [ -n "${APPLE_TEAM_ID:-}" ] && [ -n "${APPLE_APP_SPECIFIC_PASSWORD:-}" ]; then
  AUTH=(--apple-id "${APPLE_ID}" --team-id "${APPLE_TEAM_ID}" --password "${APPLE_APP_SPECIFIC_PASSWORD}")
else
  echo "==> notarize: no credentials set — skipping (DMG ships UNSIGNED)."
  echo "    To enable: set ROOST_NOTARY_PROFILE, or APPLE_ID + APPLE_TEAM_ID +"
  echo "    APPLE_APP_SPECIFIC_PASSWORD, then re-run."
  echo "    Until then, users clear Gatekeeper once after install with:"
  echo "      xattr -dr com.apple.quarantine /Applications/Roost.app"
  echo "    (or System Settings > Privacy & Security > Open Anyway)."
  exit 0
fi

if [ "${MODE}" = "app" ]; then
  # BSD `mktemp` only substitutes the X's when they END the template, so a
  # `roost-notarize-XXXXXX.zip` template produces that name *literally* on
  # macOS — which is the only place this runs. Take the uniqueness from a
  # directory, which BSD and GNU spell the same way, and put a plainly
  # named zip inside it.
  ZIP_DIR="$(mktemp -d)"
  ZIP="${ZIP_DIR}/$(basename "${TARGET}").zip"
  trap 'rm -rf "${ZIP_DIR}"' EXIT

  echo "==> ditto: zipping ${TARGET} for submission…"
  ditto -c -k --keepParent "${TARGET}" "${ZIP}"

  echo "==> notarytool submit (waits for Apple; usually a few minutes)…"
  xcrun notarytool submit "${ZIP}" "${AUTH[@]}" --wait

  echo "==> stapler staple"
  xcrun stapler staple "${TARGET}"
  xcrun stapler validate "${TARGET}"
  echo "==> Notarized + stapled: ${TARGET}"
else
  echo "==> notarytool submit (waits for Apple; usually a few minutes)…"
  xcrun notarytool submit "${TARGET}" "${AUTH[@]}" --wait

  echo "==> stapler staple"
  xcrun stapler staple "${TARGET}"
  xcrun stapler validate "${TARGET}"
  echo "==> Notarized + stapled: ${TARGET}"
fi
