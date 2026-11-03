#!/usr/bin/env bash
# Test mac/scripts/verify-sparkle-key.sh against generated ed25519 keys and a
# fake sign_update, so every refusal and both accepted key layouts are
# provable on Linux without the production private key (plan 089 D3).
#
# The production key cannot sign here, so the accepting cases run a copy of
# the script whose pinned literal is rewritten to a generated key; the copy
# sits in a scratch tree shaped like the repo, so it finds the TEST-ONLY
# fixture where the real one does. Every refusal of the committed key itself
# runs the real script. The other half — the real secret against the real
# key, through the real sign_update — is a run in Actions (plan 089 V6).
#
# The fake sign_update speaks the real one's argv and output line, and signs
# a legacy 96-byte key with the seed it was expanded from (ed25519 signing
# is deterministic, so that is the signature the expanded key would make).
#
# Usage:
#   ./mac/scripts/verify-sparkle-key_test.sh
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"
SCRIPT="${SCRIPT_DIR}/verify-sparkle-key.sh"
COMMITTED_KEY="${REPO_ROOT}/mac/keys/roost-sparkle-ed-public-key.txt"
FIXTURE="${REPO_ROOT}/tools/roosttest/fixtures/sparkle/TEST-ONLY-public-ed-key.txt"
PINNED="P2VexGJQtk6cptdgKi3skMV9+WcmcZMAKjSPrKhylJA="

fail() {
  printf 'FAIL: %s\n' "$*" >&2
  exit 1
}

pass() {
  printf 'ok: %s\n' "$*"
}

py() {
  uv run --no-project --quiet --with 'cryptography==46.0.3' python3 "$@"
}

work="$(mktemp -d)"
trap 'rm -rf "${work}"' EXIT
keys="${work}/keys"
mkdir -p "${keys}" "${work}/bin"

# Three pairs: `good` and `other` are 32-byte seeds, `legacy` is the 96-byte
# sk(64)||pk(32) layout orlp/ed25519 (Sparkle's) expands a seed into. Each
# `.secret` is the secret's own encoding: base64 of the base64 key file.
py - "${keys}" <<'PY'
import base64, hashlib, os, sys
from pathlib import Path
from cryptography.hazmat.primitives.asymmetric.ed25519 import Ed25519PrivateKey
from cryptography.hazmat.primitives.serialization import Encoding, PublicFormat

out = Path(sys.argv[1])

def public(seed):
    key = Ed25519PrivateKey.from_private_bytes(seed).public_key()
    return key.public_bytes(Encoding.Raw, PublicFormat.Raw)

def write(name, seed, key_bytes):
    (out / f"{name}.seed").write_bytes(seed)
    (out / f"{name}.pub").write_text(base64.b64encode(public(seed)).decode() + "\n")
    key_file = base64.b64encode(key_bytes) + b"\n"
    (out / f"{name}.secret").write_text(base64.b64encode(key_file).decode())

for name in ("good", "other"):
    seed = os.urandom(32)
    write(name, seed, seed)

seed = os.urandom(32)
h = bytearray(hashlib.sha512(seed).digest())
h[0] &= 248
h[31] &= 63
h[31] |= 64
write("legacy", seed, bytes(h) + public(seed))
PY

# Signs with the key it is handed, unless FAKE_SIGN_AS names another seed
# (a sign_update signing for a different key). Logs its argv, one per line.
cat > "${work}/bin/sign_update" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
printf '%s\n' "$@" > "${FAKE_SIGN_LOG}"
[ "$1" = "--ed-key-file" ] || { echo "fake sign_update: want --ed-key-file" >&2; exit 64; }
exec uv run --no-project --quiet --with 'cryptography==46.0.3' python3 - "$2" "$3" <<'PY'
import base64, hashlib, os, sys
from cryptography.hazmat.primitives.asymmetric.ed25519 import Ed25519PrivateKey
from cryptography.hazmat.primitives.serialization import Encoding, PublicFormat

key = base64.b64decode(open(sys.argv[1], "rb").read())
if os.environ.get("FAKE_SIGN_AS"):
    seed = open(os.environ["FAKE_SIGN_AS"], "rb").read()
elif len(key) == 32:
    seed = key
elif len(key) == 96:
    seed = open(os.environ["FAKE_SIGN_LEGACY_SEED"], "rb").read()
    expanded = bytearray(hashlib.sha512(seed).digest())
    expanded[0] &= 248
    expanded[31] &= 63
    expanded[31] |= 64
    public = Ed25519PrivateKey.from_private_bytes(seed).public_key().public_bytes(
        Encoding.Raw, PublicFormat.Raw
    )
    if key != bytes(expanded) + public:
        sys.exit("fake sign_update: the legacy key is not the one FAKE_SIGN_LEGACY_SEED expands to")
else:
    sys.exit(f"fake sign_update: a {len(key)}-byte key")
data = open(sys.argv[2], "rb").read()
signature = base64.b64encode(Ed25519PrivateKey.from_private_bytes(seed).sign(data)).decode()
print(f'sparkle:edSignature="{signature}" length="{len(data)}"')
PY
EOF
chmod +x "${work}/bin/sign_update"

# A repo-shaped scratch tree whose script pins `$1`'s public key.
pinned_to() {
  local name="$1"
  local tree="${work}/tree-${name}"
  mkdir -p "${tree}/mac/scripts" "${tree}/tools/roosttest/fixtures/sparkle"
  cp "${FIXTURE}" "${tree}/tools/roosttest/fixtures/sparkle/"
  sed "s|^PINNED_PUBLIC_KEY=\"${PINNED}\"\$|PINNED_PUBLIC_KEY=\"$(tr -d '[:space:]' < "${keys}/${name}.pub")\"|" \
    "${SCRIPT}" > "${tree}/mac/scripts/verify-sparkle-key.sh"
  grep -q "^PINNED_PUBLIC_KEY=\"$(tr -d '[:space:]' < "${keys}/${name}.pub")\"\$" \
    "${tree}/mac/scripts/verify-sparkle-key.sh" \
    || fail "could not re-pin the scratch copy to ${name} (did PINNED_PUBLIC_KEY's line change?)"
  printf '%s\n' "${tree}/mac/scripts/verify-sparkle-key.sh"
}

out="${work}/out"
# run SCRIPT PUBLIC_KEY_FILE SECRET [ENV=VALUE...]: sets $rc; output in $out.
run() {
  local script="$1" pub="$2" secret="$3"
  shift 3
  : > "${work}/sign.log"
  set +e
  env -u FAKE_SIGN_AS -u FAKE_SIGN_LEGACY_SEED \
    SPARKLE_ED_PRIVATE_KEY="${secret}" \
    SPARKLE_SIGN_UPDATE="${work}/bin/sign_update" \
    FAKE_SIGN_LOG="${work}/sign.log" \
    "$@" bash "${script}" "${pub}" > "${out}" 2>&1
  rc=$?
  set -e
}

expect_ok() {
  [ "${rc}" -eq 0 ] || { cat "${out}" >&2; fail "$1: exited ${rc}, want 0"; }
  pass "$1"
}

expect_refusal() {
  local name="$1" needle="$2"
  [ "${rc}" -ne 0 ] || { cat "${out}" >&2; fail "${name}: exited 0, want a refusal"; }
  grep -qF -- "${needle}" "${out}" || { cat "${out}" >&2; fail "${name}: output lacks '${needle}'"; }
  pass "${name}"
}

expect_no_key_material() {
  local name
  for name in good other legacy; do
    if grep -qF -- "$(cat "${keys}/${name}.secret")" "${out}" \
      || grep -qF -- "$(base64 < "${keys}/${name}.seed" | tr -d '[:space:]')" "${out}"; then
      cat "${out}" >&2
      fail "$1: printed key material"
    fi
  done
}

good_script="$(pinned_to good)"
legacy_script="$(pinned_to legacy)"

run "${good_script}" "${keys}/good.pub" "$(cat "${keys}/good.secret")"
expect_ok "a 32-byte seed whose public half is the pinned key is accepted"
grep -qF "probe signature verifies" "${out}" || fail "the accepted seed was never probed"
expect_no_key_material "the accepted seed"
[ "$(sed -n '1p' "${work}/sign.log")" = "--ed-key-file" ] \
  || fail "sign_update was not handed --ed-key-file first: $(cat "${work}/sign.log")"
pass "sign_update is handed the key as --ed-key-file"

run "${legacy_script}" "${keys}/legacy.pub" "$(cat "${keys}/legacy.secret")" \
  FAKE_SIGN_LEGACY_SEED="${keys}/legacy.seed"
expect_ok "a legacy 96-byte key carrying the pinned key is accepted"
grep -qF "legacy 96-byte key" "${out}" || fail "the 96-byte key was not read as the legacy layout"
expect_no_key_material "the accepted legacy key"

run "${good_script}" "${keys}/good.pub" "$(cat "${keys}/other.secret")"
expect_refusal "a 32-byte seed for another key is refused" "is not the private half"
expect_no_key_material "the mismatched seed"

run "${good_script}" "${keys}/good.pub" "$(cat "${keys}/legacy.secret")" \
  FAKE_SIGN_LEGACY_SEED="${keys}/legacy.seed"
expect_refusal "a legacy 96-byte key for another key is refused" "does not end with"

run "${good_script}" "${keys}/good.pub" "$(cat "${keys}/good.secret")" \
  FAKE_SIGN_AS="${keys}/other.seed"
expect_refusal "a sign_update that signs for another key is refused" "does not verify"

run "${good_script}" "${keys}/good.pub" "$(printf 'not a key' | base64)"
expect_refusal "a secret that decodes to no key is refused" "does not decode to a Sparkle ed25519 key"

run "${good_script}" "${keys}/good.pub" ""
expect_refusal "an unset secret is refused" "SPARKLE_ED_PRIVATE_KEY is unset"

run "${good_script}" "${keys}/good.pub" "$(cat "${keys}/good.secret")" \
  SPARKLE_SIGN_UPDATE="${work}/bin/absent-sign_update"
expect_refusal "a missing sign_update is refused" "sign_update not found"

run "${SCRIPT}" "${keys}/good.pub" "$(cat "${keys}/good.secret")"
expect_refusal "a key that is not the pinned production key is refused" "is not the pinned production key"

run "${good_script}" "${FIXTURE}" "$(cat "${keys}/good.secret")"
expect_refusal "the TEST-ONLY fixture key is refused" "is the TEST-ONLY fixture key"

printf '%s\n' "9n43stvrJeqWENXHYrisnIhByQ8N0OcjzSWAvhZj4fo=" > "${work}/throwaway.pub"
run "${good_script}" "${work}/throwaway.pub" "$(cat "${keys}/good.secret")"
expect_refusal "the throwaway spike key is refused" "is the throwaway spike key"

: > "${work}/empty.pub"
run "${good_script}" "${work}/empty.pub" "$(cat "${keys}/good.secret")"
expect_refusal "an empty public key file is refused" "is missing or empty"

# The committed key gets past every check that needs no secret: the next
# thing the real script asks for is the secret itself.
run "${SCRIPT}" "${COMMITTED_KEY}" ""
expect_refusal "the committed key is the pinned production key" "SPARKLE_ED_PRIVATE_KEY is unset"
[ "$(tr -d '[:space:]' < "${COMMITTED_KEY}")" = "${PINNED}" ] \
  || fail "${COMMITTED_KEY} is not ${PINNED}"
pass "${COMMITTED_KEY#"${REPO_ROOT}/"} holds the pinned literal"

echo "verify-sparkle-key_test.sh: all cases passed"
