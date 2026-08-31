#!/usr/bin/env bash
# Test linux/scripts/stage-session-artifact.sh against a dummy dist/, so the
# naming + checksum shape it produces for a release is provable without
# cutting one (plan 039 §3.7/C4).
#
# Naming pin: the stable-version case below asserts the exact filename
# crates/roost-ipc/src/bootstrap.rs's `asset_name`/`checksum_name` will
# request (pinned there by its own
# `asset_names_are_versioned_per_arch_and_github_safe` test). This shell
# test has no Rust to import those functions from, so the two are kept from
# drifting by cross-referencing comments rather than a shared definition —
# if one changes, update the other.
#
# Usage:
#   ./linux/scripts/stage-session-artifact_test.sh
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
STAGE_SCRIPT="${SCRIPT_DIR}/stage-session-artifact.sh"

fail() {
  printf 'FAIL: %s\n' "$*" >&2
  exit 1
}

pass() {
  printf 'ok: %s\n' "$*"
}

# Mirror stage-session-artifact.sh's own tool selection so the test can't
# disagree with the script about which tool produced the reference checksum
# (older macOS has no sha256sum on PATH; shasum -a 256 has always been there
# and emits the identical two-field format).
if command -v sha256sum >/dev/null 2>&1; then
  checksum_cmd=(sha256sum)
elif command -v shasum >/dev/null 2>&1; then
  checksum_cmd=(shasum -a 256)
else
  fail "no checksum tool found on PATH — need sha256sum or shasum (shasum -a 256)."
fi

work_dir="$(mktemp -d)"
trap 'rm -rf "${work_dir}"' EXIT

# ---------------------------------------------------------------------
# Case 1: stable version — the shape the Rust client will actually request.
# ---------------------------------------------------------------------
dist1="${work_dir}/stable/dist"
mkdir -p "${dist1}"
printf 'not a real elf, just test bytes\n' > "${dist1}/roost-session"

out="$("${STAGE_SCRIPT}" 0.0.19 amd64 --dist-dir "${dist1}")"
asset_path="$(printf '%s\n' "${out}" | sed -n '1p')"
checksum_path="$(printf '%s\n' "${out}" | sed -n '2p')"

expected_asset_name="roost-session-0.0.19-linux-amd64"
expected_checksum_name="${expected_asset_name}.sha256"

[ "$(basename "${asset_path}")" = "${expected_asset_name}" ] \
  || fail "stable: asset path basename '$(basename "${asset_path}")' != '${expected_asset_name}'"
[ "$(basename "${checksum_path}")" = "${expected_checksum_name}" ] \
  || fail "stable: checksum path basename '$(basename "${checksum_path}")' != '${expected_checksum_name}'"
[ -f "${asset_path}" ] || fail "stable: asset file not produced at ${asset_path}"
[ -f "${checksum_path}" ] || fail "stable: checksum file not produced at ${checksum_path}"
cmp -s "${asset_path}" "${dist1}/roost-session" \
  || fail "stable: staged asset content differs from the source dist/roost-session"
pass "stable version produces roost-session-0.0.19-linux-amd64 + .sha256 (matches the Rust client's asset_name/checksum_name)"

# The .sha256 content must be exactly what `sha256sum` itself would produce
# for that file under that bare name — the two-field format the client's
# parse_checksum_file requires (64 hex, two spaces, bare filename, no
# directory component).
expected_line="$(cd "$(dirname "${asset_path}")" && "${checksum_cmd[@]}" -- "$(basename "${asset_path}")")"
actual_line="$(cat "${checksum_path}")"
[ "${actual_line}" = "${expected_line}" ] \
  || fail "stable: checksum content '${actual_line}' != ${checksum_cmd[0]}'s own output '${expected_line}'"
pass "checksum content matches ${checksum_cmd[0]}'s own two-field output exactly"

hex="${actual_line%%  *}"
name_field="${actual_line#*  }"
[ "${#hex}" -eq 64 ] || fail "stable: hex field is ${#hex} chars, expected 64"
case "${hex}" in
  *[!0-9a-fA-F]*) fail "stable: hex field '${hex}' is not all hex digits" ;;
esac
[ "${name_field}" = "${expected_asset_name}" ] \
  || fail "stable: checksum filename field '${name_field}' is not the bare asset name '${expected_asset_name}' (no directory component)"
pass "checksum filename field is the bare asset name, no directory component"

# ---------------------------------------------------------------------
# Case 2: prerelease-shaped version — release.yml still uploads these
# (named by the tag spelling) even though the client never constructs a
# download URL for one; naming must still be well-formed.
# ---------------------------------------------------------------------
dist2="${work_dir}/prerelease/dist"
mkdir -p "${dist2}"
printf 'not a real elf, just test bytes\n' > "${dist2}/roost-session"

out2="$("${STAGE_SCRIPT}" 0.0.19-rc1 arm64 --dist-dir "${dist2}")"
asset_path2="$(printf '%s\n' "${out2}" | sed -n '1p')"
checksum_path2="$(printf '%s\n' "${out2}" | sed -n '2p')"

expected_asset_name2="roost-session-0.0.19-rc1-linux-arm64"
expected_checksum_name2="${expected_asset_name2}.sha256"

[ "$(basename "${asset_path2}")" = "${expected_asset_name2}" ] \
  || fail "prerelease: asset path basename '$(basename "${asset_path2}")' != '${expected_asset_name2}'"
[ "$(basename "${checksum_path2}")" = "${expected_checksum_name2}" ] \
  || fail "prerelease: checksum path basename '$(basename "${checksum_path2}")' != '${expected_checksum_name2}'"
[ -f "${asset_path2}" ] || fail "prerelease: asset file not produced at ${asset_path2}"
[ -f "${checksum_path2}" ] || fail "prerelease: checksum file not produced at ${checksum_path2}"

expected_line2="$(cd "$(dirname "${asset_path2}")" && "${checksum_cmd[@]}" -- "$(basename "${asset_path2}")")"
actual_line2="$(cat "${checksum_path2}")"
[ "${actual_line2}" = "${expected_line2}" ] \
  || fail "prerelease: checksum content '${actual_line2}' != sha256sum's own output '${expected_line2}'"
pass "prerelease-shaped version produces roost-session-0.0.19-rc1-linux-arm64 + matching .sha256"

# Every character stays inside GitHub's asset-name sanitization set
# ([A-Za-z0-9._-]), for both names produced above — a prerelease's '-rc1'
# is already in that set, so this is a belt-and-suspenders check.
for n in "${expected_asset_name}" "${expected_checksum_name}" \
         "${expected_asset_name2}" "${expected_checksum_name2}"; do
  case "${n}" in
    *[!A-Za-z0-9._-]*) fail "'${n}' contains a character outside GitHub's asset-name-safe set" ;;
  esac
done
pass "all produced names stay inside GitHub's [A-Za-z0-9._-] asset-name set"

# ---------------------------------------------------------------------
# Case 3: missing source binary fails loudly, produces nothing.
# ---------------------------------------------------------------------
dist3="${work_dir}/missing/dist"
mkdir -p "${dist3}"
if "${STAGE_SCRIPT}" 0.0.19 amd64 --dist-dir "${dist3}" >/dev/null 2>"${work_dir}/missing.err"; then
  fail "missing dist/roost-session: script should have failed, but exited 0"
fi
grep -q "roost-session" "${work_dir}/missing.err" \
  || fail "missing dist/roost-session: error message did not mention the missing file"
pass "missing dist/roost-session fails loudly with a clear error"

echo "All stage-session-artifact.sh tests passed."
