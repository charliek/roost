#!/usr/bin/env bash
# Vendored Sparkle.framework fetch for the Roost-Iced bundle (M6 6c).
#
# Downloads the official Sparkle release artifact, verifies it against a
# pinned SHA256, and stages what mac/scripts/bundle-iced.sh embeds into
# Roost-Iced.app plus the release tooling the test fixtures use:
#
#   out/Sparkle.framework      — embedded via `cp -R` (symlink farm intact)
#   out/bin/sign_update        — EdDSA appcast-entry signer (test fixtures)
#   out/bin/generate_keys      — EdDSA keypair generator (feed enablement)
#
# Same source-of-truth model as third_party/ghostty/build.sh: the pinned
# version + SHA below are the integrity boundary (network-at-build-time is
# accepted the same way cargo fetches are); everything under out/ is
# gitignored and rebuilt on demand. Idempotent via the version+sha stamp —
# rerunning with an unchanged pin is a no-op, and bumping the pin
# invalidates the stage automatically. CI additionally caches out/ keyed
# on this file's hash so a GitHub release-asset outage can't flake the
# bundle lanes.
#
# See README.roost.md for provenance + the version-choice rationale
# (latest stable 2.9.x, deliberately NOT shed's 2.8.1 pin).
#
# Usage:
#   ./third_party/sparkle/fetch.sh           # fetch + stage (idempotent)
#   ./third_party/sparkle/fetch.sh --force   # discard stage, refetch

set -euo pipefail

# --- Pinned version ----------------------------------------------------------
# Bump deliberately: update BOTH lines, re-run with --force, and refresh
# README.roost.md. The SHA256 is computed from the official release
# artifact (shasum -a 256 Sparkle-<ver>.tar.xz) and re-verified on every
# download.
SPARKLE_VERSION="2.9.5"
SPARKLE_SHA256="015336b601493e05c237964954bff6191370003d94edefe663724c88840d73cc"
SPARKLE_URL="https://github.com/sparkle-project/Sparkle/releases/download/${SPARKLE_VERSION}/Sparkle-${SPARKLE_VERSION}.tar.xz"

# --- Paths -------------------------------------------------------------------
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
OUT_DIR="${SCRIPT_DIR}/out"
STAMP_FILE="${OUT_DIR}/.stamp"
STAMP_WANT="${SPARKLE_VERSION} ${SPARKLE_SHA256}"

force=0
case "${1:-}" in
  --force) force=1 ;;
  "")      ;;
  *)       echo "usage: $0 [--force]"; exit 1 ;;
esac

if [ "${force}" -eq 1 ]; then
  echo "==> --force: removing ${OUT_DIR}"
  rm -rf "${OUT_DIR}"
fi

# validate_stage OUT_DIR
#
# The framework must keep its Versions/ symlink farm intact — codesign
# refuses a framework whose symlinks were flattened, and the runtime
# dlopen path (crates/roost-iced/src/macos/sparkle.rs, 6c) resolves the
# stable top-level `Sparkle.framework/Sparkle` symlink. Validate both the
# symlinks and the concrete components the strict signing chain
# (codesign_sparkle_or_die in mac/scripts/bundle-lib.sh) will sign.
validate_stage() {
  local out="$1"
  local fw="${out}/Sparkle.framework"
  local versions="${fw}/Versions/B"
  local problems=0

  if [ ! -L "${fw}/Sparkle" ] || [ ! -f "${fw}/Sparkle" ]; then
    echo "error: ${fw}/Sparkle is not a resolvable symlink (symlink farm broken?)" >&2
    problems=1
  fi
  local component
  for component in \
    "${versions}/XPCServices/Installer.xpc" \
    "${versions}/XPCServices/Downloader.xpc" \
    "${versions}/Updater.app"
  do
    if [ ! -d "${component}" ]; then
      echo "error: expected Sparkle component missing: ${component}" >&2
      problems=1
    fi
  done
  if [ ! -f "${versions}/Autoupdate" ]; then
    echo "error: expected Sparkle component missing: ${versions}/Autoupdate" >&2
    problems=1
  fi
  local tool
  for tool in sign_update generate_keys; do
    if [ ! -x "${out}/bin/${tool}" ]; then
      echo "error: expected Sparkle tool missing: ${out}/bin/${tool}" >&2
      problems=1
    fi
  done
  return "${problems}"
}

# Cached: stamp matches the pin AND the stage still validates (a manually
# gutted out/ with a stale stamp must refetch, not silently pass).
if [ -f "${STAMP_FILE}" ] && [ "$(cat "${STAMP_FILE}")" = "${STAMP_WANT}" ] \
   && validate_stage "${OUT_DIR}" 2>/dev/null; then
  echo "==> Sparkle ${SPARKLE_VERSION} already staged: ${OUT_DIR} (cached)"
  exit 0
fi

echo "==> Fetching Sparkle ${SPARKLE_VERSION}"
work="$(mktemp -d -t roost-sparkle-fetch)"
trap 'rm -rf "${work}"' EXIT

tarball="${work}/Sparkle-${SPARKLE_VERSION}.tar.xz"
curl -fsSL -o "${tarball}" "${SPARKLE_URL}" || {
  echo "error: download failed: ${SPARKLE_URL}" >&2
  exit 1
}

echo "==> Verifying SHA256"
(cd "${work}" && echo "${SPARKLE_SHA256}  Sparkle-${SPARKLE_VERSION}.tar.xz" \
  | shasum -a 256 -c -) || {
  echo "error: SHA256 mismatch for ${tarball} (expected ${SPARKLE_SHA256})." >&2
  echo "       Refusing to stage an artifact that doesn't match the pin." >&2
  exit 1
}

echo "==> Extracting Sparkle.framework + bin/ tools"
mkdir -p "${work}/extract"
tar -xJf "${tarball}" -C "${work}/extract"

rm -rf "${OUT_DIR}"
mkdir -p "${OUT_DIR}/bin"
# cp -R preserves the Versions/ symlink farm — never flatten it (codesign
# and the dlopen path both depend on it; see validate_stage).
cp -R "${work}/extract/Sparkle.framework" "${OUT_DIR}/Sparkle.framework"
cp "${work}/extract/bin/sign_update" "${OUT_DIR}/bin/sign_update"
cp "${work}/extract/bin/generate_keys" "${OUT_DIR}/bin/generate_keys"
chmod +x "${OUT_DIR}/bin/sign_update" "${OUT_DIR}/bin/generate_keys"

validate_stage "${OUT_DIR}" || {
  echo "error: staged Sparkle layout failed validation; inspect ${OUT_DIR}." >&2
  exit 1
}

printf '%s\n' "${STAMP_WANT}" > "${STAMP_FILE}"

echo "==> Sparkle ${SPARKLE_VERSION} staged: ${OUT_DIR}"
echo "    framework: ${OUT_DIR}/Sparkle.framework"
echo "    tools:     ${OUT_DIR}/bin/{sign_update,generate_keys}"
