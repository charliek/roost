#!/usr/bin/env bash
# Notarize + staple a Roost DMG. (notarytool needs an archive — a bare .app
# must be zipped first; the release pipeline always passes the DMG.)
#
# No-op (exit 0) when no credentials are configured, so the release pipeline
# still ships an UNSIGNED DMG until an Apple Developer account is available.
# Wire it up later by adding the secrets below — this script then activates
# with no other changes.
#
# Credentials (either form):
#   * ROOST_NOTARY_PROFILE  — a stored notarytool keychain profile
#       (local: `xcrun notarytool store-credentials <name> --apple-id … --team-id … --password …`)
#   * APPLE_ID + APPLE_TEAM_ID + APPLE_APP_SPECIFIC_PASSWORD  — CI secrets
#
# Usage:
#   ./mac/scripts/notarize.sh mac/build/Roost-0.0.1.dmg
set -euo pipefail

TARGET="${1:-}"
if [ -z "${TARGET}" ] || [ ! -e "${TARGET}" ]; then
  echo "usage: $0 <path-to-dmg-or-archive>" >&2
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

echo "==> notarytool submit (waits for Apple; usually a few minutes)…"
xcrun notarytool submit "${TARGET}" "${AUTH[@]}" --wait

echo "==> stapler staple"
xcrun stapler staple "${TARGET}"
xcrun stapler validate "${TARGET}"
echo "==> Notarized + stapled: ${TARGET}"
