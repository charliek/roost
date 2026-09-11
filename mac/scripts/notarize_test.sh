#!/usr/bin/env bash
# Test mac/scripts/notarize.sh's --app and DMG forms against fake ditto/xcrun
# shims, so the command order (submit the zip, staple the bundle) is provable
# on Linux without a real Apple notary run (plan 060 §3.3/C4).
#
# Proxy limits. CI runs this on Linux, but notarize.sh only ever runs on
# macOS, so a BSD-vs-GNU divergence is the one thing a green run here does
# not prove. Two escaped exactly that way and are pinned below: BSD
# `mktemp` substitutes `X`s only when they END the template, so the old
# `…-XXXXXX.zip` reused one fixed name on every real run (the "each run
# gets its own temp zip path" case); and `mapfile` is bash 4 while macOS
# ships bash 3.2 (`read_block_into`). Both were found by running this file
# on the mac-mini, which is the cheap half of closing the gap — it needs no
# Apple round trip, so run it there when either script changes. The other
# half, that Apple accepts the zip and the ticket survives into the DMG,
# needs a real notarization; plan 060's verification did one.
#
# Usage:
#   ./mac/scripts/notarize_test.sh
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
NOTARIZE_SCRIPT="${SCRIPT_DIR}/notarize.sh"

fail() {
  printf 'FAIL: %s\n' "$*" >&2
  exit 1
}

pass() {
  printf 'ok: %s\n' "$*"
}

work_dir="$(mktemp -d)"
trap 'rm -rf "${work_dir}"' EXIT

bin_dir="${work_dir}/bin"
mkdir -p "${bin_dir}"
log="${work_dir}/calls.log"

# The shims log one argument per line inside an @@CALL/@@END block instead
# of `$*`, which joins argv with spaces and would make a word-split path
# (a quoting regression) indistinguishable from one correctly-quoted
# argument containing a space.
cat > "${bin_dir}/ditto" <<'EOF'
#!/usr/bin/env bash
{
  echo "@@CALL ditto"
  printf '%s\n' "$@"
  echo "@@END"
} >> "${NOTARIZE_TEST_LOG}"
if [ "${FAKE_DITTO_EXIT:-0}" != "0" ]; then
  exit "${FAKE_DITTO_EXIT}"
fi
# -c -k --keepParent <src> <dst> — produce a real file at <dst> so the
# script under test can trap-remove it, same as real ditto would.
dst="${@: -1}"
: > "${dst}"
EOF

cat > "${bin_dir}/xcrun" <<'EOF'
#!/usr/bin/env bash
{
  echo "@@CALL xcrun"
  printf '%s\n' "$@"
  echo "@@END"
} >> "${NOTARIZE_TEST_LOG}"
case "$1" in
  notarytool)
    exit "${FAKE_NOTARYTOOL_EXIT:-0}"
    ;;
  stapler)
    case "$2" in
      staple)
        exit "${FAKE_STAPLE_EXIT:-0}"
        ;;
      validate)
        exit "${FAKE_VALIDATE_EXIT:-0}"
        ;;
    esac
    ;;
esac
exit 0
EOF

chmod +x "${bin_dir}/ditto" "${bin_dir}/xcrun"

export NOTARIZE_TEST_LOG="${log}"
export PATH="${bin_dir}:${PATH}"
export ROOST_NOTARY_PROFILE="test"
unset APPLE_ID APPLE_TEAM_ID APPLE_APP_SPECIFIC_PASSWORD 2>/dev/null || true

reset_log() {
  : > "${log}"
  unset FAKE_DITTO_EXIT FAKE_NOTARYTOOL_EXIT FAKE_STAPLE_EXIT FAKE_VALIDATE_EXIT
}

# Rebuild the old single-line "cmd arg1 arg2 …" shape from an @@CALL/@@END
# log, for the assertions below that only care about a call happening with
# some target, not about argument-boundary exactness. None of their
# expected values contain a space, so joining with spaces is lossless for
# them; the space-in-path case parses the raw @@CALL blocks directly instead.
flatten_log() {
  local line="" in_call=0 entry
  while IFS= read -r entry; do
    case "${entry}" in
      '@@CALL '*)
        line="${entry#@@CALL }"
        in_call=1
        ;;
      '@@END')
        printf '%s\n' "${line}"
        line=""
        in_call=0
        ;;
      *)
        line="${line} ${entry}"
        ;;
    esac
  done < "$1"
}

# Print the argument lines (one per line, exact) of the Nth @@CALL block
# (1-based, per command name) — used where a test needs an argument's exact
# boundaries instead of the flattened, space-joined view.
block_lines() {
  local log_file="$1" marker="@@CALL $2" want="${3:-1}"
  awk -v marker="${marker}" -v want="${want}" '
    $0 == marker { c++; if (c == want) { f = 1 }; next }
    /^@@END$/ { if (f) exit }
    f { print }
  ' "${log_file}"
}

# Read `block_lines` output into the named array, one element per line.
# `mapfile`/`readarray` are bash 4; macOS ships bash 3.2, and this suite has
# to run on the platform whose script it gates.
read_block_into() {
  local __name="$1" __log="$2" __cmd="$3" __nth="$4" __line
  eval "${__name}=()"
  while IFS= read -r __line; do
    eval "${__name}+=(\"\${__line}\")"
  done < <(block_lines "${__log}" "${__cmd}" "${__nth}")
}

# ---------------------------------------------------------------------
# Case 1: --app on a throwaway Roost.app.
# ---------------------------------------------------------------------
app1="${work_dir}/Roost.app"
mkdir -p "${app1}/Contents"

reset_log
"${NOTARIZE_SCRIPT}" --app "${app1}"
flat="$(flatten_log "${log}")"

# The zip is a temp file the script itself names; find it from ditto's own
# logged invocation rather than guessing the path.
ditto_line="$(grep '^ditto ' <<< "${flat}" | head -1)"
zip_path="${ditto_line##* }"
case "${zip_path}" in
  *.zip) ;;
  *) fail "Roost.app: ditto's last arg '${zip_path}' does not look like a zip path" ;;
esac

notarytool_line="$(grep '^xcrun notarytool submit ' <<< "${flat}" | head -1)"
case "${notarytool_line}" in
  "xcrun notarytool submit ${zip_path} "*) ;;
  *) fail "Roost.app: notarytool submit target was '${notarytool_line}', expected the zip '${zip_path}'" ;;
esac

staple_line="$(grep '^xcrun stapler staple ' <<< "${flat}" | head -1)"
[ "${staple_line}" = "xcrun stapler staple ${app1}" ] \
  || fail "Roost.app: stapler staple target was '${staple_line}', expected the app '${app1}'"

validate_line="$(grep '^xcrun stapler validate ' <<< "${flat}" | head -1)"
[ "${validate_line}" = "xcrun stapler validate ${app1}" ] \
  || fail "Roost.app: stapler validate target was '${validate_line}', expected the app '${app1}'"

# Order: ditto -> notarytool submit -> stapler staple -> stapler validate.
order="$(grep -E '^(ditto |xcrun (notarytool submit|stapler staple|stapler validate))' <<< "${flat}" \
  | sed -E 's/^(ditto).*/DITTO/; s/^xcrun notarytool submit.*/SUBMIT/; s/^xcrun stapler staple.*/STAPLE/; s/^xcrun stapler validate.*/VALIDATE/')"
expected_order="$(printf 'DITTO\nSUBMIT\nSTAPLE\nVALIDATE')"
[ "${order}" = "${expected_order}" ] \
  || fail "Roost.app: call order was '$(echo "${order}" | tr '\n' ' ')', expected 'DITTO SUBMIT STAPLE VALIDATE'"
pass "--app Roost.app: ditto -> notarytool submit(zip) -> stapler staple(app) -> stapler validate(app), in order"

[ -e "${zip_path}" ] && fail "Roost.app: temp zip '${zip_path}' still exists after a successful run"
pass "--app Roost.app: temp zip removed after success"

# A second run must not reuse the first run's zip path. A template whose X's
# do not END it is substituted by GNU mktemp and taken literally by BSD's, so
# this passes on Linux and names a fixed file on the macOS it actually runs
# on — observed on the mac-mini during plan 060's verification run.
: > "${log}"
"${NOTARIZE_SCRIPT}" --app "${app1}" >/dev/null
flat_again="$(flatten_log "${log}")"
zip_path_again="$(grep '^ditto ' <<< "${flat_again}" | head -1)"
zip_path_again="${zip_path_again##* }"
[ "${zip_path_again}" != "${zip_path}" ] \
  || fail "Roost.app: two runs reused the same temp zip path '${zip_path}' — the name is not unique per run"
pass "--app Roost.app: each run gets its own temp zip path"

# ---------------------------------------------------------------------
# Case 2: --app on a throwaway Roost-Iced.app — same assertions, the other
# bundle name (one real Mac run only exercises one of the two).
# ---------------------------------------------------------------------
app2="${work_dir}/Roost-Iced.app"
mkdir -p "${app2}/Contents"

reset_log
"${NOTARIZE_SCRIPT}" --app "${app2}"
flat2="$(flatten_log "${log}")"

ditto_line2="$(grep '^ditto ' <<< "${flat2}" | head -1)"
zip_path2="${ditto_line2##* }"
case "${zip_path2}" in
  *.zip) ;;
  *) fail "Roost-Iced.app: ditto's last arg '${zip_path2}' does not look like a zip path" ;;
esac

notarytool_line2="$(grep '^xcrun notarytool submit ' <<< "${flat2}" | head -1)"
case "${notarytool_line2}" in
  "xcrun notarytool submit ${zip_path2} "*) ;;
  *) fail "Roost-Iced.app: notarytool submit target was '${notarytool_line2}', expected the zip '${zip_path2}'" ;;
esac

staple_line2="$(grep '^xcrun stapler staple ' <<< "${flat2}" | head -1)"
[ "${staple_line2}" = "xcrun stapler staple ${app2}" ] \
  || fail "Roost-Iced.app: stapler staple target was '${staple_line2}', expected the app '${app2}'"

validate_line2="$(grep '^xcrun stapler validate ' <<< "${flat2}" | head -1)"
[ "${validate_line2}" = "xcrun stapler validate ${app2}" ] \
  || fail "Roost-Iced.app: stapler validate target was '${validate_line2}', expected the app '${app2}'"

order2="$(grep -E '^(ditto |xcrun (notarytool submit|stapler staple|stapler validate))' <<< "${flat2}" \
  | sed -E 's/^(ditto).*/DITTO/; s/^xcrun notarytool submit.*/SUBMIT/; s/^xcrun stapler staple.*/STAPLE/; s/^xcrun stapler validate.*/VALIDATE/')"
[ "${order2}" = "${expected_order}" ] \
  || fail "Roost-Iced.app: call order was '$(echo "${order2}" | tr '\n' ' ')', expected 'DITTO SUBMIT STAPLE VALIDATE'"

[ -e "${zip_path2}" ] && fail "Roost-Iced.app: temp zip '${zip_path2}' still exists after a successful run"
pass "--app Roost-Iced.app: same ditto/submit/staple/validate shape and order, zip target for submit, app target for staple/validate, zip removed"

# ---------------------------------------------------------------------
# Case 3: --app on a bundle whose path contains a space — proves the shims'
# argument logging preserves argv boundaries: a quoting regression that
# word-splits the path would otherwise be invisible, since no other case
# uses a path with a space in it.
# ---------------------------------------------------------------------
space_dir="${work_dir}/with space"
mkdir -p "${space_dir}"
app_space="${space_dir}/Roost-space.app"
mkdir -p "${app_space}/Contents"

reset_log
"${NOTARIZE_SCRIPT}" --app "${app_space}"

read_block_into ditto_args_space "${log}" ditto 1
[ "${#ditto_args_space[@]}" -eq 5 ] \
  || fail "space-in-path: ditto got ${#ditto_args_space[@]} args, expected 5 (-c -k --keepParent <src> <dst>) — the app path likely word-split"
[ "${ditto_args_space[3]:-}" = "${app_space}" ] \
  || fail "space-in-path: ditto's source argument was '${ditto_args_space[3]:-}', expected the whole path '${app_space}'"

read_block_into staple_args_space "${log}" xcrun 2
[ "${#staple_args_space[@]}" -eq 3 ] \
  || fail "space-in-path: stapler staple got ${#staple_args_space[@]} args, expected 3 (stapler staple <target>) — the app path likely word-split"
[ "${staple_args_space[2]:-}" = "${app_space}" ] \
  || fail "space-in-path: stapler staple's target was '${staple_args_space[2]:-}', expected the whole path '${app_space}'"

pass "--app on a path containing a space: ditto's source and stapler's target each arrive as one argument, not split"

# ---------------------------------------------------------------------
# Case 4: a failing stapler staple shim — exits non-zero AND the zip is
# still gone (the trap fired).
# ---------------------------------------------------------------------
app4="${work_dir}/Roost-fail.app"
mkdir -p "${app4}/Contents"

reset_log
export FAKE_STAPLE_EXIT=1
if "${NOTARIZE_SCRIPT}" --app "${app4}" >"${work_dir}/fail.out" 2>&1; then
  fail "failing stapler staple: notarize.sh --app exited 0, expected non-zero"
fi
unset FAKE_STAPLE_EXIT

flat4="$(flatten_log "${log}")"
ditto_line4="$(grep '^ditto ' <<< "${flat4}" | head -1)"
zip_path4="${ditto_line4##* }"
[ -e "${zip_path4}" ] && fail "failing stapler staple: temp zip '${zip_path4}' survived a failed staple (trap did not fire)"
pass "a failing stapler staple exits non-zero and still removes the temp zip (trap fired)"

# ---------------------------------------------------------------------
# Case 5: a failing ditto shim — exits non-zero AND the temp dir it made is
# still gone (the trap fired before any zip was ever created).
# ---------------------------------------------------------------------
app5="${work_dir}/Roost-ditto-fail.app"
mkdir -p "${app5}/Contents"

reset_log
export FAKE_DITTO_EXIT=1
if "${NOTARIZE_SCRIPT}" --app "${app5}" >"${work_dir}/fail-ditto.out" 2>&1; then
  fail "failing ditto: notarize.sh --app exited 0, expected non-zero"
fi
unset FAKE_DITTO_EXIT

flat5="$(flatten_log "${log}")"
ditto_line5="$(grep '^ditto ' <<< "${flat5}" | head -1)"
zip_dir5="$(dirname "${ditto_line5##* }")"
[ -d "${zip_dir5}" ] && fail "failing ditto: temp dir '${zip_dir5}' survived a failed ditto (trap did not fire)"
pass "a failing ditto exits non-zero and still removes the temp dir (trap fired)"

# ---------------------------------------------------------------------
# Case 6: a failing notarytool submit — exits non-zero AND the zip ditto
# already created is gone (the trap fired).
# ---------------------------------------------------------------------
app6="${work_dir}/Roost-notary-fail.app"
mkdir -p "${app6}/Contents"

reset_log
export FAKE_NOTARYTOOL_EXIT=1
if "${NOTARIZE_SCRIPT}" --app "${app6}" >"${work_dir}/fail-notary.out" 2>&1; then
  fail "failing notarytool submit: notarize.sh --app exited 0, expected non-zero"
fi
unset FAKE_NOTARYTOOL_EXIT

flat6="$(flatten_log "${log}")"
ditto_line6="$(grep '^ditto ' <<< "${flat6}" | head -1)"
zip_path6="${ditto_line6##* }"
[ -e "${zip_path6}" ] && fail "failing notarytool submit: temp zip '${zip_path6}' survived a failed submit (trap did not fire)"
pass "a failing notarytool submit exits non-zero and still removes the temp zip (trap fired)"

# ---------------------------------------------------------------------
# Case 7: the DMG form on a throwaway file — no ditto at all, submits +
# staples + validates the DMG path itself.
# ---------------------------------------------------------------------
dmg1="${work_dir}/Roost-0.0.0-test.dmg"
: > "${dmg1}"

reset_log
"${NOTARIZE_SCRIPT}" "${dmg1}"
flat7="$(flatten_log "${log}")"

grep -q '^ditto ' <<< "${flat7}" && fail "DMG form: ditto was called, expected none"
pass "DMG form issues no ditto"

notarytool_line7="$(grep '^xcrun notarytool submit ' <<< "${flat7}" | head -1)"
[ "${notarytool_line7}" = "xcrun notarytool submit ${dmg1} --keychain-profile test --wait" ] \
  || fail "DMG form: notarytool submit line was '${notarytool_line7}'"

staple_line7="$(grep '^xcrun stapler staple ' <<< "${flat7}" | head -1)"
[ "${staple_line7}" = "xcrun stapler staple ${dmg1}" ] \
  || fail "DMG form: stapler staple target was '${staple_line7}', expected the dmg '${dmg1}'"

validate_line7="$(grep '^xcrun stapler validate ' <<< "${flat7}" | head -1)"
[ "${validate_line7}" = "xcrun stapler validate ${dmg1}" ] \
  || fail "DMG form: stapler validate target was '${validate_line7}', expected the dmg '${dmg1}'"
pass "DMG form submits/staples/validates the DMG path itself, with no ditto"

# ---------------------------------------------------------------------
# Case 8: no credentials — both forms exit 0 and issue no xcrun/ditto.
# ---------------------------------------------------------------------
unset ROOST_NOTARY_PROFILE
unset APPLE_ID APPLE_TEAM_ID APPLE_APP_SPECIFIC_PASSWORD 2>/dev/null || true

app8="${work_dir}/Roost-nocreds.app"
mkdir -p "${app8}/Contents"
reset_log
"${NOTARIZE_SCRIPT}" --app "${app8}" || fail "no credentials (--app): expected exit 0"
[ -s "${log}" ] && fail "no credentials (--app): a call was logged: $(cat "${log}")"
pass "no credentials: --app form exits 0 with no ditto/xcrun calls"

dmg8="${work_dir}/Roost-nocreds.dmg"
: > "${dmg8}"
reset_log
"${NOTARIZE_SCRIPT}" "${dmg8}" || fail "no credentials (dmg): expected exit 0"
[ -s "${log}" ] && fail "no credentials (dmg): a call was logged: $(cat "${log}")"
pass "no credentials: DMG form exits 0 with no ditto/xcrun calls"

export ROOST_NOTARY_PROFILE="test"

echo "All notarize.sh tests passed."
