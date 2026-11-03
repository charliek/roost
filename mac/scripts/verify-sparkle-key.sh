#!/usr/bin/env bash
# Prove, before a release builds anything, that the Sparkle key pair it is
# about to ship is the one every installed Roost.app trusts:
#
#   * the committed public key is neither the TEST-ONLY fixture key nor the
#     throwaway spike key, and is exactly the pinned production key — the
#     SUPublicEDKey the Swift Roost.app shipped with, so the iced Roost.app
#     that replaces it is signed for the users who already have it;
#   * $SPARKLE_ED_PRIVATE_KEY is that key's private half, in either format
#     Sparkle's sign_update reads (a 32-byte seed, or the legacy 96-byte
#     sk(64)||pk(32) layout, whose public half is its trailing 32 bytes);
#   * sign_update, given that private key, produces a signature the
#     committed public key verifies — the same check Sparkle makes on a
#     user's machine, run on a probe file instead of a DMG.
#
# A mismatch anywhere ships an update that installed apps reject, which
# Sparkle reports to users as nothing at all: they simply never update.
#
# $SPARKLE_ED_PRIVATE_KEY is base64 of the key FILE `generate_keys -x`
# wrote, whose own contents are base64 of the key (mac/keys/README.md).
# No key material is ever printed.
#
# Usage:
#   SPARKLE_ED_PRIVATE_KEY=... mac/scripts/verify-sparkle-key.sh <public-key-file>
#
# SPARKLE_SIGN_UPDATE names the sign_update binary (default: the vendored
# third_party/sparkle/out/bin/sign_update that fetch.sh stages). The
# derivation and the probe's verification use `cryptography` through uv.
set -euo pipefail

PINNED_PUBLIC_KEY="P2VexGJQtk6cptdgKi3skMV9+WcmcZMAKjSPrKhylJA="
THROWAWAY_PUBLIC_KEY="9n43stvrJeqWENXHYrisnIhByQ8N0OcjzSWAvhZj4fo="

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"
TEST_FIXTURE="${REPO_ROOT}/tools/roosttest/fixtures/sparkle/TEST-ONLY-public-ed-key.txt"
SIGN_UPDATE="${SPARKLE_SIGN_UPDATE:-${REPO_ROOT}/third_party/sparkle/out/bin/sign_update}"

die() {
  printf 'error: %s\n' "$*" >&2
  exit 1
}

[ $# -eq 1 ] || die "usage: $0 <public-key-file>"
pub_file="$1"

# The pinned uv invocation the release path has always used for this. Run
# with --no-project so a stale uv.lock or an unreachable docs-theme tag
# cannot fail a KEY check with a message about something else.
crypto() {
  uv run --no-project --quiet --with 'cryptography==46.0.3' python3 -c '
import base64, sys
from cryptography.exceptions import InvalidSignature
from cryptography.hazmat.primitives.asymmetric.ed25519 import Ed25519PrivateKey, Ed25519PublicKey
from cryptography.hazmat.primitives.serialization import Encoding, PublicFormat

if sys.argv[1] == "derive":
    with open(sys.argv[2], "rb") as handle:
        seed = handle.read()
    public = Ed25519PrivateKey.from_private_bytes(seed).public_key()
    print(base64.b64encode(public.public_bytes(Encoding.Raw, PublicFormat.Raw)).decode())
elif sys.argv[1] == "verify":
    public = Ed25519PublicKey.from_public_bytes(base64.b64decode(sys.argv[2], validate=True))
    with open(sys.argv[4], "rb") as handle:
        data = handle.read()
    try:
        public.verify(base64.b64decode(sys.argv[3], validate=True), data)
    except InvalidSignature:
        sys.exit(1)
' "$@"
}

[ -s "${pub_file}" ] || die "${pub_file} is missing or empty — commit the production public key (mac/keys/README.md)."
[ -f "${TEST_FIXTURE}" ] || die "${TEST_FIXTURE} not found — the TEST-ONLY guard has nothing to compare against; update this script's path."
if cmp -s "${pub_file}" "${TEST_FIXTURE}"; then
  die "${pub_file} is the TEST-ONLY fixture key — a shipped feed must carry the production public key."
fi
pub_key="$(tr -d '[:space:]' < "${pub_file}")"
if [ "${pub_key}" = "${THROWAWAY_PUBLIC_KEY}" ]; then
  die "${pub_file} is the throwaway spike key (#122) — a shipped feed must carry the production public key."
fi
if [ "${pub_key}" != "${PINNED_PUBLIC_KEY}" ]; then
  die "${pub_file} is not the pinned production key ${PINNED_PUBLIC_KEY}, the SUPublicEDKey every installed Roost.app verifies updates with. An app signed for another key never updates."
fi

[ -n "${SPARKLE_ED_PRIVATE_KEY:-}" ] || die "SPARKLE_ED_PRIVATE_KEY is unset — nothing can sign the appcast entry (mac/keys/README.md)."
[ -x "${SIGN_UPDATE}" ] || die "sign_update not found at ${SIGN_UPDATE} — run third_party/sparkle/fetch.sh first."

work="$(mktemp -d)"
chmod 700 "${work}"
trap 'rm -rf "${work}"' EXIT

# macOS `base64 --decode` exits 0 on garbage, so a mangled secret is caught
# by the byte count rather than by either decode.
printf '%s' "${SPARKLE_ED_PRIVATE_KEY}" | base64 --decode > "${work}/key" 2>/dev/null || true
base64 --decode < "${work}/key" > "${work}/raw" 2>/dev/null || true
raw_bytes="$(wc -c < "${work}/raw" | tr -d ' ')"
case "${raw_bytes}" in
  32)
    derived="$(crypto derive "${work}/raw")" \
      || die "could not derive the public half of SPARKLE_ED_PRIVATE_KEY (is uv on PATH?)."
    [ "${derived}" = "${pub_key}" ] \
      || die "SPARKLE_ED_PRIVATE_KEY is not the private half of ${pub_file}. Signing with it would ship updates no installed Roost.app can verify."
    echo "ok: SPARKLE_ED_PRIVATE_KEY (a 32-byte seed) derives ${pub_file}'s public key"
    ;;
  96)
    legacy_pub="$(tail -c 32 "${work}/raw" | base64 | tr -d '[:space:]')"
    [ "${legacy_pub}" = "${pub_key}" ] \
      || die "SPARKLE_ED_PRIVATE_KEY (a legacy 96-byte key) does not end with ${pub_file}'s public key. Signing with it would ship updates no installed Roost.app can verify."
    echo "ok: SPARKLE_ED_PRIVATE_KEY (a legacy 96-byte key) carries ${pub_file}'s public key"
    ;;
  *)
    die "SPARKLE_ED_PRIVATE_KEY does not decode to a Sparkle ed25519 key (32 or 96 bytes after both base64 layers, got ${raw_bytes}) — set it to \`base64 < <the file generate_keys -x wrote>\` (mac/keys/README.md)."
    ;;
esac
rm -f "${work}/raw"

# The legacy layout's public half is copied, not derived, so only a real
# signature proves its private half belongs to it — and for either layout
# this is the one check that exercises sign_update itself.
head -c 4096 /dev/urandom > "${work}/probe"
"${SIGN_UPDATE}" --ed-key-file "${work}/key" "${work}/probe" > "${work}/sign.txt" \
  || die "sign_update could not sign a probe file with SPARKLE_ED_PRIVATE_KEY."
rm -f "${work}/key"
signature="$(sed -n 's/.*sparkle:edSignature="\([^"]*\)".*/\1/p' "${work}/sign.txt")"
length="$(sed -n 's/.*length="\([0-9]*\)".*/\1/p' "${work}/sign.txt")"
[ -n "${signature}" ] || die "sign_update printed no sparkle:edSignature for the probe."
[ "${length}" = "4096" ] || die "sign_update reported length '${length}' for a 4096-byte probe."
crypto verify "${pub_key}" "${signature}" "${work}/probe" \
  || die "the probe's signature does not verify against ${pub_file}. sign_update is signing for another key."
echo "ok: sign_update's probe signature verifies against ${pub_file}"
