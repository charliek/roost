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

# One work dir for the whole run: the submission zip and notarytool's
# captured output both live here, and one trap removes both.
#
# BSD `mktemp` only substitutes the X's when they END the template, so a
# `roost-notarize-XXXXXX.zip` template produces that name *literally* on
# macOS — which is the only place this runs. Take the uniqueness from a
# directory, which BSD and GNU spell the same way, and put a plainly
# named zip inside it.
WORK_DIR="$(mktemp -d)"
trap 'rm -rf "${WORK_DIR}"' EXIT

# `notarytool submit --wait` exits 0 even when Apple's verdict is Invalid —
# the verdict is in its output, not its exit status. Left unchecked the run
# walks on to `stapler`, which fails with a bare "Record not found" and
# exit 65, hiding the actual reason behind a symptom. Read the verdict, and
# on anything but Accepted print Apple's own issue list before stopping.
submit_or_die() {
  local archive="$1"
  local out="${WORK_DIR}/notarytool-submit.out"
  local rc id status

  echo "==> notarytool submit (waits for Apple; usually a few minutes)…"
  # tee, not command substitution, so a multi-minute wait still streams
  # into the CI log instead of going silent until Apple answers.
  set +e
  xcrun notarytool submit "${archive}" "${AUTH[@]}" --wait 2>&1 | tee "${out}"
  rc="${PIPESTATUS[0]}"
  set -e

  # The progress lines read `Current status: In Progress`, so anchoring on
  # a line that STARTS with `status:` picks the final verdict only.
  id="$(awk '/^[[:space:]]*id:/ { print $2; exit }' "${out}")"
  status="$(awk '/^[[:space:]]*status:/ { s = $2 } END { print s }' "${out}")"

  if [ "${rc}" -ne 0 ] || [ "${status}" != "Accepted" ]; then
    if [ -n "${id}" ]; then
      echo "==> notarytool log ${id} — Apple's own reasons:"
      xcrun notarytool log "${id}" "${AUTH[@]}" || true
    fi
    echo "error: notarization failed for ${archive} (status=${status:-unknown}, notarytool exit ${rc})." >&2
    exit 1
  fi
}

staple_or_die() {
  local bundle="$1"
  echo "==> stapler staple"
  xcrun stapler staple "${bundle}"
  xcrun stapler validate "${bundle}"
  echo "==> Notarized + stapled: ${bundle}"
}

if [ "${MODE}" = "app" ]; then
  ZIP="${WORK_DIR}/$(basename "${TARGET}").zip"
  echo "==> ditto: zipping ${TARGET} for submission…"
  ditto -c -k --keepParent "${TARGET}" "${ZIP}"
  submit_or_die "${ZIP}"
else
  submit_or_die "${TARGET}"
fi

staple_or_die "${TARGET}"
