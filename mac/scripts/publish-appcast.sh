#!/usr/bin/env bash
# Publish the Sparkle appcast entry for a Roost release (issue #122).
#
# Why this is a local script, not a CI step: release.yml's CI-driven push to
# main is rejected by main's branch protection (github-actions[bot] isn't an
# admin, so enforce_admins=false doesn't help it — see #136). The maintainer
# runs this locally; their own `git push` bypasses cleanly. Mirrors shed's
# scripts/set-version.sh pattern: file mutation here, commit + push by hand.
#
# Usage:
#   mac/scripts/publish-appcast.sh v0.0.4
#
# Prereqs:
#   * GitHub Release <tag> already exists with Roost-<version>.dmg attached
#     (release.yml's mac job uploads it).
#   * The Sparkle EdDSA key lives in the login keychain under account
#     `roost-release` (override with ROOST_SPARKLE_ACCOUNT).
#   * Sparkle SPM bin tools cached under mac/.build/artifacts/ — any prior
#     `cd mac && swift build` populates them.
#
# Effect: downloads the published DMG, EdDSA-signs it, appends a signed
# <item> to docs/appcast.xml (deduped by version). Leaves the working tree
# dirty so you can review the diff and commit + push yourself.

set -euo pipefail

if [[ $# -ne 1 ]]; then
  echo "usage: $0 <tag>   e.g. $0 v0.0.4" >&2
  exit 2
fi
TAG="$1"
case "$TAG" in
  v[0-9]*) ;;
  *) echo "error: tag must look like v<X.Y.Z[-suffix]> (got '$TAG')" >&2; exit 2 ;;
esac
VER="${TAG#v}"

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"
ACCOUNT="${ROOST_SPARKLE_ACCOUNT:-roost-release}"

# Sparkle ships sign_update + generate_keys in the SPM artifact bundle that
# `swift build` fetches. old_dsa_scripts/sign_update is a legacy shell shim
# with the same name — exclude it.
SIGN_UPDATE="$(find "${REPO_ROOT}/mac/.build/artifacts" -path '*/bin/sign_update' -type f -not -path '*old_dsa*' 2>/dev/null | head -1)"
GENERATE_KEYS="$(find "${REPO_ROOT}/mac/.build/artifacts" -path '*/bin/generate_keys' -type f 2>/dev/null | head -1)"
if [[ -z "${SIGN_UPDATE}" || -z "${GENERATE_KEYS}" ]]; then
  echo "error: Sparkle SPM bin tools not found under mac/.build/artifacts." >&2
  echo "       run: (cd mac && swift build)   then retry" >&2
  exit 1
fi

DMG="Roost-${VER}.dmg"
W="$(mktemp -d)"
chmod 700 "$W"
trap 'rm -rf "$W"' EXIT

echo "==> Downloading ${DMG} from GitHub Release ${TAG}"
if ! gh release download "${TAG}" --pattern "${DMG}" --dir "${W}" >/dev/null 2>&1; then
  echo "error: could not download ${DMG} from release ${TAG} — is the release published?" >&2
  exit 1
fi
[[ -s "${W}/${DMG}" ]] || { echo "error: ${W}/${DMG} is empty after download" >&2; exit 1; }

# Re-export the key from the keychain prompt-free (generate_keys is the
# creating tool, so it has ACL access; sign_update would prompt). The key
# file is restricted to /tmp and wiped immediately after signing.
echo "==> Exporting EdDSA private key from keychain account '${ACCOUNT}'"
if ! "${GENERATE_KEYS}" --account "${ACCOUNT}" -x "${W}/key" >/dev/null 2>&1; then
  echo "error: generate_keys failed — is the '${ACCOUNT}' keychain item present?" >&2
  exit 1
fi
[[ -s "${W}/key" ]] || { echo "error: exported key file is empty" >&2; exit 1; }

echo "==> EdDSA-signing ${DMG}"
"${SIGN_UPDATE}" --ed-key-file "${W}/key" "${W}/${DMG}" > "${W}/sign_update.txt"
rm -f "${W}/key"
echo "    $(cat "${W}/sign_update.txt")"

echo "==> Appending appcast entry"
(
  cd "${REPO_ROOT}"
  ROOST_VERSION="${VER}" ROOST_TAG="${TAG}" ROOST_SIGN_FILE="${W}/sign_update.txt" \
    python3 mac/scripts/update-appcast.py
)
(cd "${REPO_ROOT}" && xmllint --noout docs/appcast.xml)

cat <<NEXT

✓ docs/appcast.xml updated for ${TAG}. Review and commit yourself:

    git diff docs/appcast.xml
    git add docs/appcast.xml
    git commit -m "chore(appcast): publish ${TAG} (#122)"
    git push origin main

(docs.yml redeploys https://charliek.github.io/roost/appcast.xml on push.)
NEXT
